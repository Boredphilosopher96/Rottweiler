use super::display_agent_error;
use crate::cli_args::OutputFormat;
use miette::IntoDiagnostic;
use miette::Result;
use miette::miette;
use rw_core::EngineEvent;
use rw_core::MessageDisposition;
use rw_core::QuestionId;
use rw_core::ToolOutputStream;
use rw_core::TurnStatus;
use rw_types::ApprovalBinding;
use rw_types::ApprovalDecision;
use rw_types::ToolCapability;
use serde::Serialize;
use std::collections::VecDeque;
use std::io;
use std::io::Write;

mod aggregate;
mod input;
mod public_event;
mod repl_encoding;
mod terminal;
use aggregate::PrintOutput;
pub(super) const MAX_REPL_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

#[allow(clippy::too_many_lines)]
pub(super) async fn run_print(
    actor: &rw_core::SessionHandle,
    session_id: &str,
    prompt: &str,
    format: OutputFormat,
    perf_markers: bool,
) -> Result<Option<TurnStatus>> {
    let mut events = actor.subscribe().map_err(display_agent_error)?;
    // Complete the initial durable replay before dispatch. Otherwise a fast
    // command result can enter the replay ahead of its connection-scoped ACK.
    events
        .prime()
        .await
        .map_err(|error| miette!("session event stream failed: {error}"))?;
    let dispatch_started = std::time::Instant::now();
    let actor_task = actor.clone();
    let prompt_task = prompt.to_owned();
    let dispatch = tokio::spawn(async move { actor_task.send_message(prompt_task).await });
    let first_event = events
        .recv()
        .await
        .map_err(|error| miette!("session event stream failed: {error}"))?;
    let disposition = dispatch
        .await
        .map_err(|error| miette!("message dispatch worker failed: {error}"))?
        .map_err(display_agent_error)?;
    let command_mode = disposition == MessageDisposition::Command;
    let waits_for_compaction = prompt
        .split_whitespace()
        .next()
        .is_some_and(|name| name == "/compact");
    let mut aggregate = PrintOutput::new(session_id, format);
    let mut target_turn = None;
    let mut first_event = Some(first_event);
    loop {
        let event = if let Some(event) = first_event.take() {
            event
        } else {
            tokio::select! {
                event = events.recv() => event
                    .map_err(|error| miette!("session event stream failed: {error}"))?,
                signal = tokio::signal::ctrl_c() => {
                    signal.into_diagnostic()?;
                    if !actor.interrupt().await.map_err(display_agent_error)? {
                        return Err(miette!("interrupt received while no turn was running"));
                    }
                    continue;
                }
            }
        };
        if let EngineEvent::ToolApprovalNeeded {
            tool_call_id,
            invocation_id,
            diff,
            ..
        } = event.as_ref()
        {
            let binding = diff.as_ref().map(|diff| ApprovalBinding {
                proposal_id: diff.proposal_id.clone(),
                arguments_hash: diff.arguments_hash.clone(),
                base_hash: diff.base_hash.clone(),
                diff_hash: diff.diff_hash.clone(),
            });
            actor
                .approve_bound(
                    tool_call_id.0.clone(),
                    invocation_id.clone(),
                    ApprovalDecision::Deny,
                    binding,
                )
                .await
                .map_err(display_agent_error)?;
        }
        if let EngineEvent::QuestionAsked {
            question_id,
            question,
            ..
        } = event.as_ref()
        {
            let answer = question.options.first().map_or_else(
                || "No interactive answer is available in headless mode.".to_owned(),
                |option| option.value.clone(),
            );
            actor
                .answer_question(question_id.clone(), answer)
                .await
                .map_err(display_agent_error)?;
        }
        match format {
            OutputFormat::Text => render_text_event(event.as_ref(), false)?,
            OutputFormat::StreamJson => write_json_line(public_cli_event(event.as_ref())?.value())?,
            OutputFormat::Json => {}
        }
        if let EngineEvent::UserMessageAccepted {
            agent_turn,
            content,
            ..
        } = event.as_ref()
            && content == prompt
        {
            target_turn = Some(agent_turn.to_string());
        }
        let target_finished = if command_mode {
            if waits_for_compaction {
                matches!(event.as_ref(), EngineEvent::CompactionFinished { .. })
            } else {
                matches!(event.as_ref(), EngineEvent::CommandFinished { .. })
            }
        } else {
            matches!(
                event.as_ref(),
                EngineEvent::TurnFinished { turn_id, .. }
                    if Some(&turn_id.0) == target_turn.as_ref()
            )
        };
        if command_mode || target_turn.is_some() {
            aggregate.push(event.as_ref())?;
        }
        if target_finished {
            if perf_markers {
                eprintln!(
                    "rw_perf_zero_latency_turn_us={}",
                    dispatch_started.elapsed().as_micros()
                );
            }
            break;
        }
    }
    aggregate.finish(format)
}

