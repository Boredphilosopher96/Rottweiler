#![allow(clippy::expect_used)]
use super::{
    CanonicalRecovery, HistoryMaterializationLimits,
    tests::{append, catch_up, terminal, text},
};
use crate::engine::PendingEvent;
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{Role, SequenceId};

fn usage(turn: u64) -> PendingEvent {
    PendingEvent::ContextUsage {
        turn,
        used_tokens: 10,
        usable_tokens: 100,
        reserved_tokens: 0,
        context_window_known: true,
        context_window_reason: None,
        stable_prefix_hash: "fixture".into(),
        cache_hit_basis_points: 0,
        estimated_input_tokens: 10,
        provider_input_tokens: 0,
        correction_millionths: 1_000_000,
    }
}
#[test]
fn prompt_cut_precedes_answer_and_tracks_effective_reused_turn_identity() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("source");
    let mut index = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("index");
    for turn in 1..=2 {
        append(
            &mut journal,
            vec![
                PendingEvent::TurnStarted { turn },
                PendingEvent::ConversationTurnCommitted {
                    agent_turn: turn,
                    turn: text(Role::User, &format!("request {turn}")),
                },
                usage(turn),
                PendingEvent::ConversationTurnCommitted {
                    agent_turn: turn,
                    turn: text(Role::Assistant, &format!("answer {turn}")),
                },
                usage(turn),
                terminal(turn),
            ],
        );
    }
    catch_up(&mut index, &journal.read_view(), &modes);
    let before = index
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("reader");
    let prompt = before.prompt_at_turn(2).expect("indexed prompt");
    assert_eq!(prompt.head().next_sequence, 9);
    assert!(
        prompt
            .bootstrap()
            .expect("prompt controls")
            .interrupted
            .is_none()
    );
    let page = prompt
        .conversation_page(0..3, HistoryMaterializationLimits::default())
        .expect("prompt page");
    assert_eq!(
        page.turns,
        vec![
            text(Role::User, "request 1"),
            text(Role::Assistant, "answer 1"),
            text(Role::User, "request 2")
        ]
    );
    assert_eq!(page.sources[2].sequence, SequenceId(7));
    append(
        &mut journal,
        vec![
            PendingEvent::ConversationRewound {
                to_turn: 1,
                operation_id: "rewind".into(),
                unrestorable_paths: vec![],
            },
            PendingEvent::TurnStarted { turn: 2 },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 2,
                turn: text(Role::User, "replacement"),
            },
        ],
    );
    catch_up(&mut index, &journal.read_view(), &modes);
    let after = index
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("reader");
    assert!(
        after.prompt_at_turn(2).is_err(),
        "new turn has not assembled a context"
    );
    assert!(after.prompt_at_turn(1).is_ok());
    assert_eq!(
        before
            .prompt_at_turn(2)
            .expect("pinned source")
            .head()
            .next_sequence,
        9
    );
    append(&mut journal, vec![usage(2)]);
    catch_up(&mut index, &journal.read_view(), &modes);
    let current = index
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("reader");
    let replacement = current.prompt_at_turn(2).expect("replacement prompt");
    assert_eq!(replacement.head().next_sequence, 16);
    assert_eq!(
        replacement
            .conversation_page(2..3, HistoryMaterializationLimits::default())
            .expect("replacement source")
            .turns,
        vec![text(Role::User, "replacement")]
    );
}
