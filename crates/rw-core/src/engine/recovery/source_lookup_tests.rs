#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{
    CanonicalRecovery,
    tests::{append, catch_up, text},
};
use crate::engine::{AgentTurnStatus, PendingEvent, SessionUsage};
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{Cost, Role, SequenceId};

fn turn(journal: &mut SegmentedJournal, id: u64, body: &str) -> SequenceId {
    let source = SequenceId(journal.read_view().prefix_identity().next_sequence + 2);
    append_script(
        journal,
        vec![
            SourceEvent::Event(PendingEvent::TurnStarted { turn: id }),
            SourceEvent::Input {
                agent_turn: id,
                turn: text(Role::User, body),
            },
            SourceEvent::Event(PendingEvent::TurnFinished {
                turn: id,
                status: AgentTurnStatus::Completed,
                usage: SessionUsage::default(),
                cost: Cost::Unavailable {
                    reason: "fixture".into(),
                },
            }),
        ],
    );
    source
}
#[test]
fn canonical_source_lookup_rejects_rewound_source_after_turn_number_is_reused() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("index");
    let first = turn(&mut journal, 1, "first");
    let removed = turn(&mut journal, 2, "removed");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    {
        let history = recovery
            .snapshot()
            .expect("snapshot")
            .bind_source(&journal.read_view())
            .expect("source");
        assert_initial_sources(
            &history,
            first,
            removed,
            journal.read_view().last_sequence().expect("tail"),
        );
    }
    append(
        &mut journal,
        vec![PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "rewind".into(),
            unrestorable_paths: vec![],
        }],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    assert!(
        recovery
            .snapshot()
            .expect("snapshot")
            .bind_source(&journal.read_view())
            .expect("source")
            .source_turn(removed)
            .expect("lookup")
            .is_none()
    );
    let replacement = turn(&mut journal, 2, "replacement");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let history = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    assert!(history.source_turn(removed).expect("removed").is_none());
    assert_eq!(
        history
            .source_turn(replacement)
            .expect("replacement")
            .expect("effective")
            .0,
        1
    );
    assert_eq!(
        history
            .source_turn(first)
            .expect("first")
            .expect("effective")
            .0,
        0
    );
    assert_eq!(
        history
            .completed_boundary(2)
            .expect("boundary")
            .expect("effective")
            .source_sequence,
        SequenceId(replacement.0 + 1)
    );
}

fn assert_initial_sources(
    history: &super::CanonicalHistory,
    first: SequenceId,
    removed: SequenceId,
    through: SequenceId,
) {
    assert_eq!(
        history
            .source_turn(removed)
            .expect("lookup")
            .expect("effective")
            .1
            .agent_turn,
        2
    );
    assert!(history.completed_before(1).expect("first").is_none());
    assert_eq!(
        history
            .resolve_source_rewind(through, removed, 2, rw_types::RewindSourcePosition::Before)
            .expect("before"),
        1
    );
    assert_eq!(
        history
            .resolve_source_rewind(through, removed, 2, rw_types::RewindSourcePosition::Through)
            .expect("through"),
        2
    );
    assert!(
        history
            .resolve_source_rewind(through, first, 1, rw_types::RewindSourcePosition::Before)
            .is_err()
    );
    assert!(
        history
            .resolve_source_rewind(
                SequenceId(through.0 - 1),
                removed,
                2,
                rw_types::RewindSourcePosition::Through
            )
            .is_err()
    );
    assert!(
        history
            .resolve_source_rewind(through, removed, 1, rw_types::RewindSourcePosition::Through)
            .is_err()
    );

    let before = history
        .completed_before(2)
        .expect("before")
        .expect("boundary");
    assert_eq!(before.agent_turn, 1);
    assert_eq!(before.source_sequence, SequenceId(first.0 + 1));
}

use super::test_source::{SourceEvent, append_script};
