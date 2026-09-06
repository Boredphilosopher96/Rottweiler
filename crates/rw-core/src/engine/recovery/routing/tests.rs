#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{SessionRoutingIndex, decode_head};
use crate::engine::{
    PendingEvent,
    recovery::{
        CanonicalRecovery, RecoveryError,
        tests::{append, catch_up, terminal},
    },
};
use rw_ext::ModeRegistry;
use rw_store::session::journal::{JournalReadView, SegmentedJournal};
use rw_types::{SequenceId, WorkspaceRootDescriptor};

fn workspace(generation: u64) -> PendingEvent {
    PendingEvent::WorkspaceRootsChanged {
        generation,
        effective_from_turn: generation,
        roots: vec![WorkspaceRootDescriptor {
            index: 0,
            path: "@root/0".into(),
            machine_local: false,
        }],
    }
}
fn converge(index: &mut SessionRoutingIndex, source: &JournalReadView) -> usize {
    for batches in 1..1000 {
        if !index.advance(source).expect("bounded catch-up") {
            return batches;
        }
    }
    panic!("routing failed to converge");
}

#[test]
fn routing_rewind_reuses_exact_boundaries_and_keeps_historical_workspace_sources() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![
            workspace(1),
            PendingEvent::TurnStarted { turn: 1 },
            terminal(1),
        ],
    );
    let mut index = SessionRoutingIndex::open(&journal.read_view()).expect("routing");
    converge(&mut index, &journal.read_view());
    let first = journal.read_view();
    assert_eq!(
        index.completed(&first, 1).expect("first boundary"),
        Some(SequenceId(2))
    );
    append(
        &mut journal,
        vec![
            workspace(2),
            PendingEvent::TurnStarted { turn: 2 },
            terminal(2),
        ],
    );
    let full = journal.read_view();
    assert!(matches!(
        index.completed(&full, 2),
        Err(RecoveryError::Maintenance)
    ));
    converge(&mut index, &full);
    assert_eq!(
        index
            .workspace_at(&full, Some(SequenceId(2)))
            .expect("old route"),
        1
    );
    assert_eq!(
        index
            .workspace_at(&full, full.last_sequence())
            .expect("new route"),
        2
    );
    assert!(
        index.completed(&first, 1).is_err(),
        "new checkpoint cannot masquerade as old cut"
    );
    append(
        &mut journal,
        vec![PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "rewind".into(),
            unrestorable_paths: vec![],
        }],
    );
    converge(&mut index, &journal.read_view());
    assert!(index.completed(&journal.read_view(), 2).is_err());
    append(
        &mut journal,
        vec![PendingEvent::TurnStarted { turn: 2 }, terminal(2)],
    );
    converge(&mut index, &journal.read_view());
    assert_eq!(
        index
            .completed(&journal.read_view(), 2)
            .expect("reused boundary"),
        Some(SequenceId(8))
    );
    assert_eq!(
        index
            .workspace_at(&journal.read_view(), Some(SequenceId(8)))
            .expect("physical route survives rewind"),
        2
    );
    let checkpoint = index.index.head().expect("checkpoint");
    assert!(!index.advance(&journal.read_view()).expect("warm lookup"));
    assert_eq!(
        index.index.head().expect("unchanged checkpoint"),
        checkpoint
    );
    drop(index);
    let reopened = SessionRoutingIndex::open(&journal.read_view()).expect("reopened");
    assert_eq!(
        reopened
            .completed(&journal.read_view(), 2)
            .expect("reopen boundary"),
        Some(SequenceId(8))
    );
}

#[test]
fn routing_pages_and_rewind_cleanup_bound_each_source_transaction() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        (1..=150)
            .flat_map(|turn| [PendingEvent::TurnStarted { turn }, terminal(turn)])
            .collect(),
    );
    let source = journal.read_view();
    let mut index = SessionRoutingIndex::open(&source).expect("routing");
    assert!(index.advance(&source).expect("first page"));
    assert_eq!(
        decode_head(&index.index.read().expect("read"))
            .expect("head")
            .next,
        32
    );
    assert!(converge(&mut index, &source) > 1);
    append(
        &mut journal,
        vec![PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "rewind".into(),
            unrestorable_paths: vec![],
        }],
    );
    assert!(
        converge(&mut index, &journal.read_view()) >= 5,
        "149 boundaries require multiple32-row transactions"
    );
    assert!(index.completed(&journal.read_view(), 150).is_err());
    assert_eq!(
        index.completed(&journal.read_view(), 1).expect("retained"),
        Some(SequenceId(1))
    );
}

#[test]
fn fork_validation_owns_a_separate_checkpoint_and_rejects_raw_user_authority() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            terminal(1),
            PendingEvent::TurnStarted { turn: 2 },
            terminal(2),
        ],
    );
    let source = journal.read_view();
    let mut live = CanonicalRecovery::open(&source, &modes, None).expect("live");
    catch_up(&mut live, &source, &modes);
    let through = source
        .prefix_through(Some(SequenceId(1)))
        .expect("selected prefix");
    let mut fork = CanonicalRecovery::for_fork(&through, &modes, None).expect("fork");
    catch_up(&mut fork, &through, &modes);
    assert_eq!(fork.head().expect("fork head").control.completed_turns, 1);
    assert_eq!(
        live.head()
            .expect("live remains current")
            .control
            .completed_turns,
        2
    );
    drop(fork);
    append(
        &mut journal,
        vec![PendingEvent::ConversationTurnCommitted {
            agent_turn: 2,
            turn: crate::engine::recovery::tests::text(
                rw_types::Role::User,
                "invalid embedded input",
            ),
        }],
    );
    let source = journal.read_view();
    let mut invalid = CanonicalRecovery::for_fork(&source, &modes, None).expect("fork owner");
    assert!(invalid.advance(&source, &modes).is_err());
    assert_eq!(
        live.head()
            .expect("failed validation cannot alter actor")
            .control
            .completed_turns,
        2
    );
}
