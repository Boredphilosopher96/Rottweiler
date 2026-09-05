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
