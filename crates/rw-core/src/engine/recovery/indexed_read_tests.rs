#![allow(clippy::expect_used)]
use super::{
    tests::{append, catch_up, text},
    *,
};
use crate::engine::{AgentTurnStatus, PendingEvent, SessionUsage};
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{Cost, Role, SequenceId};

fn terminal(turn: u64) -> PendingEvent {
    PendingEvent::TurnFinished {
        turn,
        status: AgentTurnStatus::Completed,
        usage: SessionUsage::default(),
        cost: Cost::Unavailable {
            reason: "fixture".into(),
        },
    }
}

#[test]
fn accounting_byte_cut_resumes_exactly_and_rewind_preserves_billed_history() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    for turn in 1..=2 {
        append_script(
            &mut journal,
            vec![
                SourceEvent::event(PendingEvent::TurnStarted { turn }),
                SourceEvent::Input {
                    agent_turn: turn,
                    turn: text(Role::User, "message"),
                },
                SourceEvent::event(terminal(turn)),
            ],
        );
    }
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let history = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    let both = history
        .accounting_page(None, 128, MAX_ACCOUNTING_PAGE_BYTES)
        .expect("all accounting");
    assert_eq!(both.events.len(), 2);
    assert_accounting_page_cuts(&history, &both);
    assert_eq!(
        history
            .completed_boundary(2)
            .expect("boundary")
            .expect("present")
            .conversation
            .turns,
        2
    );
    assert_eq!(
        history.window_estimated_tokens(0..2).expect("tokens"),
        2 * rw_context::LocalTokenEstimator::turn(&text(Role::User, "message"))
    );
    append(
        &mut journal,
        vec![PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "rewind".into(),
            unrestorable_paths: vec![],
        }],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let rewound = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    assert!(
        rewound
            .completed_boundary(2)
            .expect("old boundary")
            .is_none()
    );
    assert_eq!(
        rewound
            .completed_boundary(1)
            .expect("boundary")
            .expect("present")
            .conversation
            .turns,
        1
    );
    assert_eq!(
        rewound
            .accounting_page(None, 128, MAX_ACCOUNTING_PAGE_BYTES)
            .expect("billing history")
            .events,
        both.events
    );
    assert!(
        history
            .completed_boundary(2)
            .expect("older snapshot")
            .is_some()
    );
}

fn assert_accounting_page_cuts(history: &CanonicalHistory, both: &RecoveryAccountingPage) {
    let first_bytes = super::encoding::serialized_size(&both.events[0]).expect("size");
    let first = history
        .accounting_page(None, 128, first_bytes)
        .expect("first byte cut");
    assert_eq!(first.events.len(), 1);
    assert_eq!(first.next_cursor, Some(SequenceId(3)));
    assert!(first.has_more);
    let second = history
        .accounting_page(first.next_cursor, 128, MAX_ACCOUNTING_PAGE_BYTES)
        .expect("second page");
    assert_eq!(second.events, both.events[1..]);
    assert!(!second.has_more);
    assert!(matches!(
        history.accounting_page(None, 128, first_bytes - 1),
        Err(RecoveryError::Limit(_))
    ));
    assert!(matches!(
        history.accounting_page(Some(SequenceId(8)), 128, MAX_ACCOUNTING_PAGE_BYTES),
        Err(RecoveryError::Invalid(_))
    ));
}

use super::test_source::{SourceEvent, append_script};