pub(super) fn public_cli_event(
    event: &EngineEvent,
) -> Result<rw_types::allocation::PreparedAllocation<EngineEvent>> {
    let plan = public_event::PublicEventPlan::new(event)
        .ok_or_else(|| miette!("public event allocation admission exceeded"))?;
    if plan
        .bytes()
        .checked_mul(2)
        .is_none_or(|bytes| bytes > MAX_REPL_OUTPUT_BYTES)
    {
        return Err(miette!("public event allocation admission exceeded"));
    }
    plan.prepare()
        .ok_or_else(|| miette!("public event allocation admission exceeded"))
}

pub(super) fn write_json_line(value: &impl Serialize) -> Result<()> {
    let mut stdout = io::stdout().lock();
    rw_types::json_encoding::JsonWriter::stream(&mut stdout, usize::MAX)
        .serialize(value)
        .into_diagnostic()?;
    stdout.write_all(b"\n").into_diagnostic()?;
    stdout.flush().into_diagnostic()
}

pub(super) fn render_text_event(event: &EngineEvent, repl: bool) -> Result<()> {
    match event {
        EngineEvent::TextDelta { text, .. } => {
            print!("{text}");
            io::stdout().flush().into_diagnostic()?;
        }
        EngineEvent::ToolOutputDelta { stream, chunk, .. } if repl => {
            if *stream == ToolOutputStream::Stderr {
                eprint!("{chunk}");
                io::stderr().flush().into_diagnostic()?;
            } else {
                print!("{chunk}");
                io::stdout().flush().into_diagnostic()?;
            }
        }
        EngineEvent::ContextSnapshotReady { snapshot, .. } => {
            println!(
                "{}",
                serde_json::to_string_pretty(snapshot).into_diagnostic()?
            );
        }
        EngineEvent::CostSnapshotReady { snapshot, .. } => {
            println!(
                "{}",
                serde_json::to_string_pretty(snapshot).into_diagnostic()?
            );
        }
        EngineEvent::PromptDumpReady { dump, .. } => {
            println!("{}", serde_json::to_string_pretty(dump).into_diagnostic()?);
        }
        EngineEvent::ContextItemPinned { item_id, .. } => {
            println!("pinned context item {}", item_id.0);
        }
        EngineEvent::ContextItemEvicted { item_id, .. } => {
            println!("evicted context item {}", item_id.0);
        }
        EngineEvent::CompactionStarted { reason, .. } => {
            println!("compaction started ({reason:?})");
        }
        EngineEvent::CompactionAttemptFinished { cost, .. } => {
            println!("compaction attempt accounted ({cost:?})");
        }
        EngineEvent::CompactionFinished {
            reclaimed_tokens, ..
        } => {
            println!("compaction finished; reclaimed {reclaimed_tokens} estimated tokens");
        }
        EngineEvent::BudgetStatusChanged {
            level,
            scope,
            current,
            limit,
            ..
        } => {
            eprintln!("budget {level:?} ({scope:?}): {current}/{limit}");
        }
        EngineEvent::CommandFinished { message, .. } => println!("{message}"),
        EngineEvent::GuardTriggered { message, .. } => {
            eprintln!("error: {message}");
        }
        EngineEvent::Error { error, .. } => eprintln!("error: {}", error.message),
        _ => {}
    }
    Ok(())
}

