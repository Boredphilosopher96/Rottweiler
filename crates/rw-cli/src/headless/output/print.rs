//! Print output uses the same finite, wakeable physical owner as the REPL.
use super::{aggregate::PrintOutput, display_agent_error, repl_encoding, terminal::Terminal};
use crate::cli_args::OutputFormat;
use miette::{IntoDiagnostic as _, Result, miette};
use rw_core::{EngineEvent, MessageDisposition, TurnStatus};
use rw_types::{ApprovalBinding, ApprovalDecision};

/// Register once before any print work. A fresh `ctrl_c` future drops signals
/// delivered between output/event waits after Tokio has installed its handler.
struct PrintInterrupts(tokio::signal::unix::Signal);
impl PrintInterrupts {
    fn new() -> Result<Self> {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .map(Self)
            .into_diagnostic()
    }
    async fn recv(&mut self) -> Result<()> {
        self.0
            .recv()
            .await
            .ok_or_else(|| miette!("print interrupt stream closed"))
    }
}

pub(in crate::headless) async fn run_print(
    actor: &rw_core::SessionHandle,
    session_id: &str,
    prompt: &str,
    format: OutputFormat,
    perf_markers: bool,
) -> Result<Option<TurnStatus>> {
    let mut interrupts = PrintInterrupts::new()?;
    let mut printer = Terminal::start_output().await?;
    let execution = async {
        ready(actor, &mut printer, &mut interrupts, perf_markers).await?;
        consume(
            actor,
            session_id,
            prompt,
            format,
            perf_markers,
            &mut printer,
            &mut interrupts,
        )
        .await
    }
    .await;
    printer.close().await?;
    execution
}

pub(in crate::headless) async fn print_dump(
    actor: &rw_core::SessionHandle,
    dump: &rw_types::PromptDump,
    perf_markers: bool,
) -> Result<Option<TurnStatus>> {
    let mut interrupts = PrintInterrupts::new()?;
    let mut printer = Terminal::start_output().await?;
    let execution = async {
        ready(actor, &mut printer, &mut interrupts, perf_markers).await?;
        let message = repl_encoding::pretty(dump, super::MAX_REPL_OUTPUT_BYTES)?;
        write(actor, &mut printer, &mut interrupts, message, false).await?;
        Ok(None)
    }
    .await;
    printer.close().await?;
    execution
}

async fn ready(
    actor: &rw_core::SessionHandle,
    printer: &mut Terminal,
    interrupts: &mut PrintInterrupts,
    perf_markers: bool,
) -> Result<()> {
    if perf_markers {
        write(
            actor,
            printer,
            interrupts,
            "rw_perf_prompt_ready=1\n".to_owned(),
            true,
        )
        .await?;
    }
    Ok(())
}

async fn write(
    actor: &rw_core::SessionHandle,
    printer: &mut Terminal,
    interrupts: &mut PrintInterrupts,
    message: String,
    stderr: bool,
) -> Result<()> {
    tokio::select! {
        biased;
        signal = interrupts.recv() => {
            signal?;
            // Wake the physical owner before awaiting actor admission. Its
            // request/slot remain owned until the worker actually settles.
            printer.cancel();
            actor.interrupt().await.map_err(display_agent_error)?;
            Err(miette!("print output interrupted"))
        },
        result = printer.print_to(message, stderr) => result,
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
    interrupts: &mut PrintInterrupts,
) -> Result<Option<TurnStatus>> {
    let mut events = actor.subscribe().map_err(display_agent_error)?;
    let dispatch_started = std::time::Instant::now();
    let (disposition, first_event) = startup(actor, &mut events, interrupts, prompt).await?;
    let mut completion = Completion::new(disposition, prompt);
    let mut aggregate = PrintOutput::new(session_id, format);
    let mut first_event = Some(first_event);
    loop {
        let event = if let Some(event) = first_event.take() {
            event
        } else {
            tokio::select! {
                event = events.recv() => event.map_err(|error| miette!("session event stream failed: {error}"))?,
                signal = interrupts.recv() => {
                    signal?;
                    if !actor.interrupt().await.map_err(display_agent_error)? {
                        return Err(miette!("interrupt received while no turn was running"));
                    }
                    continue;
                }
            }
        };
        answer_noninteractive(actor, event.as_ref()).await?;
        if let Some((message, stderr)) = event_message(event.as_ref(), format)? {
            write(actor, printer, interrupts, message, stderr).await?;
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
                    interrupts,
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
        write(actor, printer, interrupts, message, false).await?;
    }
    Ok(status)
}

async fn startup(
    actor: &rw_core::SessionHandle,
    events: &mut rw_core::SessionSubscription,
    interrupts: &mut PrintInterrupts,
    prompt: &str,
) -> Result<(MessageDisposition, rw_core::SessionEventDelivery)> {
    // Validate the captured source before admitting the command. The subscription
    // owns any entered read even when this client stops waiting for it.
    tokio::select! {
        result = events.prime() => result.map_err(display_agent_error)?,
        signal = interrupts.recv() => {
            signal?;
            return Err(miette!("print startup interrupted"));
        }
    }
    tokio::select! {
        result = async {
            // Poll both without spawning a detached dispatcher. Once admitted,
            // request and effects belong to the actor, independently of these
            // response waiters; the enclosing session always settles on exit.
            tokio::try_join!(
                async { actor.send_message(prompt.to_owned()).await.map_err(display_agent_error) },
                async { events.recv().await.map_err(display_agent_error) },
            )
        } => result,
        signal = interrupts.recv() => {
            signal?;
            actor.interrupt().await.map_err(display_agent_error)?;
            Err(miette!("print startup interrupted"))
        }
    }
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

#[cfg(test)]
mod tests;
