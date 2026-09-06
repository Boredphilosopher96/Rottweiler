//! Print output uses the same finite, wakeable physical owner as the REPL.
use super::{aggregate::PrintOutput, display_agent_error, repl_encoding, terminal::Terminal};
use crate::cli_args::OutputFormat;
use miette::{IntoDiagnostic as _, Result, miette};
use rw_core::{EngineEvent, MessageDisposition, TurnStatus};
use rw_types::{ApprovalBinding, ApprovalDecision};

pub(super) async fn run_print(
    actor: &rw_core::SessionHandle,
    session_id: &str,
    prompt: &str,
    format: OutputFormat,
    perf_markers: bool,
) -> Result<Option<TurnStatus>> {
    let mut printer = Terminal::start_output().await?;
    let execution = async {
        ready(actor, &mut printer, perf_markers).await?;
        consume(
            actor,
            session_id,
            prompt,
            format,
            perf_markers,
            &mut printer,
        )
        .await
    }
    .await;
    printer.close().await?;
    execution
}

pub(super) async fn print_dump(
    actor: &rw_core::SessionHandle,
    dump: &rw_types::PromptDump,
    perf_markers: bool,
) -> Result<Option<TurnStatus>> {
    let mut printer = Terminal::start_output().await?;
    let execution = async {
        ready(actor, &mut printer, perf_markers).await?;
        let message = repl_encoding::pretty(dump, super::MAX_REPL_OUTPUT_BYTES)?;
        write(actor, &mut printer, message, false).await?;
        Ok(None)
    }
    .await;
    printer.close().await?;
    execution
}

async fn ready(
    actor: &rw_core::SessionHandle,
    printer: &mut Terminal,
    perf_markers: bool,
) -> Result<()> {
    if perf_markers {
        write(actor, printer, "rw_perf_prompt_ready=1\n".to_owned(), true).await?;
    }
    Ok(())
}

async fn write(
    actor: &rw_core::SessionHandle,
    printer: &mut Terminal,
    message: String,
    stderr: bool,
) -> Result<()> {
    tokio::select! {
        biased;
        result = printer.print_to(message, stderr) => result,
        signal = tokio::signal::ctrl_c() => {
            signal.into_diagnostic()?;
            // The canceled waiter cannot release the physical request. The
            // caller closes and settles that worker before session shutdown.
            actor.interrupt().await.map_err(display_agent_error)?;
            Err(miette!("print output interrupted"))
        }
    }
}

fn event_message(event: &EngineEvent, format: OutputFormat) -> Result<Option<(String, bool)>> {
    if format == OutputFormat::Json
        || (format == OutputFormat::Text && matches!(event, EngineEvent::ToolOutputDelta { .. }))
    {
        return Ok(None);
    }
    let stderr = format == OutputFormat::Text
        && matches!(
            event,
            EngineEvent::BudgetStatusChanged { .. }
                | EngineEvent::GuardTriggered { .. }
                | EngineEvent::Error { .. }
        );
    Ok(repl_encoding::message(event, format)?.map(|message| (message, stderr)))
}

async fn consume(
    actor: &rw_core::SessionHandle,
    session_id: &str,
    prompt: &str,
    format: OutputFormat,
    perf_markers: bool,
    printer: &mut Terminal,
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
    let mut completion = Completion::new(disposition, prompt);
    let mut aggregate = PrintOutput::new(session_id, format);
    let mut first_event = Some(first_event);
    loop {
        let event = if let Some(event) = first_event.take() {
            event
        } else {
            tokio::select! {
                event = events.recv() => event.map_err(|error| miette!("session event stream failed: {error}"))?,
                signal = tokio::signal::ctrl_c() => {
                    signal.into_diagnostic()?;
                    if !actor.interrupt().await.map_err(display_agent_error)? {
                        return Err(miette!("interrupt received while no turn was running"));
                    }
                    continue;
                }
            }
        };
        answer_noninteractive(actor, event.as_ref()).await?;
        if let Some((message, stderr)) = event_message(event.as_ref(), format)? {
            write(actor, printer, message, stderr).await?;
        }
        let target_finished = completion.observe(event.as_ref(), prompt);
        if completion.command || completion.turn.is_some() {
            aggregate.push(event.as_ref())?;
        }
        if target_finished {
            if perf_markers {
                write(
                    actor,
                    printer,
                    format!(
                        "rw_perf_zero_latency_turn_us={}\n",
                        dispatch_started.elapsed().as_micros()
                    ),
                    true,
                )
                .await?;
            }
            break;
        }
    }
    let (status, message) = aggregate.finish(format)?;
    if let Some(message) = message {
        write(actor, printer, message, false).await?;
    }
    Ok(status)
}

struct Completion {
    command: bool,
    compaction: bool,
    turn: Option<String>,
}
impl Completion {
    fn new(disposition: MessageDisposition, prompt: &str) -> Self {
        Self {
            command: disposition == MessageDisposition::Command,
            compaction: prompt
                .split_whitespace()
                .next()
                .is_some_and(|name| name == "/compact"),
            turn: None,
        }
    }
    fn observe(&mut self, event: &EngineEvent, prompt: &str) -> bool {
        if let EngineEvent::UserMessageAccepted {
            agent_turn,
            content,
            ..
        } = event
            && content == prompt
        {
            self.turn = Some(agent_turn.to_string());
        }
        if self.command {
            if self.compaction {
                matches!(event, EngineEvent::CompactionFinished { .. })
            } else {
                matches!(event, EngineEvent::CommandFinished { .. })
            }
        } else {
            matches!(event, EngineEvent::TurnFinished { turn_id, .. } if Some(&turn_id.0) == self.turn.as_ref())
        }
    }
}

async fn answer_noninteractive(actor: &rw_core::SessionHandle, event: &EngineEvent) -> Result<()> {
    if let EngineEvent::ToolApprovalNeeded {
        tool_call_id,
        invocation_id,
        diff,
        ..
    } = event
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
    } = event
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
    Ok(())
}