pub(super) enum InputLine {
    Line(String),
    Eof,
    Error(String),
}

#[allow(clippy::too_many_lines)]
pub(super) async fn run_repl(
    actor: &rw_core::SessionHandle,
    format: OutputFormat,
) -> Result<Option<TurnStatus>> {
    let mut events = actor.subscribe().map_err(display_agent_error)?;
    let (mut input, mut interrupts, mut printer) = terminal::Terminal::start().await?;
    let execution = async {
    let mut interactions = VecDeque::new();
    let mut last_status = None;
    loop {
        tokio::select! {
            signal = interrupts.recv() => {
                signal.into_diagnostic()?;
                if actor.interrupt().await.map_err(display_agent_error)? { interactions.clear(); } else { break; }
            },
            maybe = input.recv() => {
                let input::InputDelivery { value, bytes: _input_bytes } = maybe.unwrap_or(input::InputDelivery { value: InputLine::Eof, bytes: None });
                match value {
                    InputLine::Line(line) => {
                        if let Some(interaction) = interactions.pop_front() {
                            match interaction {
                                PendingInteraction::Plan => {
                                    let (decision, revisions) = if line.trim().eq_ignore_ascii_case("approve")
                                        || line.trim().eq_ignore_ascii_case("y")
                                    {
                                        (rw_core::PlanDecision::Approve, None)
                                    } else {
                                        (rw_core::PlanDecision::Reject, Some(line))
                                    };
                                    let _ = actor
                                        .review_plan(decision, revisions)
                                        .await
                                        .map_err(display_agent_error)?;
                                }
                                PendingInteraction::Question { id, prompt, options } => {
                                    if !actor.answer_question(id.clone(), line).await.map_err(display_agent_error)? {
                                        interactions.push_front(PendingInteraction::Question { id, prompt, options });
                                    }
                                }
                                PendingInteraction::Permission { tool_call_id, invocation_id, binding, .. } => {
                                    let decision = parse_approval(&line);
                                    let _ = actor
                                        .approve_bound(tool_call_id, invocation_id, decision, binding)
                                        .await
                                        .map_err(display_agent_error)?;
                                }
                            }
                            if !display_next_interaction(actor, &mut interrupts, &mut printer, &mut interactions).await? { return Ok(last_status); }
                            continue;
                        }
                        if line.trim() == "/exit" {
                            let _ = actor.interrupt().await;
                            break;
                        }
                        if line.trim().is_empty() {
                            continue;
                        }
                        actor.send_message(line).await.map_err(display_agent_error)?;
                    }
                    InputLine::Eof => {
                        let _ = actor.interrupt().await;
                        break;
                    }
                    InputLine::Error(error) => return Err(miette!("REPL input failed: {error}")),
                }
            }
            event = events.recv() => {
                let event = event.map_err(|error| miette!("session event stream failed: {error}"))?;
                if let EngineEvent::ToolApprovalNeeded {
                    tool_call_id,
                    invocation_id,
                    capabilities,
                    rationale,
                    diff,
                    ..
                } = event.as_ref() {
                    let announce = interactions.is_empty();
                    interactions.push_back(PendingInteraction::Permission {
                        tool_call_id: tool_call_id.0.clone(),
                        invocation_id: invocation_id.clone(),
                        capabilities: capabilities.clone(),
                        rationale: rationale.clone(),
                        binding: diff.as_ref().map(|diff| ApprovalBinding {
                            proposal_id: diff.proposal_id.clone(),
                            arguments_hash: diff.arguments_hash.clone(),
                            base_hash: diff.base_hash.clone(),
                            diff_hash: diff.diff_hash.clone(),
                        }),
                    });
                    if announce {
                        if !display_next_interaction(actor, &mut interrupts, &mut printer, &mut interactions).await? { return Ok(last_status); }
                    }
                }
                if let EngineEvent::QuestionAsked {
                    question_id,
                    question,
                    ..
                } = event.as_ref()
                {
                    let announce = interactions.is_empty();
                    interactions.push_back(PendingInteraction::Question {
                        id: question_id.clone(),
                        prompt: question.prompt.clone(),
                        options: question
                            .options
                            .iter()
                            .map(|option| option.label.clone())
                            .collect(),
                    });
                    if announce {
                        if !display_next_interaction(actor, &mut interrupts, &mut printer, &mut interactions).await? { return Ok(last_status); }
                    }
                }
                if let EngineEvent::PlanSubmitted { .. } = event.as_ref() {
                    interactions.push_back(PendingInteraction::Plan);
                }
                if let EngineEvent::TurnFinished { status, .. } = event.as_ref() {
                    last_status = Some(status.clone());
                    interactions.retain(|interaction| matches!(interaction, PendingInteraction::Plan));
                    if !display_next_interaction(actor, &mut interrupts, &mut printer, &mut interactions).await? { return Ok(last_status); }
                }
                if let Some(message) = repl_event_message(event.as_ref(), format)? {
                    if !print_ordered(actor, &mut interrupts, &mut printer, &mut interactions, message).await? { return Ok(last_status); }
                }
            }
        }
    }
    Ok(last_status)
    }.await;
    printer.close().await?;
    execution
}

