//! Bounded metadata bootstrap without conversation or event replay.
use crate::engine::{AgentLoopError, session::ActorState, wire_turn_id};
use rw_types::{
    ModelAlias, SequenceId,
    allocation::PrepareAllocation,
    session_state::{
        self, SessionActiveTurn, SessionQueuedPreview, SessionShellState, SessionStateSnapshot,
    },
};

pub(super) fn snapshot(state: &ActorState) -> Result<SessionStateSnapshot, AgentLoopError> {
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

#[cfg(test)]
mod tests {
    use super::snapshot;
    use crate::engine::{
        SessionRecoveredState, SystemEventClock,
        session::{ActorState, control::SessionControl},
    };
    use rw_types::{SessionId, config::ThinkingLevel, session_state::MAX_SESSION_QUEUE_ITEMS};
    use std::sync::Arc;

    #[test]
    fn queued_previews_preserve_positions_and_utf8_with_bounded_payload() {
        let session = SessionId("live-state".into());
        let clock = Arc::new(SystemEventClock);
        let mut state = ActorState::recover(
            session.clone(),
            clock.clone(),
            "model",
            ThinkingLevel::Off,
            &rw_ext::ModeRegistry::builtins().expect("modes"),
            SessionRecoveredState::default(),
            Arc::new(SessionControl::new(session, None, clock)),
        );
        state.queued.push_back("a".repeat(1023) + "🙂end");
        state.queued_positions.push_back(u64::MAX);
        let result = snapshot(&state).expect("bounded snapshot");
        assert_eq!(result.queued_messages[0].position, u64::MAX);
        assert_eq!(result.queued_messages[0].preview.len(), 1023);
        assert!(result.queued_messages[0].truncated);
        for position in 1..MAX_SESSION_QUEUE_ITEMS {
            state.queued.push_back("short".into());
            state.queued_positions.push_back(position as u64);
        }
        assert_eq!(
            snapshot(&state)
                .expect("full admitted queue")
                .queued_messages
                .len(),
            MAX_SESSION_QUEUE_ITEMS
        );
        state.queued.push_back("extra".into());
        state.queued_positions.push_back(1);
        assert!(snapshot(&state).is_err());
    }
}
