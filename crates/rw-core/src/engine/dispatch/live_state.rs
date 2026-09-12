//! Bounded metadata bootstrap without conversation or event replay.
use crate::engine::{AgentLoopError, session::ActorState, wire_turn_id};
use rw_types::{
    ModelAlias, SequenceId,
    allocation::PrepareAllocation,
    session_state::{
        self, SessionActiveTurn, SessionQueuedPreview, SessionShellState, SessionStateSnapshot,
    },
};

pub(in crate::engine) fn snapshot(
    state: &ActorState,
) -> Result<SessionStateSnapshot, AgentLoopError> {
    if state.poisoned || state.closing {
        return Err(AgentLoopError::Closed);
    }
    if state.queued.len() > session_state::MAX_SESSION_QUEUE_ITEMS
        || state.queued.len() != state.queued_positions.len()
    {
        return Err(limit());
    }
    let header = [
        state.session_title.prepared_bytes(),
        state.model_alias.prepared_bytes(),
        state.provider.prepared_bytes(),
        state.mode_id.prepared_bytes(),
        state.live.budget.prepared_bytes(),
        state.live.compaction.prepared_bytes(),
        state.live.plugin_statuses.prepared_bytes(),
    ];
    let mut prepared = 32 * 1024_usize;
    for bytes in header {
        prepared = prepared
            .checked_add(bytes.ok_or_else(limit)?)
            .ok_or_else(limit)?;
    }
    prepared = prepared
        .checked_add(
            state
                .queued
                .len()
                .saturating_add(1)
                .saturating_mul(session_state::MAX_SESSION_QUEUE_PREVIEW_BYTES + 256),
        )
        .ok_or_else(limit)?;
    if prepared > session_state::MAX_SESSION_STATE_PREPARED_BYTES {
        return Err(limit());
    }
    let queued_messages = state
        .queued_positions
        .iter()
        .zip(&state.queued)
        .map(|(position, content)| {
            let (preview, truncated) = preview(content);
            SessionQueuedPreview {
                position: *position,
                preview,
                truncated,
            }
        })
        .collect();
    let shell = state.active_shell.as_ref().map(|shell| {
        let (command_preview, truncated) = preview(&shell.command);
        SessionShellState {
            shell_id: shell.shell_id.clone(),
            command_preview,
            truncated,
        }
    });
    let result = SessionStateSnapshot {
        through: state.sequence.map(SequenceId),
        driver_client_id: state.control.driver().clone(),
        title: state.session_title.clone(),
        model_alias: ModelAlias(state.model_alias.clone()),
        provider: state.provider.clone(),
        thinking: state.thinking,
        mode_id: state.mode_id.clone(),
        active_turn: state.running.as_ref().map(|running| {
            let turn_id = wire_turn_id(running.id);
            let started = state
                .live
                .turn_source
                .as_ref()
                .filter(|(id, _)| id == &turn_id)
                .map(|(_, source)| *source);
            SessionActiveTurn { turn_id, started }
        }),
        completed_turns: state.completed_turns,
        shell,
        compaction: state.live.compaction.clone(),
        queued_messages,
        plugin_statuses: state.live.plugin_statuses.clone(),
        budget: state.live.budget.clone(),
    };
    rw_types::session_controls::encoded_size(&result, session_state::MAX_SESSION_STATE_BYTES)
        .map_err(|_| limit())?;
    Ok(result)
}
fn preview(value: &str) -> (String, bool) {
    let end =
        value.floor_char_boundary(session_state::MAX_SESSION_QUEUE_PREVIEW_BYTES.min(value.len()));
    (value[..end].to_owned(), end < value.len())
}
fn limit() -> AgentLoopError {
    AgentLoopError::InvalidConfiguration("session state exceeds source admission".into())
}
