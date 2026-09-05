use crate::engine::AgentLoopError;
use crate::engine::AgentTurnStatus;
use crate::engine::RoutedEvent;
use crate::engine::SessionUsage;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::InterruptedToolRepair;
use crate::engine::projection::SessionRecoveredState;
use crate::engine::projection::project_session_read_view;
use crate::engine::session::config::SessionActorConfig;
use crate::engine::session::state::ActorState;
use crate::engine::turn::emit;
use crate::engine::turn::emit_batch;
use crate::engine::unavailable_cost;
use crate::engine::wire_turn_id;
use rw_types::AccountingAttribution;
use rw_types::ApprovalDecision;
use rw_types::TurnAccounting;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::broadcast;

pub(in crate::engine) fn interrupted_tool_recovery_events(
    repair: &InterruptedToolRepair,
) -> Vec<PendingEvent> {
    let mut events = Vec::with_capacity(2);
    if let Some(start) = &repair.missing_start {
        events.push(PendingEvent::ToolCallStarted {
            turn: repair.agent_turn,
            id: repair.tool_call_id.0.clone(),
            invocation_id: repair.invocation_id.clone(),
            name: start.name.clone(),
            arguments: start.arguments.clone(),
            index: repair.call_index,
        });
    }
    events.push(PendingEvent::ToolCallFinished {
        presentation: None,
        turn: repair.agent_turn,
        id: repair.tool_call_id.0.clone(),
        invocation_id: repair.invocation_id.clone(),
        output: repair.output.clone(),
        is_error: true,
        index: repair.call_index,
    });
    events
}

pub(super) fn interrupted_turn_recovery_events(
    recovered: &SessionRecoveredState,
) -> Vec<PendingEvent> {
    let Some(turn) = recovered.interrupted_turn else {
        return Vec::new();
    };
    let mut events = recovered
        .interrupted_tool_repairs
        .iter()
        .flat_map(interrupted_tool_recovery_events)
        .collect::<Vec<_>>();
    if let Some(tool_turn) = &recovered.interrupted_tool_turn {
        events.push(PendingEvent::ConversationTurnCommitted {
            agent_turn: turn,
            turn: tool_turn.clone(),
        });
    }
    events.push(PendingEvent::TurnFinished {
        turn,
        status: AgentTurnStatus::Interrupted,
        usage: SessionUsage::default(),
        cost: unavailable_cost(),
    });
    events
}

/// Rebuilds all mutable actor state from the authoritative journal after an
/// append error. A sink's default batch implementation may have committed a
/// prefix before returning an error, so retaining any in-memory mutations is
/// unsafe. The interrupted turn is durably closed before the actor accepts
/// more work.
pub(in crate::engine) async fn recover_actor_from_journal(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    events: &broadcast::Sender<RoutedEvent>,
    active_turn: &Arc<AtomicU64>,
) -> Result<(), AgentLoopError> {
    if let Some(running) = &state.running {
        running.cancellation.cancel();
        state.control.finish(running.id);
    }
    active_turn.store(0, Ordering::Release);
    for (_, pending) in std::mem::take(&mut state.pending_approvals) {
        let _ = pending.respond.send(ApprovalDecision::Deny);
    }
    for (_, pending) in std::mem::take(&mut state.pending_questions) {
        let _ = pending.respond.send(Err(rw_tools::ToolError::Cancelled));
    }

    let recovered = project_session_read_view(
        config.event_sink.capture_read_view()?,
        &config.session_id,
        &config.modes,
    )
    .await?;
    let client_roles = std::mem::take(&mut state.client_roles);
    let tasks = state.tasks.clone();
    let control = Arc::clone(&state.control);
    control.commit_driver(recovered.driver_client_id.clone());
    let interrupted_compaction = recovered.interrupted_compaction;
    let interrupted_turn = recovered.interrupted_turn;
    let recovery_events = interrupted_turn_recovery_events(&recovered);
    *state = ActorState::recover(
        config.session_id.clone(),
        Arc::clone(&config.event_clock),
        &config.model_alias,
        config.thinking,
        &config.modes,
        recovered,
        control,
    );
    state.tasks = tasks;
    state.client_roles = client_roles;

    if interrupted_compaction {
        emit(
            state,
            events,
            &config.event_sink,
            PendingEvent::Error {
                message: "interrupted compaction was aborted during recovery".to_owned(),
            },
        )
        .await?;
    }
    if let Some(turn) = interrupted_turn {
        emit_batch(state, events, &config.event_sink, recovery_events).await?;
        state.accounting.record(&TurnAccounting {
            turn_id: wire_turn_id(turn),
            attribution: AccountingAttribution::Main,
            usage: SessionUsage::default().into(),
            cost: unavailable_cost(),
        });
        state.completed_turns = state.completed_turns.saturating_add(1);
    }
    Ok(())
}
