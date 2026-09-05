use crate::engine::AgentLoopError;
use crate::engine::RoutedEvent;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::ActorState;
use crate::engine::session::SessionActorConfig;
use crate::engine::session_mode_name;
use crate::engine::turn::emit;
use rw_types::ModeId;
use rw_types::UnrestorablePath;
use tokio::sync::broadcast;

pub(super) async fn rewind_state(
    state: &mut ActorState,
    config: &SessionActorConfig,
    events: &broadcast::Sender<RoutedEvent>,
    to_turn: u64,
) -> Result<Vec<UnrestorablePath>, AgentLoopError> {
    if state.running.is_some() {
        return Err(AgentLoopError::InvalidConfiguration(
            "cannot rewind while a turn is running".to_owned(),
        ));
    }
    if let Some((pending_turn, pending)) = state.pending_rewind.clone() {
        if pending_turn != to_turn {
            return Err(AgentLoopError::InvalidConfiguration(format!(
                "rewind to turn {pending_turn} is awaiting acknowledgement"
            )));
        }
        if let Err(error) = config.checkpoints.acknowledge_rewind(&pending).await {
            state.poisoned = true;
            return Err(error);
        }
        state.pending_rewind = None;
        state.poisoned = false;
        return Ok(pending.unrestorable_paths);
    }
    let history = config.history.capture_history().await?;
    if history.through().map(|sequence| sequence.0) != state.sequence {
        return Err(AgentLoopError::InvalidConfiguration(
            "rewind history does not match the actor's committed prefix".into(),
        ));
    }
    let historical = history.recovery_at_completed_turn(to_turn).await?;
    let budgeter = rw_context::Budgeter::from_snapshot(historical.head.budget)
        .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
    let operation_id = format!(
        "rewind-{}-{to_turn}",
        state
            .sequence
            .map_or(0, |sequence| sequence.saturating_add(1))
    );
    let rewind = config
        .checkpoints
        .prepare_apply_rewind(&config.session_id, to_turn, &operation_id)
        .await?;
    if let Err(error) = emit(
        state,
        events,
        &config.event_sink,
        PendingEvent::ConversationRewound {
            to_turn,
            operation_id,
            unrestorable_paths: rewind.unrestorable_paths.clone(),
        },
    )
    .await
    .map(|_| ())
    {
        state.poisoned = true;
        return Err(error);
    }
    // Transfer bounded controls while retaining their read allowance. Historical
    // conversation and context edits stay in the canonical indexed authority.
    let _applied = historical.map(|historical| {
        state.budgeter = budgeter;
        state.mode = historical.head.control.mode;
        state.mode_id =
            historical.head.control.mode_id.unwrap_or_else(|| {
                ModeId(session_mode_name(historical.head.control.mode).to_owned())
            });
        state.pending_plan = historical.controls.pending_plan;
        state.approved_plan = historical.controls.approved_plan;
        state.plan_gate_active = historical.head.control.plan_gate_active;
        state.completed_turns = historical.head.control.completed_turns;
    });
    state.queued.clear();
    state.queued_positions.clear();
    state.pending_rewind = Some((to_turn, rewind.clone()));
    if let Err(error) = config.checkpoints.acknowledge_rewind(&rewind).await {
        state.poisoned = true;
        return Err(error);
    }
    state.pending_rewind = None;
    Ok(rewind.unrestorable_paths)
}
