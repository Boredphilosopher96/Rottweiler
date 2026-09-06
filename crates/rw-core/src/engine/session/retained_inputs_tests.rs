//! Crash repair retains one accepted body; terminal cancellation abandons its claim.
#![allow(clippy::expect_used)]
use super::{SessionActorRecovery, recovery::interrupted_turn_recovery_events};
use crate::engine::{
    AgentTurnStatus, PendingEvent,
    recovery::{CanonicalRecovery, HistoryRead, RecoveryBootstrap},
};
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{
    AttachmentData, Cost, EngineEvent, EventMeta, PROTOCOL_VERSION, SequenceId, SessionId,
    StoredAttachment, conversation_input::InputSelection,
};

fn event(sequence: u64, pending: PendingEvent) -> EngineEvent {
    pending.stamp(EventMeta {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId("retained".into()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-09-06T00:00:00.000Z".into(),
        caused_by: None,
    })
}
fn append(journal: &mut SegmentedJournal, events: Vec<PendingEvent>) {
    let first = journal.read_view().prefix_identity().next_sequence;
    journal
        .append_batch(
            events
                .into_iter()
                .enumerate()
                .map(|(i, item)| event(first + u64::try_from(i).expect("fixture index"), item)),
        )
        .expect("append");
}
fn bootstrap(
    journal: &SegmentedJournal,
) -> Result<RecoveryBootstrap, crate::recovery::RecoveryError> {
    let modes = ModeRegistry::builtins().expect("modes");
    let source = journal.read_view();
    let mut index = CanonicalRecovery::open(&source, &modes, None)?;
    while index.advance(&source, &modes)?.has_more {}
    index.snapshot()?.bind_source(&source)?.bootstrap()
}
fn input() -> PendingEvent {
    let content = "retained attachment\n".repeat(1024);
    PendingEvent::UserMessageAccepted {
        turn: 1,
        content: "accepted once".into(),
        attachments: vec![StoredAttachment {
            name: "retained.txt".into(),
            source_path: None,
            media_type: "text/plain".into(),
            content_hash: blake3::hash(content.as_bytes()).to_hex().to_string(),
            byte_len: u64::try_from(content.len()).expect("length"),
            data: AttachmentData::Text { content },
        }],
    }
}
fn retained() -> PendingEvent {
    PendingEvent::UserMessageRetained {
        accepted_source: SequenceId(1),
    }
}
fn terminal(turn: u64) -> PendingEvent {
    PendingEvent::TurnFinished {
        turn,
        status: AgentTurnStatus::Interrupted,
        usage: Default::default(),
        cost: Cost::Unavailable {
            reason: "interrupted".into(),
        },
    }
}
fn commit(turn: u64) -> PendingEvent {
    PendingEvent::ConversationInputCommitted {
        agent_turn: turn,
        accepted_source: SequenceId(1),
        selection: InputSelection::Accepted {},
    }
}
fn repair(journal: &SegmentedJournal) -> Vec<PendingEvent> {
    let recovered = SessionActorRecovery::from_bootstrap(HistoryRead::new(
        bootstrap(journal).expect("bootstrap"),
        (),
    ))
    .expect("actor recovery");
    interrupted_turn_recovery_events(&recovered)
}

#[test]
fn repeated_crash_repair_claims_one_accepted_body_without_duplicate_markers() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "retained").expect("journal");
    append(
        &mut journal,
        vec![PendingEvent::TurnStarted { turn: 1 }, input()],
    );
    for turn in 1..=3 {
        let pending = repair(&journal);
        assert!(matches!(
            pending.as_slice(),
            [
                PendingEvent::UserMessageRetained { .. },
                PendingEvent::TurnFinished { .. }
            ]
        ));
        // A crash after the marker must not emit that attempt's marker twice.
        append(&mut journal, vec![pending[0].clone()]);
        let remaining = repair(&journal);
        assert!(matches!(
            remaining.as_slice(),
            [PendingEvent::TurnFinished { .. }]
        ));
        append(&mut journal, remaining);
        assert!(repair(&journal).is_empty());
        let state = bootstrap(&journal).expect("reopen");
        assert_eq!(state.controls.accepted_messages.len(), 1);
        assert_eq!(state.head.control.accepted[0].agent_turn, 1);
        assert_eq!(state.head.control.accepted[0].claimed_turn, turn);
        append(
            &mut journal,
            vec![PendingEvent::TurnStarted { turn: turn + 1 }],
        );
        assert!(!bootstrap(&journal).expect("claimed").head.control.accepted[0].retained);
    }
    append(&mut journal, vec![commit(4), terminal(4)]);
    assert!(
        bootstrap(&journal)
            .expect("consumed")
            .head
            .control
            .accepted
            .is_empty()
    );
    // Only the original source contains the attachment; every retry is metadata.
    let source = journal.read_view();
    let events = source
        .page::<EngineEvent>(None, Default::default())
        .expect("events")
        .events
        .into_iter()
        .map(|row| row.event)
        .collect::<Vec<_>>();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, EngineEvent::UserMessageAccepted { .. }))
            .count(),
        1
    );
    let audit = crate::engine::project_session_events(&events).expect("audit exact claims");
    assert_eq!(audit.conversation.len(), 1);
    let expected =
        crate::engine::recovery::input::resolve_input(&event(100, commit(4)), &event(1, input()))
            .expect("body");
    assert_eq!(audit.conversation[0], expected);
}

