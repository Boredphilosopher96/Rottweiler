#![cfg(test)]
#![allow(clippy::expect_used)]
use super::ActorState;
use crate::engine::{SessionRecoveredState, SystemEventClock, session::control::SessionControl};
use rw_types::{Block, Role, SessionId, Turn, TurnMeta, config::ThinkingLevel};
use std::sync::Arc;

#[test]
fn recovery_transfers_conversation_and_payload_allocations_into_actor_state() {
    let session = SessionId("owned-recovery".into());
    let clock = Arc::new(SystemEventClock);
    let text = "message".repeat(100_000);
    let text_pointer = text.as_ptr();
    let conversation = vec![Turn {
        role: Role::User,
        blocks: vec![Block::Text { text }],
        meta: TurnMeta::default(),
    }];
    let turns_pointer = conversation.as_ptr();
    let recovered = SessionRecoveredState {
        conversation,
        ..SessionRecoveredState::default()
    };
    let state = ActorState::recover(
        session.clone(),
        clock.clone(),
        "model",
        ThinkingLevel::Off,
        &rw_ext::ModeRegistry::builtins().expect("modes"),
        recovered,
        Arc::new(SessionControl::new(session, None, clock)),
    );
    assert_eq!(state.conversation.as_ptr(), turns_pointer);
    let Block::Text { text } = &state.conversation[0].blocks[0] else {
        panic!("text");
    };
    assert_eq!(text.as_ptr(), text_pointer);
    assert_eq!(text.len(), 700_000);
}

#[cfg(test)]
mod live_state {
    use crate::engine::dispatch::live_state::snapshot;
    use crate::engine::{
        SessionRecoveredState, SystemEventClock,
        session::{ActorState, SessionControl},
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
