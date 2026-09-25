//! Durable bounded ownership for controls requested across a turn boundary.
use super::DispatchContext;
use crate::engine::{
    AgentLoopError,
    pending_event::PendingEvent,
    session::{ActorState, ProtocolCompletion, SessionActorConfig},
    turn::emit,
};
use rw_types::{
    ClientCommand, CommandMeta, DeferredSessionAction, QueuedControlStatus, QueuedSessionControl,
    SessionControlOutcome, SessionControlSettlement,
};
use tokio::sync::oneshot;

type Completion = Result<ProtocolCompletion, AgentLoopError>;
pub(in crate::engine) struct ActiveControl {
    receive: Option<oneshot::Receiver<Completion>>,
    result: Option<bool>,
}

pub(super) fn action(command: &ClientCommand) -> Option<DeferredSessionAction> {
    match command {
        ClientCommand::SwitchModel {
            model, provider, ..
        } => Some(DeferredSessionAction::SwitchModel {
            model: model.clone(),
            provider: provider.clone(),
        }),
        ClientCommand::SwitchMode { mode, .. } => {
            Some(DeferredSessionAction::SwitchMode { mode: mode.clone() })
        }
        ClientCommand::Compact { instructions, .. } => Some(DeferredSessionAction::Compact {
            instructions: instructions.clone(),
        }),
        _ => None,
    }
}

pub(super) fn owns_model_selection(
    state: &ActorState,
    model: &rw_types::ModelAlias,
    provider: Option<&str>,
) -> bool {
    state.deferred_active.is_some() && state.deferred_controls.first().is_some_and(|control| {
        control.status == QueuedControlStatus::Running && matches!(&control.action,
            DeferredSessionAction::SwitchModel { model: target, provider: route } if target == model && route.as_deref() == provider)
    })
}

pub(super) fn must_queue(state: &ActorState) -> bool {
    state.running.is_some()
        || state.pending_command.is_some()
        || state.pending_model_preparation.is_some()
        || !state.pending_model_switches.is_empty()
        || !state.deferred_controls.is_empty()
}

pub(super) async fn enqueue(
    state: &mut ActorState,
    config: &SessionActorConfig,
    events: &crate::engine::live_events::LiveEvents,
    request: CommandMeta,
    mut action: DeferredSessionAction,
) -> Result<(), AgentLoopError> {
    if state.closing || state.poisoned || state.active_shell.is_some() {
        return Err(invalid(
            "This session cannot accept queued controls right now.",
        ));
    }
    if let DeferredSessionAction::Compact {
        instructions: Some(instructions),
    } = &mut action
    {
        *instructions = config.secret_redactor.redact(instructions);
    }
    let mut controls = state.deferred_controls.clone();
    controls.push(QueuedSessionControl {
        request,
        action,
        status: QueuedControlStatus::Queued,
    });
    rw_types::validate_queued_controls(&controls).map_err(invalid)?;
    publish(state, config, events, controls, None).await
}

async fn publish(
    state: &mut ActorState,
    config: &SessionActorConfig,
    events: &crate::engine::live_events::LiveEvents,
    controls: Vec<QueuedSessionControl>,
    settlement: Option<SessionControlSettlement>,
) -> Result<(), AgentLoopError> {
    emit(
        state,
        events,
        &config.event_sink,
        PendingEvent::SessionControlQueueChanged {
            controls: controls.clone(),
            settlement,
        },
    )
    .await?;
    state.deferred_controls = controls;
    Ok(())
}

async fn settle(
    state: &mut ActorState,
    config: &SessionActorConfig,
    events: &crate::engine::live_events::LiveEvents,
    index: usize,
    outcome: SessionControlOutcome,
    message: &str,
) -> Result<(), AgentLoopError> {
    let mut controls = state.deferred_controls.clone();
    let control = controls.remove(index);
    let cancelled_question = if outcome != SessionControlOutcome::Applied
        && control.status == QueuedControlStatus::Running
    {
        match &control.action {
            DeferredSessionAction::SwitchModel { model, provider } => state
                .pending_model_switches
                .iter()
                .find(|(_, pending)| pending.model == *model && pending.provider == *provider)
                .map(|(id, _)| rw_types::QuestionId(id.clone())),
            _ => None,
        }
    } else {
        None
    };
    let result = publish(
        state,
        config,
        events,
        controls,
        Some(SessionControlSettlement {
            cancelled_question: cancelled_question.clone(),
            request: control.request,
            action: control.action.kind(),
            outcome,
            message: message.into(),
        }),
    )
    .await;
    if result.is_ok()
        && let Some(question) = cancelled_question
    {
        state.pending_model_switches.remove(&question.0);
    }
    result
}

pub(in crate::engine) async fn wait(active: &mut Option<ActiveControl>) -> Completion {
    if let Some(active) = active
        && let Some(receive) = &mut active.receive
    {
        receive
            .await
            .unwrap_or_else(|_| Err(invalid("Queued control completion was interrupted.")))
    } else {
        std::future::pending().await
    }
}

pub(in crate::engine) fn completed(state: &mut ActorState, result: &Completion) {
    if let Some(active) = &mut state.deferred_active {
        active.receive = None;
        active.result = Some(result.is_ok());
    }
}

