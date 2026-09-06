#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{
    CanonicalRecovery, HistoryMaterializationLimits,
    tests::{append, catch_up, terminal},
};
use crate::engine::PendingEvent;
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{
    SequenceId,
    conversation_input::{ContextSelection, InputSelection},
};

fn retain(selected: u64, body: u64) -> PendingEvent {
    PendingEvent::ConversationContextCommitted {
        agent_turn: 1,
        selection: ContextSelection::Retained {
            selected_source: SequenceId(selected),
            body_source: SequenceId(body),
        },
    }
}
fn start() -> PendingEvent {
    PendingEvent::CompactionStarted {
        reason: rw_types::CompactionReason::Manual,
    }
}
fn finish() -> PendingEvent {
    PendingEvent::CompactionFinished {
        summary_turn: 1,
        reclaimed_tokens: 0,
        usage: None,
        cost: None,
    }
}
fn input() -> Vec<PendingEvent> {
    vec![
        PendingEvent::TurnStarted { turn: 1 },
        PendingEvent::UserMessageAccepted {
            turn: 1,
            content: "retained input".into(),
            attachments: vec![],
        },
        PendingEvent::ConversationInputCommitted {
            agent_turn: 1,
            accepted_source: SequenceId(1),
            selection: InputSelection::Accepted {},
        },
        terminal(1),
    ]
}
#[test]
fn repeated_compaction_retains_terminal_input_source_without_reference_chains() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut events = input();
    events.extend([
        start(),
        retain(2, 2),
        finish(),
        start(),
        retain(5, 2),
        finish(),
    ]);
    append(&mut journal, events);
    let modes = ModeRegistry::builtins().expect("modes");
    let source = journal.read_view();
    let mut recovery = CanonicalRecovery::open(&source, &modes, None).expect("recovery");
    catch_up(&mut recovery, &source, &modes);
    let history = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&source)
        .expect("source");
    let (_, selected) = history
        .source_turn(SequenceId(8))
        .expect("row")
        .expect("current");
    assert_eq!(selected.body_source, SequenceId(2));
    assert!(
        history
            .source_turn(SequenceId(5))
            .expect("removed generation")
            .is_none()
    );
    assert_eq!(
        history
            .materialize(0..1, HistoryMaterializationLimits::default())
            .expect("input"),
        [super::tests::text(rw_types::Role::User, "retained input")]
    );
}

#[test]
fn retained_context_requires_effective_selection_compaction_and_terminal_body() {
    for extra in [
        vec![retain(2, 2)],
        vec![start(), retain(1, 1)],
        vec![start(), retain(2, 1)],
        vec![start(), retain(2, 2), finish(), start(), retain(5, 5)],
    ] {
        let root = tempfile::tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
        let mut events = input();
        events.extend(extra);
        append(&mut journal, events);
        let modes = ModeRegistry::builtins().expect("modes");
        let source = journal.read_view();
        let mut recovery = CanonicalRecovery::open(&source, &modes, None).expect("recovery");
        assert!(recovery.advance(&source, &modes).is_err());
    }
}
