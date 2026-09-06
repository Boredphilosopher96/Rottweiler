#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{
    CanonicalRecovery, HistoryMaterializationLimits, materialize_conversation_event,
    tests::{append, catch_up, event, terminal},
};
use crate::engine::PendingEvent;
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{
    AttachmentData, Block, EngineEvent, SequenceId, StoredAttachment,
    conversation_input::InputSelection,
};

fn accepted() -> PendingEvent {
    let body = "attachment-source".repeat(2048);
    PendingEvent::UserMessageAccepted {
        turn: 1,
        content: "original".into(),
        attachments: vec![StoredAttachment {
            name: "source.txt".into(),
            source_path: None,
            media_type: "text/plain".into(),
            content_hash: blake3::hash(body.as_bytes()).to_hex().to_string(),
            byte_len: body.len() as u64,
            data: AttachmentData::Text { content: body },
        }],
    }
}
fn commit(source: u64, selection: InputSelection) -> PendingEvent {
    PendingEvent::ConversationInputCommitted {
        agent_turn: 1,
        accepted_source: SequenceId(source),
        selection,
    }
}

#[test]
fn referenced_input_has_one_attachment_body_and_identical_recovery_and_display() {
    for selection in [
        InputSelection::Accepted {},
        InputSelection::Transformed {
            text: "hook text".into(),
        },
    ] {
        let root = tempfile::tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
        let input = accepted();
        let selected = commit(1, selection);
        assert!(
            serde_json::to_vec(&event(2, selected.clone()))
                .expect("commit JSON")
                .len()
                < 400
        );
        let expected =
            super::input::resolve_input(&event(2, selected.clone()), &event(1, input.clone()))
                .expect("resolved");
        let pending = vec![
            PendingEvent::TurnStarted { turn: 1 },
            input,
            selected,
            terminal(1),
        ];
        let events = pending
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, pending)| event(index as u64, pending))
            .collect::<Vec<_>>();
        let audit = crate::engine::project_session_events(&events).expect("audit");
        assert_eq!(
            audit.conversation.as_slice(),
            std::slice::from_ref(&expected)
        );
        append(&mut journal, pending);
        let source = journal.read_view();
        let modes = ModeRegistry::builtins().expect("modes");
        let mut recovery = CanonicalRecovery::open(&source, &modes, None).expect("recovery");
        catch_up(&mut recovery, &source, &modes);
        let history = recovery
            .snapshot()
            .expect("snapshot")
            .bind_source(&source)
            .expect("source");
        assert_eq!(
            history
                .materialize(0..1, HistoryMaterializationLimits::default())
                .expect("IR"),
            [expected]
        );
        assert!(
            history
                .bootstrap()
                .expect("bootstrap")
                .controls
                .accepted_messages
                .is_empty()
        );
        assert_eq!(
            history
                .source_turn(SequenceId(2))
                .expect("source")
                .expect("commit")
                .0,
            0
        );
        assert!(
            history
                .source_turn(SequenceId(1))
                .expect("accepted source")
                .is_none()
        );
        assert_display(&source, &events[2]);
    }
}

fn assert_display(source: &rw_store::session::journal::JournalReadView, commit: &EngineEvent) {
    let resolved = materialize_conversation_event(source, commit).expect("resolve display");
    let mut projector = crate::transcript::TranscriptProjector::open(source).expect("transcript");
    while projector.advance(source).expect("advance display").has_more {}
    let rows = projector.index().page(0, 8, 128 * 1024).expect("rows");
    assert!(rows.rows.iter().any(|row| row.source == SequenceId(2)));
    let document = crate::transcript::TranscriptDocument::from_event(
        resolved.into_owned(),
        &rw_types::transcript::TranscriptContentSource {
            sequence: SequenceId(2),
            selector: rw_types::transcript::TranscriptContentSelector::ConversationBlock {
                index: 1,
            },
        },
        128 * 1024,
    )
    .expect("attachment source body");
    assert!(
        document
            .chunk(0, 128 * 1024)
            .expect("content")
            .text
            .contains("attachment-source")
    );
}

#[test]
fn input_commit_rejects_wrong_forward_consumed_and_redundant_sources() {
    let cases = [
        vec![PendingEvent::ConversationTurnCommitted {
            agent_turn: 1,
            turn: super::tests::text(rw_types::Role::User, "embedded bypass"),
        }],
        vec![commit(0, InputSelection::Accepted {})],
        vec![commit(2, InputSelection::Accepted {})],
        vec![commit(
            1,
            InputSelection::Transformed {
                text: "original".into(),
            },
        )],
        vec![PendingEvent::ConversationInputCommitted {
            agent_turn: 2,
            accepted_source: SequenceId(1),
            selection: InputSelection::Accepted {},
        }],
        vec![
            commit(1, InputSelection::Accepted {}),
            commit(1, InputSelection::Accepted {}),
        ],
        vec![terminal(1), commit(1, InputSelection::Accepted {})],
    ];
    for invalid in cases {
        let root = tempfile::tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
        let mut events = vec![PendingEvent::TurnStarted { turn: 1 }, accepted()];
        events.extend(invalid);
        append(&mut journal, events);
        let modes = ModeRegistry::builtins().expect("modes");
        let source = journal.read_view();
        let mut recovery = CanonicalRecovery::open(&source, &modes, None).expect("recovery");
        assert!(recovery.advance(&source, &modes).is_err());
    }
}

#[test]
fn input_reference_cannot_cross_session_or_select_a_different_event() {
    let commit = event(2, commit(1, InputSelection::Accepted {}));
    let mut foreign = event(1, accepted());
    if let EngineEvent::UserMessageAccepted { meta, .. } = &mut foreign {
        meta.session_id.0 = "foreign".into();
    }
    assert!(super::input::resolve_input(&commit, &foreign).is_err());
    assert!(
        super::input::resolve_input(&commit, &event(1, PendingEvent::TurnStarted { turn: 1 }))
            .is_err()
    );
}

#[test]
fn rewind_removes_the_commit_identity_without_changing_accepted_body() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            accepted(),
            commit(1, InputSelection::Accepted {}),
            terminal(1),
            PendingEvent::TurnStarted { turn: 2 },
            PendingEvent::UserMessageAccepted {
                turn: 2,
                content: "discarded".into(),
                attachments: vec![],
            },
            PendingEvent::ConversationInputCommitted {
                agent_turn: 2,
                accepted_source: SequenceId(5),
                selection: InputSelection::Accepted {},
            },
            terminal(2),
            PendingEvent::ConversationRewound {
                to_turn: 1,
                operation_id: "rewind".into(),
                unrestorable_paths: vec![],
            },
        ],
    );
    let modes = ModeRegistry::builtins().expect("modes");
    let source = journal.read_view();
    let mut recovery = CanonicalRecovery::open(&source, &modes, None).expect("recovery");
    catch_up(&mut recovery, &source, &modes);
    let history = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&source)
        .expect("history");
    assert!(
        history
            .source_turn(SequenceId(6))
            .expect("removed")
            .is_none()
    );
    let turns = history
        .materialize(0..1, HistoryMaterializationLimits::default())
        .expect("IR");
    assert!(matches!(&turns[0].blocks[0], Block::Text { text } if text == "original"));
}
