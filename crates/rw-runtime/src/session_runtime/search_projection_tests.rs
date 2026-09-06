#![allow(clippy::expect_used)]
use super::*;
use rw_core::SESSION_EVENT_VERSION;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{EventMeta, SessionId, ToolCallId, ToolInvocationId, TurnId};

fn meta(sequence: u64) -> EventMeta {
    EventMeta {
        protocol_version: SESSION_EVENT_VERSION,
        session_id: SessionId("search".into()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-01-01T00:00:00.000Z".into(),
        caused_by: None,
    }
}
fn user(sequence: u64, turn: u64, content: &str) -> EngineEvent {
    EngineEvent::ConversationTurnCommitted {
        meta: meta(sequence),
        agent_turn: turn,
        turn: rw_types::Turn {
            role: Role::User,
            blocks: vec![Block::Text {
                text: content.into(),
            }],
            meta: rw_types::TurnMeta::default(),
        },
    }
}
fn start(sequence: u64, invocation: &str) -> EngineEvent {
    EngineEvent::ToolCallStarted {
        meta: meta(sequence),
        turn_id: TurnId("2".into()),
        tool_call_id: ToolCallId("provider-call".into()),
        invocation_id: ToolInvocationId(invocation.into()),
        name: "read".into(),
        args: serde_json::json!({}),
        call_index: 0,
    }
}
fn finish(sequence: u64, invocation: &str, output: ToolOutput) -> EngineEvent {
    EngineEvent::ToolCallFinished {
        meta: meta(sequence),
        turn_id: TurnId("2".into()),
        tool_call_id: ToolCallId("provider-call".into()),
        invocation_id: ToolInvocationId(invocation.into()),
        output,
        is_error: false,
        call_index: 0,
        presentation: None,
    }
}

#[test]
fn pages_resume_from_exact_source_without_duplicate_documents() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "search").expect("journal");
    for batch in 0..3 {
        journal
            .append_batch(
                (batch * 100..(batch + 1) * 100).map(|sequence| {
                    user(sequence, sequence + 1, &format!("message unique{sequence}"))
                }),
            )
            .expect("append");
    }
    synchronize(root.path(), "search", &journal.read_view()).expect("paged catchup");
    synchronize(root.path(), "search", &journal.read_view()).expect("warm repeated read");
    let index = SessionIndex::open(root.path()).expect("index");
    assert_eq!(
        index
            .search("unique299 unique0", 10)
            .expect("cross-page search")
            .len(),
        1
    );
    assert_eq!(
        index
            .projection("search")
            .expect("projection")
            .expect("row")
            .source,
        journal.read_view().prefix_identity()
    );
    drop(journal);
    let mut journal = SegmentedJournal::open(root.path(), "search").expect("reopen");
    journal
        .append_batch([user(300, 301, "freshneedle")])
        .expect("append after restart");
    synchronize(root.path(), "search", &journal.read_view()).expect("incremental resumed source");
    assert_eq!(
        index
            .search("freshneedle unique299", 10)
            .expect("search")
            .len(),
        1
    );
}