#[test]
fn explicit_cancellation_discards_original_and_resumed_unretained_claims() {
    for resumed in [false, true] {
        let root = tempfile::tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "retained").expect("journal");
        append(
            &mut journal,
            vec![PendingEvent::TurnStarted { turn: 1 }, input()],
        );
        if resumed {
            append(
                &mut journal,
                vec![
                    retained(),
                    terminal(1),
                    PendingEvent::TurnStarted { turn: 2 },
                ],
            );
        }
        append(&mut journal, vec![terminal(if resumed { 2 } else { 1 })]);
        assert!(
            bootstrap(&journal)
                .expect("cancelled")
                .head
                .control
                .accepted
                .is_empty()
        );
        assert!(repair(&journal).is_empty());
        append(
            &mut journal,
            vec![PendingEvent::TurnStarted { turn: 3 }, commit(3)],
        );
        assert!(bootstrap(&journal).is_err());
    }
}

#[test]
fn retained_source_rejects_duplicate_wrong_phase_and_unclaimed_cross_turn_commits() {
    let cases = [
        vec![retained(), retained()],
        vec![
            retained(),
            PendingEvent::TurnFinished {
                turn: 1,
                status: AgentTurnStatus::Completed,
                usage: Default::default(),
                cost: Cost::Unavailable {
                    reason: "fixture".into(),
                },
            },
        ],
        vec![retained(), PendingEvent::TurnStarted { turn: 2 }],
        vec![retained(), terminal(1), commit(2)],
        vec![terminal(1), retained()],
        vec![PendingEvent::UserMessageRetained {
            accepted_source: SequenceId(0),
        }],
        vec![
            retained(),
            terminal(1),
            PendingEvent::TurnStarted { turn: 2 },
            commit(1),
        ],
    ];
    for tail in cases {
        let root = tempfile::tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "retained").expect("journal");
        append(
            &mut journal,
            vec![PendingEvent::TurnStarted { turn: 1 }, input()],
        );
        append(&mut journal, tail);
        assert!(bootstrap(&journal).is_err());
        let events = journal
            .read_view()
            .page::<EngineEvent>(None, Default::default())
            .expect("events")
            .events
            .into_iter()
            .map(|row| row.event)
            .collect::<Vec<_>>();
        assert!(crate::engine::project_session_events(&events).is_err());
    }
}

#[test]
fn rewind_discards_pending_claims_without_resurrecting_accepted_input() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "retained").expect("journal");
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            input(),
            retained(),
            terminal(1),
            PendingEvent::TurnStarted { turn: 2 },
            retained(),
            terminal(2),
            PendingEvent::ConversationRewound {
                to_turn: 1,
                operation_id: "rewind".into(),
                unrestorable_paths: vec![],
            },
        ],
    );
    assert!(
        bootstrap(&journal)
            .expect("rewound")
            .head
            .control
            .accepted
            .is_empty()
    );
    assert!(repair(&journal).is_empty());
    let events = journal
        .read_view()
        .page::<EngineEvent>(None, Default::default())
        .expect("events")
        .events
        .into_iter()
        .map(|row| row.event)
        .collect::<Vec<_>>();
    assert!(
        crate::engine::project_session_events(&events)
            .expect("audit")
            .conversation
            .is_empty()
    );
}
