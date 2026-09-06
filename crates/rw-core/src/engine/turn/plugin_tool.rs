//! A command's host tool call owns a real tool-only turn and the shared scheduler.
use super::{
    RunningTurn, StartTurnRuntime, TurnSignal, emit,
    start::{CommandTurnOverrides, prepare_turn_start},
    tool_admission::AdmittedToolBatch,
    tool_requests::{ActorQuestionAsker, ChannelApprover, PendingToolCall, ToolExecution},
    tool_scheduling::execute_tool_calls,
};
use crate::engine::{
    AgentLoopError, AgentTurnStatus, SessionUsage,
    pending_event::PendingEvent,
    session::{ActorState, SessionActorConfig},
    wire_turn_id,
};
use rw_tools::CancellationToken;
use rw_types::extension_tools::{
    ExtensionToolCall, ExtensionToolOutcome, MAX_EXTENSION_TOOL_OUTPUT_BYTES, within_json_limit,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::oneshot;

pub(in crate::engine) struct PendingPluginTool {
    turn: u64,
    respond: oneshot::Sender<Result<ExtensionToolOutcome, AgentLoopError>>,
}

pub(in crate::engine) async fn start(
    request: ExtensionToolCall,
    respond: oneshot::Sender<Result<ExtensionToolOutcome, AgentLoopError>>,
    state: &mut ActorState,
    runtime: StartTurnRuntime<'_>,
) {
    let prepared = prepare(&request, state, runtime.config);
    let (config, calls) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = respond.send(Err(error));
            return;
        }
    };
    let turn = state.next_turn;
    let Some(next_turn) = turn.checked_add(1) else {
        let _ = respond.send(Err(invalid("turn identity exhausted")));
        return;
    };
    if let Err(error) = emit(
        state,
        runtime.events,
        &config.event_sink,
        PendingEvent::TurnStarted { turn },
    )
    .await
    {
        let _ = respond.send(Err(error));
        return;
    }
    state.next_turn = next_turn;
    let cancellation = CancellationToken::default();
    state.running = Some(RunningTurn {
        id: turn,
        cancellation: cancellation.clone(),
        caused_by: state.transient_cause.clone(),
    });
    state.control.start(turn, cancellation.clone());
    runtime.active_turn.store(turn, Ordering::Release);
    state.pending_plugin_tool = Some(PendingPluginTool { turn, respond });
    let signals = runtime.signals.clone();
    let context = bind_context(turn, runtime, &cancellation, &config.model_alias);
    let tasks = state.tasks.clone();
    let owned_tasks = tasks.clone();
    let mode = state.mode;
    let operation = async move {
        let approver = ChannelApprover {
            signals: signals.clone(),
            cancellation: cancellation.clone(),
        };
        let mut results = execute_tool_calls(
            turn,
            &owned_tasks,
            calls,
            &config,
            &context,
            &cancellation,
            &approver,
            &signals,
            mode,
        )
        .await;
        let result = results.pop().ok_or_else(|| {
            AgentLoopError::EffectsUnsettled("host tool scheduler returned no completion".into())
        });
        // The same FIFO carries ToolCallStarted/output/Finished first. The actor
        // commits them before exposing this completion to the plugin callback.
        let _ = signals.send(TurnSignal::PluginToolComplete { turn, result });
    };
    match tasks.spawn(
        Arc::clone(runtime.config),
        state
            .running
            .as_ref()
            .map_or_else(CancellationToken::default, |running| {
                running.cancellation.clone()
            }),
        operation,
    ) {
        Ok(task) => drop(task),
        Err(error) => {
            let _ = runtime.signals.send(TurnSignal::PluginToolComplete {
                turn,
                result: Err(error),
            });
        }
    }
}

fn bind_context(
    turn: u64,
    runtime: StartTurnRuntime<'_>,
    cancellation: &CancellationToken,
    model_alias: &str,
) -> rw_tools::ToolContext {
    let signals = runtime.signals.clone();
    runtime
        .tool_context
        .clone()
        .with_cancellation(cancellation.clone())
        .with_question_asker(Arc::new(ActorQuestionAsker::new(
            signals.clone(),
            cancellation.clone(),
        )))
        .with_model_alias(model_alias.to_owned())
        .with_todo_store(Arc::new(super::todos::ActorTodoStore::new(
            turn,
            signals.clone(),
        )))
}

