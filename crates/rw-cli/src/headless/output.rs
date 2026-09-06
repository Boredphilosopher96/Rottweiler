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
use std::path::Path;

mod aggregate;
mod printer;
mod repl_encoding;
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
        } = &event
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
        } = &event
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
            OutputFormat::Text => render_text_event(&event, false)?,
            OutputFormat::StreamJson => write_json_line(&public_cli_event(event.clone()))?,
            OutputFormat::Json => {}
        }
        if let EngineEvent::UserMessageAccepted {
            agent_turn,
            content,
            ..
        } = &event
            && content == prompt
        {
            target_turn = Some(agent_turn.to_string());
        }
        let target_finished = if command_mode {
            if waits_for_compaction {
                matches!(&event, EngineEvent::CompactionFinished { .. })
            } else {
                matches!(&event, EngineEvent::CommandFinished { .. })
            }
        } else {
            matches!(
                &event,
                EngineEvent::TurnFinished { turn_id, .. }
                    if Some(&turn_id.0) == target_turn.as_ref()
            )
        };
        if command_mode || target_turn.is_some() {
            aggregate.push(event)?;
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

pub(super) fn public_cli_event(mut event: EngineEvent) -> EngineEvent {
    if let EngineEvent::ThinkingDelta { signature, .. } = &mut event {
        *signature = None;
    }
    event
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
    Interrupt,
    Eof,
    Error(String),
}

#[allow(clippy::too_many_lines)]
pub(super) async fn run_repl(
    actor: &rw_core::SessionHandle,
    storage_root: &Path,
    format: OutputFormat,
) -> Result<Option<TurnStatus>> {
    let mut events = actor.subscribe().map_err(display_agent_error)?;
    let (mut input, mut printer) = printer::spawn_readline(storage_root.join("history.txt"))?;
    let mut interactions = VecDeque::new();
    let mut last_status = None;
    loop {
        tokio::select! {
            maybe = input.recv() => {
                match maybe.unwrap_or(InputLine::Eof) {
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
                            display_next_interaction(interactions.front(), &mut printer).await?;
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
                    InputLine::Interrupt => {
                        if actor.interrupt().await.map_err(display_agent_error)? {
                            interactions.clear();
                        } else {
                            break;
                        }
                    }
                    InputLine::Eof => {
                        let _ = actor.interrupt().await;
                        break;
                    }
                    InputLine::Error(error) => return Err(miette!("readline failed: {error}")),
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
                } = &event {
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
                        display_next_interaction(interactions.front(), &mut printer).await?;
                    }
                }
                if let EngineEvent::QuestionAsked {
                    question_id,
                    question,
                    ..
                } = &event
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
                        display_next_interaction(interactions.front(), &mut printer).await?;
                    }
                }
                if let EngineEvent::PlanSubmitted { .. } = &event {
                    interactions.push_back(PendingInteraction::Plan);
                }
                if let EngineEvent::TurnFinished { status, .. } = &event {
                    last_status = Some(status.clone());
                    interactions.retain(|interaction| matches!(interaction, PendingInteraction::Plan));
                    display_next_interaction(interactions.front(), &mut printer).await?;
                }
                if let Some(message) = repl_event_message(event, format)? {
                    printer.print(message).await?;
                }
            }
        }
    }
    Ok(last_status)
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
    interaction: Option<&PendingInteraction>,
    printer: &mut printer::OwnedPrinter,
) -> Result<()> {
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
        None => return Ok(()),
    };
    printer.print(message).await
}

pub(super) fn repl_event_message(
    event: EngineEvent,
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
