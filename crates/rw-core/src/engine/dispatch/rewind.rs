use crate::engine::AgentLoopError;
use crate::engine::RoutedEvent;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::project_journal_prefix;
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
    let boundary = config
        .event_sink
        .completed_turn(to_turn)
        .await?
        .ok_or_else(|| {
            AgentLoopError::InvalidConfiguration(format!(
                "turn {to_turn} is not a completed rewind target"
            ))
        })?;
    let view = config.event_sink.capture_read_view()?;
    let historical = project_journal_prefix(
        view,
        &config.session_id,
        &config.modes,
        Some(boundary.sequence_id),
    )
    .await?;
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
    state.conversation = historical.conversation;
    state.context_surgery = historical.context_surgery;
    state.pruned_tool_outputs = historical.pruned_tool_outputs;
    state.budgeter = historical.budgeter;
    state.mode = historical.mode;
    state.mode_id = historical
        .mode_id
        .unwrap_or_else(|| ModeId(session_mode_name(historical.mode).to_owned()));
    state.pending_plan = historical.pending_plan;
    state.approved_plan = historical.approved_plan;
    state.plan_gate_active = historical.plan_gate_active;
    state.completed_turns = boundary.completed_turns;
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
