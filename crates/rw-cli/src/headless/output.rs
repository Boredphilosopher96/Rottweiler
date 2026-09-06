use super::display_agent_error;
use crate::cli_args::OutputFormat;
use miette::IntoDiagnostic;
use miette::Result;
use miette::miette;
use rw_core::EngineEvent;
use rw_core::QuestionId;
use rw_core::TurnStatus;
use rw_types::ApprovalBinding;
use rw_types::ApprovalDecision;
use rw_types::ToolCapability;
use std::collections::VecDeque;

mod aggregate;
mod input;
mod print;
mod public_event;
mod repl_encoding;
mod terminal;
pub(super) use print::{print_dump, run_print};
pub(super) const MAX_REPL_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

#[cfg(test)]
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
                    if announce && !display_next_interaction(actor, &mut interrupts, &mut printer, &mut interactions).await? {
                        return Ok(last_status);
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
                    if announce && !display_next_interaction(actor, &mut interrupts, &mut printer, &mut interactions).await? {
                        return Ok(last_status);
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
                if let Some(message) = repl_event_message(event.as_ref(), format)?
                    && !print_ordered(actor, &mut interrupts, &mut printer, &mut interactions, message).await? {
                    return Ok(last_status);
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