#[test]
fn committed_words_and_structured_fields_are_searchable_across_rewinds() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "search").expect("journal");
    journal
        .append_batch([
            user(0, 1, &format!("{} deepneedle", "title ".repeat(30))),
            EngineEvent::TextDelta {
                meta: meta(1),
                turn_id: TurnId("1".into()),
                text: "inter".into(),
            },
            EngineEvent::TextDelta {
                meta: meta(2),
                turn_id: TurnId("1".into()),
                text: "operability".into(),
            },
            EngineEvent::ConversationTurnCommitted {
                meta: meta(3),
                agent_turn: 1,
                turn: rw_types::Turn {
                    role: Role::Assistant,
                    blocks: vec![Block::Text {
                        text: "interoperability".into(),
                    }],
                    meta: rw_types::TurnMeta::default(),
                },
            },
            user(4, 2, "discardedmessage"),
            start(5, "first"),
            finish(
                6,
                "first",
                ToolOutput::Structured {
                    value: serde_json::json!({"status":"discardedresult", "count":42}),
                },
            ),
            start(7, "pending"),
        ])
        .expect("append");
    synchronize(root.path(), "search", &journal.read_view()).expect("search source");
    let index = SessionIndex::open(root.path()).expect("index");
    assert_eq!(
        index
            .search("deepneedle interoperability discardedresult 42", 10)
            .expect("fields")
            .len(),
        1
    );
    journal
        .append_batch([
            EngineEvent::ConversationRewound {
                meta: meta(8),
                to_agent_turn: 1,
                operation_id: "rewind".into(),
                unrestorable_paths: vec![],
            },
            user(9, 2, "replacementmessage"),
            start(10, "replacement"),
            finish(
                11,
                "pending",
                ToolOutput::Text {
                    text: "latepoison".into(),
                },
            ),
            finish(
                12,
                "replacement",
                ToolOutput::Mixed {
                    parts: vec![
                        ToolOutputPart::Text {
                            text: "validresult".into(),
                        },
                        ToolOutputPart::Structured {
                            value: serde_json::json!([true, "nestedvalue"]),
                        },
                    ],
                },
            ),
        ])
        .expect("rewind and replacement");
    synchronize(root.path(), "search", &journal.read_view()).expect("rewind source");
    for absent in ["discardedmessage", "discardedresult", "latepoison"] {
        assert!(index.search(absent, 10).expect("removed source").is_empty());
    }
    assert_eq!(
        index
            .search("deepneedle interoperability validresult nestedvalue", 10)
            .expect("retained source")
            .len(),
        1
    );
}

#[test]
fn failed_page_keeps_cursor_and_documents_atomic_then_resumes() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "search").expect("journal");
    journal
        .append_batch([user(0, 1, "retainedneedle")])
        .expect("first");
    synchronize(root.path(), "search", &journal.read_view()).expect("initial");
    let index = SessionIndex::open(root.path()).expect("index");
    let old = index
        .projection("search")
        .expect("projection")
        .expect("row");
    let mut candidate = old.clone();
    candidate.complete = false;
    candidate.source.next_sequence += 1;
    let failure = index.apply_page(Some(old.source), &candidate, |writer| {
        writer.text(2, SequenceId(1), 0, "uncommittedneedle")?;
        Err(SessionStoreError::CorruptEvent("injected page failure"))
    });
    assert!(failure.is_err());
    assert_eq!(
        index.projection("search").expect("projection"),
        Some(old.clone())
    );
    assert!(
        index
            .search("uncommittedneedle", 10)
            .expect("atomic body")
            .is_empty()
    );
    candidate.source = old.source;
    index
        .upsert(&candidate)
        .expect("incomplete catchup metadata");
    assert!(
        index
            .search("retainedneedle", 10)
            .expect("incomplete hidden")
            .is_empty()
    );
    journal
        .append_batch([user(1, 2, "completedneedle")])
        .expect("next");
    synchronize(root.path(), "search", &journal.read_view()).expect("resume");
    assert_eq!(
        index
            .search("retainedneedle completedneedle", 10)
            .expect("completed")
            .len(),
        1
    );
}

#[test]
fn referenced_input_searches_selected_text() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "search").expect("journal");
    journal
        .append_batch([
            EngineEvent::UserMessageAccepted {
                meta: meta(0),
                agent_turn: 1,
                content: "unselectedneedle".into(),
                attachments: vec![],
            },
            EngineEvent::ConversationInputCommitted {
                meta: meta(1),
                agent_turn: 1,
                accepted_source: SequenceId(0),
                selection: rw_types::conversation_input::InputSelection::Transformed {
                    text: "selectedneedle".into(),
                },
            },
        ])
        .expect("input");
    synchronize(root.path(), "search", &journal.read_view()).expect("projection");
    let index = SessionIndex::open(root.path()).expect("index");
    assert!(
        index
            .search("unselectedneedle", 10)
            .expect("unselected")
            .is_empty()
    );
    assert_eq!(
        index.search("selectedneedle", 10).expect("selected").len(),
        1
    );
}