pub(super) enum PendingInteraction {
    Plan,
    Question {
        id: QuestionId,
        prompt: String,
        options: Vec<String>,
    },
    Permission {
        tool_call_id: String,
        invocation_id: rw_types::ToolInvocationId,
        capabilities: Vec<ToolCapability>,
        rationale: String,
        binding: Option<ApprovalBinding>,
    },
}

async fn display_next_interaction(
    actor: &rw_core::SessionHandle,
    interrupts: &mut terminal::Interrupts,
    printer: &mut terminal::Terminal,
    interactions: &mut VecDeque<PendingInteraction>,
) -> Result<bool> {
    let interaction = interactions.front();
    let message = match interaction {
        Some(PendingInteraction::Plan) => {
            "plan submitted: type `approve` to enter Execute, or rejection feedback to stay in Plan\n".to_owned()
        }
        Some(PendingInteraction::Question {
            prompt, options, ..
        }) => {
            if options.is_empty() {
                format!("question: {prompt}\n")
            } else {
                format!("question: {prompt}\noptions: {}\n", options.join(" | "))
            }
        }
        Some(PendingInteraction::Permission {
            capabilities,
            rationale,
            ..
        }) => format!("allow {capabilities:?} ({rationale})? [y] once / [a] session / [p] project / [n] deny\n"),
        None => return Ok(true),
    };
    print_ordered(actor, interrupts, printer, interactions, message).await
}

pub(super) fn repl_event_message(
    event: &EngineEvent,
    format: OutputFormat,
) -> Result<Option<String>> {
    repl_encoding::message(event, format)
}

pub(super) fn parse_approval(input: &str) -> ApprovalDecision {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "once" => ApprovalDecision::AllowOnce,
        "a" | "all" | "session" => ApprovalDecision::AllowSession,
        "p" | "project" => ApprovalDecision::AllowProject,
        _ => ApprovalDecision::Deny,
    }
}

#[cfg(test)]
mod tests;

async fn print_ordered(
    actor: &rw_core::SessionHandle,
    interrupts: &mut terminal::Interrupts,
    printer: &mut terminal::Terminal,
    interactions: &mut VecDeque<PendingInteraction>,
    message: String,
) -> Result<bool> {
    let printing = printer.print(message);
    tokio::pin!(printing);
    loop {
        tokio::select! {
            result = &mut printing => { result?; return Ok(true); },
            signal = interrupts.recv() => {
                signal.into_diagnostic()?;
                if actor.interrupt().await.map_err(display_agent_error)? { interactions.clear(); }
                else { return Ok(false); }
            },
        }
    }
}