fn prepare(
    request: &ExtensionToolCall,
    state: &ActorState,
    config: &Arc<SessionActorConfig>,
) -> Result<(Arc<SessionActorConfig>, AdmittedToolBatch), AgentLoopError> {
    request.validate().map_err(invalid)?;
    if state.running.is_some()
        || state.pending_plugin_tool.is_some()
        || state.pending_model_preparation.is_some()
        || state.active_shell.is_some()
        || state.initialization_running
        || state.closing
        || state.poisoned
        || state.unsettled.is_some()
        || !state.pending_model_switches.is_empty()
    {
        return Err(invalid("host tool invocation is busy"));
    }
    let command = state
        .pending_command
        .as_ref()
        .filter(|command| command.allows(&request.origin, config, state.control.driver().as_ref()))
        .ok_or_else(|| invalid("host tool origin is not the active command"))?;
    let allowed = command.host_tools();
    if !allowed.iter().any(|name| name == &request.name) {
        return Err(invalid(
            "host tool is outside the command's declared authority",
        ));
    }
    let prepared = prepare_turn_start(
        state,
        config,
        Vec::new(),
        CommandTurnOverrides {
            allowed_tools: Some(allowed.to_vec()),
            ..CommandTurnOverrides::default()
        },
    )?;
    let turn = state.next_turn;
    let calls = AdmittedToolBatch::new(
        vec![PendingToolCall {
            id: format!("extension-{turn}"),
            invocation_id: rw_types::ToolInvocationId(format!("turn-{turn}:extension-0")),
            name: request.name.clone(),
            arguments: Some(request.input.clone()),
            index: 0,
        }],
        config.secret_redactor.as_ref(),
    )
    .map_err(invalid)?;
    Ok((prepared.config, calls))
}

pub(in crate::engine) async fn finish(
    turn: u64,
    result: Result<ToolExecution, AgentLoopError>,
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    events: &crate::engine::live_events::LiveEvents,
    active_turn: &Arc<AtomicU64>,
) -> Result<(), AgentLoopError> {
    if state
        .pending_plugin_tool
        .as_ref()
        .map(|pending| pending.turn)
        != Some(turn)
        || state.running.as_ref().map(|running| running.id) != Some(turn)
    {
        return Err(AgentLoopError::EffectsUnsettled(
            "host tool completion lost its active owner".into(),
        ));
    }
    let pending = state
        .pending_plugin_tool
        .take()
        .ok_or_else(|| invalid("host tool owner unavailable"))?;
    let cancelled = state
        .running
        .as_ref()
        .is_some_and(|running| running.cancellation.is_cancelled());
    state.control.finish(turn);
    state.running = None;
    active_turn.store(0, Ordering::Release);
    state.pending_approvals.clear();
    for (_, question) in std::mem::take(&mut state.pending_questions) {
        let _ = question.respond.send(Err(rw_tools::ToolError::Cancelled));
    }
    let execution = match result {
        Ok(execution) if !execution.unsettled && state.unsettled.is_none() => execution,
        _ => {
            state
                .unsettled
                .get_or_insert_with(|| "host tool effects did not settle".into());
            state.tasks.cancel();
            let _ = pending.respond.send(Err(AgentLoopError::EffectsUnsettled(
                "host tool effects did not settle".into(),
            )));
            return Ok(());
        }
    };
    let status = if cancelled {
        AgentTurnStatus::Interrupted
    } else if execution.is_error {
        AgentTurnStatus::Failed
    } else {
        AgentTurnStatus::Completed
    };
    if let Err(error) = emit(
        state,
        events,
        &config.event_sink,
        PendingEvent::TurnFinished {
            turn,
            status,
            usage: SessionUsage::default(),
            cost: rw_types::Cost::Unavailable {
                reason: "no provider invocation".into(),
            },
        },
    )
    .await
    {
        let _ = pending.respond.send(Err(error.clone()));
        return Err(error);
    }
    state.completed_turns = state.completed_turns.saturating_add(1);
    let output = within_json_limit(&execution.output, MAX_EXTENSION_TOOL_OUTPUT_BYTES)
        .then_some(execution.output);
    let _ = pending.respond.send(Ok(ExtensionToolOutcome {
        turn_id: wire_turn_id(turn),
        invocation_id: execution.call.invocation_id,
        is_error: execution.is_error,
        output,
    }));
    Ok(())
}
fn invalid(message: impl Into<String>) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(message.into())
}
