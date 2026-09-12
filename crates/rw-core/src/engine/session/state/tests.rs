#![cfg(test)]
#![allow(clippy::expect_used)]
use super::ActorState;
use crate::engine::{SessionActorRecovery, SystemEventClock, session::control::SessionControl};
use rw_types::{Block, Role, SessionId, Turn, TurnMeta, config::ThinkingLevel};
use std::sync::Arc;

#[test]
fn recovery_retains_bounded_conversation_metadata() {
    let session = SessionId("owned-recovery".into());
    let clock = Arc::new(SystemEventClock);
    let text = "message".repeat(100_000);
    let conversation = vec![Turn {
        role: Role::User,
        blocks: vec![Block::Text { text }],
        meta: TurnMeta::default(),
    }];
    let recovered = SessionActorRecovery {
        conversation: super::super::ConversationSummary::from_turns(&conversation),
        ..SessionActorRecovery::default()
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
    assert_eq!(state.conversation_turns, 1);
    assert!(
        state
            .title_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.len() < 2048)
    );
    assert!(!state.has_assistant_text);
}

#[cfg(test)]
mod live_state {
    use crate::engine::dispatch::live_state::snapshot;
    use crate::engine::{
        SessionActorRecovery, SystemEventClock,
        session::{ActorState, SessionControl},
    };
    use rw_types::{SessionId, config::ThinkingLevel, session_state::MAX_SESSION_QUEUE_ITEMS};
    use std::sync::Arc;

    #[test]
    fn maximum_live_payload_fits_the_source_serialization_allowance() {
        use rw_types::session_state::{
            MAX_PLUGIN_STATUS_BYTES, MAX_SESSION_PLUGIN_STATUSES, SessionCompactionState,
            SessionPluginStatus,
        };
        use rw_types::transcript_tail::{TRANSCRIPT_TAIL_TEXT_BYTES, TranscriptTailText};
        let session = SessionId("maximum-live-state".into());
        let clock = Arc::new(SystemEventClock);
        let statuses = (0..MAX_SESSION_PLUGIN_STATUSES)
            .map(|index| SessionPluginStatus {
                plugin_id: format!("plugin-{index}"),
                status: "\"".repeat(MAX_PLUGIN_STATUS_BYTES),
                source: rw_types::SequenceId(index as u64),
            })
            .collect();
        let mut state = ActorState::recover(
            session.clone(),
            clock.clone(),
            "model",
            ThinkingLevel::Off,
            &rw_ext::ModeRegistry::builtins().expect("modes"),
            SessionActorRecovery {
                plugin_statuses: statuses,
                ..SessionActorRecovery::default()
            },
            Arc::new(SessionControl::new(session, None, clock)),
        );
        state.sequence = Some(1000);
        for position in 0..MAX_SESSION_QUEUE_ITEMS {
            state.queued.push_back(
                "\u{0001}".repeat(rw_types::session_state::MAX_SESSION_QUEUE_PREVIEW_BYTES),
            );
            state.queued_positions.push_back(position as u64);
        }
        let preview = || TranscriptTailText {
            text: "\u{0001}".repeat(TRANSCRIPT_TAIL_TEXT_BYTES),
            truncated: true,
        };
        state.live.compaction = Some(SessionCompactionState {
            revision: u64::MAX,
            text: preview(),
            thinking: preview(),
            summary_turn_id: rw_types::TurnId("turn-1".into()),
            started: rw_types::SequenceId(900),
            attempt: Some(u32::MAX),
        });
        let result = snapshot(&state).expect("maximum escaped snapshot fits");
        assert_eq!(result.plugin_statuses.len(), MAX_SESSION_PLUGIN_STATUSES);
        assert_eq!(result.queued_messages.len(), MAX_SESSION_QUEUE_ITEMS);
        let encoded = serde_json::to_vec(&result).expect("encode");
        assert!(encoded.len() <= rw_types::session_state::MAX_SESSION_STATE_BYTES);
        assert!(
            encoded.len() > 1024 * 1024,
            "exercise the expanded wire representation"
        );
    }

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
            SessionActorRecovery::default(),
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
