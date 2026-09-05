#![allow(clippy::expect_used)]
use super::{
    CanonicalHistory, CanonicalRecovery, HistoryMaterializationLimits,
    tests::{append, catch_up, text},
};
use crate::engine::{AgentTurnStatus, PendingEvent, SessionUsage};
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{ContextItemId, Cost, Role};

fn completed(turn: u64) -> PendingEvent {
    PendingEvent::TurnFinished {
        turn,
        status: AgentTurnStatus::Completed,
        usage: SessionUsage::default(),
        cost: Cost::Unavailable {
            reason: "fixture".into(),
        },
    }
}
fn snapshot(recovery: &CanonicalRecovery, journal: &SegmentedJournal) -> CanonicalHistory {
    recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source")
}
fn pinned(history: &CanonicalHistory, ordinal: u64) -> Option<bool> {
    history
        .context_action(&ContextItemId(format!("conversation:{ordinal}")))
        .expect("indexed action")
        .map(|action| action.pinned)
}

#[test]
fn context_revision_seek_restores_rewind_without_resurrecting_discarded_actions() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: text(Role::User, "first"),
            },
            PendingEvent::ContextItemPinned {
                item_id: ContextItemId("conversation:0".into()),
                effective_after_agent_turn: 1,
            },
            completed(1),
            PendingEvent::TurnStarted { turn: 2 },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 2,
                turn: text(Role::User, "discarded"),
            },
            PendingEvent::ContextItemEvicted {
                item_id: ContextItemId("conversation:0".into()),
                effective_after_agent_turn: 2,
            },
            PendingEvent::ContextItemPinned {
                item_id: ContextItemId("conversation:1".into()),
                effective_after_agent_turn: 2,
            },
            completed(2),
        ],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let before = snapshot(&recovery, &journal);
    assert_eq!(pinned(&before, 0), Some(false));
    assert_eq!(pinned(&before, 1), Some(true));
    append(
        &mut journal,
        vec![PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "rewind".into(),
            unrestorable_paths: vec![],
        }],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let rewound = snapshot(&recovery, &journal);
    assert_eq!(pinned(&rewound, 0), Some(true));
    assert_eq!(pinned(&rewound, 1), None);
    assert_eq!(
        pinned(&before, 0),
        Some(false),
        "captured revision remains immutable"
    );
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 3 },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 3,
                turn: text(Role::User, "replacement"),
            },
            PendingEvent::ContextItemEvicted {
                item_id: ContextItemId("conversation:1".into()),
                effective_after_agent_turn: 3,
            },
            completed(3),
        ],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let current = snapshot(&recovery, &journal);
    let page = current
        .conversation_page(0..2, HistoryMaterializationLimits::default())
        .expect("annotated page");
    assert_eq!(page.context_actions.len(), 2);
    assert_eq!(
        page.context_actions[0].as_ref().map(|action| action.pinned),
        Some(true)
    );
    assert_eq!(
        page.context_actions[1].as_ref().map(|action| action.pinned),
        Some(false)
    );
}

#[test]
fn context_latest_revision_is_found_after_many_same_item_updates_and_reopen() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![PendingEvent::ConversationTurnCommitted {
            agent_turn: 1,
            turn: text(Role::User, "same source"),
        }],
    );
    append(
        &mut journal,
        (0..300)
            .map(|index| {
                let item_id = ContextItemId("conversation:0".into());
                if index % 2 == 0 {
                    PendingEvent::ContextItemPinned {
                        item_id,
                        effective_after_agent_turn: 1,
                    }
                } else {
                    PendingEvent::ContextItemEvicted {
                        item_id,
                        effective_after_agent_turn: 1,
                    }
                }
            })
            .collect(),
    );
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    drop(recovery);
    let recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("reopen");
    assert_eq!(pinned(&snapshot(&recovery, &journal), 0), Some(false));
}