/// Returns true after a transition so the actor revisits its idle boundary.
pub(in crate::engine) async fn pump(context: DispatchContext<'_>) -> Result<bool, AgentLoopError> {
    let DispatchContext {
        state,
        config,
        events,
        tool_context,
        turn_signals,
        active_turn,
        command_descriptors,
        mode_registry,
    } = context;
    // Cancel pending requests immediately when their owning lease is lost.
    if let Some(index) = state.deferred_controls.iter().position(|control| {
        control.status == QueuedControlStatus::Queued
            && (state.closing
                || state.control.driver().as_ref() != Some(&control.request.client_id))
    }) {
        settle(
            state,
            config,
            events,
            index,
            SessionControlOutcome::Cancelled,
            "Queued control cancelled because its session control ended.",
        )
        .await?;
        return Ok(true);
    }
    let Some(first) = state.deferred_controls.first().cloned() else {
        return Ok(false);
    };
    if first.status == QueuedControlStatus::Running && state.deferred_active.is_none() {
        // A crash between effect commit and settlement must never execute twice.
        settle(state, config, events, 0, SessionControlOutcome::Cancelled, "Interrupted control was not repeated after recovery. Check the current setting before retrying.").await?;
        return Ok(true);
    }
    if state.deferred_active.is_some() {
        return settle_active(state, config, events, &first).await;
    }
    if state.closing
        || state.poisoned
        || state.recovery_requested
        || state.unsettled.is_some()
        || state.running.is_some()
        || state.active_shell.is_some()
        || state.pending_command.is_some()
        || state.pending_model_preparation.is_some()
        || state.initialization_running
        || state.suspended_inputs.is_some()
        || !state.pending_model_switches.is_empty()
    {
        return Ok(false);
    }
    let mut controls = state.deferred_controls.clone();
    controls[0].status = QueuedControlStatus::Running;
    publish(state, config, events, controls, None).await?;
    let command = command_for(first, &state.session_id)?;
    let (send, receive) = oneshot::channel();
    let (acknowledge, acknowledgement) = oneshot::channel();
    drop(acknowledgement);
    state.deferred_active = Some(ActiveControl {
        receive: Some(receive),
        result: None,
    });
    super::admission::dispatch_protocol(
        command,
        acknowledge,
        Some(send),
        false,
        true,
        None,
        DispatchContext {
            state,
            config,
            tool_context,
            turn_signals,
            events,
            active_turn,
            command_descriptors,
            mode_registry,
        },
    )
    .await;
    Ok(true)
}

fn command_for(
    first: QueuedSessionControl,
    session_id: &rw_types::SessionId,
) -> Result<ClientCommand, AgentLoopError> {
    let mut identity = [0_u8; 16];
    getrandom::fill(&mut identity)
        .map_err(|_| invalid("Queued control identity is unavailable."))?;
    let meta = CommandMeta {
        request_id: rw_types::RequestId(format!(
            "queued-control-{}",
            blake3::hash(&identity).to_hex()
        )),
        ..first.request
    };
    Ok(match first.action {
        DeferredSessionAction::SwitchModel { model, provider } => ClientCommand::SwitchModel {
            meta: meta.clone(),
            session_id: session_id.clone(),
            model,
            provider,
        },
        DeferredSessionAction::SwitchMode { mode } => ClientCommand::SwitchMode {
            meta: meta.clone(),
            session_id: session_id.clone(),
            mode,
        },
        DeferredSessionAction::Compact { instructions } => ClientCommand::Compact {
            meta: meta.clone(),
            session_id: session_id.clone(),
            instructions,
        },
    })
}

async fn settle_active(
    state: &mut ActorState,
    config: &SessionActorConfig,
    events: &crate::engine::live_events::LiveEvents,
    first: &QueuedSessionControl,
) -> Result<bool, AgentLoopError> {
    let Some(active) = &state.deferred_active else {
        return Ok(false);
    };
    let Some(success) = active.result else {
        return Ok(false);
    };
    if state.running.is_some() || state.pending_model_preparation.is_some() {
        return Ok(false);
    }
    let lost = state.closing || state.control.driver().as_ref() != Some(&first.request.client_id);
    if !lost && success && !state.pending_model_switches.is_empty() {
        return Ok(false);
    }
    let applied = success
        && match &first.action {
            DeferredSessionAction::SwitchModel { model, provider } => {
                state.model_alias == model.0 && state.provider == *provider
            }
            DeferredSessionAction::SwitchMode { mode } => state.mode_id == *mode,
            DeferredSessionAction::Compact { .. } => true,
        };
    let preference_failed =
        if applied && !lost && matches!(first.action, DeferredSessionAction::SwitchModel { .. }) {
            if let Some(preferences) = &config.model_preferences {
                preferences.persist(&state.model_alias).await.is_err()
            } else {
                false
            }
        } else {
            false
        };
    let (outcome, message) = if preference_failed {
        (
            SessionControlOutcome::Failed,
            "Model changed, but saving its default failed. Select it again to retry saving.",
        )
    } else if lost {
        (
            SessionControlOutcome::Cancelled,
            "Queued control cancelled because its session control ended.",
        )
    } else if applied {
        (SessionControlOutcome::Applied, "Queued control applied.")
    } else {
        (
            SessionControlOutcome::Failed,
            "Queued control could not be applied. Review the current state and try again.",
        )
    };
    settle(state, config, events, 0, outcome, message).await?;
    state.deferred_active = None;
    Ok(true)
}

fn invalid(message: &str) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(message.into())
}
