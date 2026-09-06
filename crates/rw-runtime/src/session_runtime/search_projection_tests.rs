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
fn user(sequence: u64, turn: u64, content: &str) -> [EngineEvent; 2] {
    crate::session_runtime::test_history::input_events(meta(sequence), turn, content.into())
}
fn start(sequence: u64, turn: u64, invocation: &str) -> EngineEvent {
    EngineEvent::ToolCallStarted {
        meta: meta(sequence),
        turn_id: TurnId(turn.to_string()),
        tool_call_id: ToolCallId("provider-call".into()),
        invocation_id: ToolInvocationId(invocation.into()),
        name: "read".into(),
        args: serde_json::json!({}),
        call_index: 0,
    }
}
fn finish(sequence: u64, turn: u64, invocation: &str, output: ToolOutput) -> EngineEvent {
    EngineEvent::ToolCallFinished {
        meta: meta(sequence),
        turn_id: TurnId(turn.to_string()),
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
            .append_batch((batch * 100..(batch + 1) * 100).flat_map(|sequence| {
                user(
                    2 * sequence,
                    sequence + 1,
                    &format!("message unique{sequence}"),
                )
            }))
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
        .append_batch(user(600, 301, "freshneedle"))
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
    let mut events = user(0, 1, &format!("{} deepneedle", "title ".repeat(30))).to_vec();
    events.extend([
        EngineEvent::TextDelta {
            meta: meta(2),
            turn_id: TurnId("1".into()),
            text: "inter".into(),
        },
        EngineEvent::TextDelta {
            meta: meta(3),
            turn_id: TurnId("1".into()),
            text: "operability".into(),
        },
        EngineEvent::ConversationTurnCommitted {
            meta: meta(4),
            agent_turn: 1,
            turn: rw_types::Turn {
                role: Role::Assistant,
                blocks: vec![Block::Text {
                    text: "interoperability".into(),
                }],
                meta: rw_types::TurnMeta::default(),
            },
        },
    ]);
    events.push(EngineEvent::TurnStarted {
        meta: meta(5),
        turn_id: TurnId("2".into()),
    });
    events.extend(user(6, 2, "discardedmessage"));
    events.extend([
        start(8, 2, "first"),
        finish(
            9,
            2,
            "first",
            ToolOutput::Structured {
                value: serde_json::json!({"status":"discardedresult", "count":42}),
            },
        ),
        start(10, 2, "pending"),
    ]);
    journal.append_batch(events).expect("append");
    synchronize(root.path(), "search", &journal.read_view()).expect("search source");
    let index = SessionIndex::open(root.path()).expect("index");
    assert_eq!(
        index
            .search("deepneedle interoperability discardedresult 42", 10)
            .expect("fields")
            .len(),
        1
    );
    let mut events = vec![EngineEvent::ConversationRewound {
        meta: meta(11),
        to_agent_turn: 1,
        operation_id: "rewind".into(),
        unrestorable_paths: vec![],
    }];
    events.push(EngineEvent::TurnStarted {
        meta: meta(12),
        turn_id: TurnId("3".into()),
    });
    events.extend(user(13, 3, "replacementmessage"));
    events.extend([
        start(15, 3, "replacement"),
        finish(
            16,
            3,
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
    ]);
    journal
        .append_batch(events)
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
    let published = index.projection("search").expect("published source");
    journal
        .append(finish(
            17,
            2,
            "pending",
            ToolOutput::Text {
                text: "latepoison".into(),
            },
        ))
        .expect("invalid source fixture");
    assert!(synchronize(root.path(), "search", &journal.read_view()).is_err());
    assert_eq!(
        index.projection("search").expect("unchanged index"),
        published
    );
    assert!(
        index
            .search("latepoison", 10)
            .expect("rejected body")
            .is_empty()
    );
}

#[test]
fn failed_page_keeps_cursor_and_documents_atomic_then_resumes() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "search").expect("journal");
    journal
        .append_batch(user(0, 1, "retainedneedle"))
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
        .append_batch(user(2, 2, "completedneedle"))
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
            // Search includes the session title. Give it an independent title
            // so this assertion specifically verifies committed body selection.
            EngineEvent::SessionTitleUpdated {
                meta: meta(2),
                title: "Search session".into(),
                usage: None,
                cost: None,
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

#[test]
fn search_rejects_unclaimed_cross_turn_input_without_publishing_a_new_prefix() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "search").expect("journal");
    let [accepted, mut commit] = user(0, 1, "body needle");
    journal.append_batch([accepted]).expect("accepted source");
    synchronize(root.path(), "search", &journal.read_view()).expect("pending checkpoint");
    let index = SessionIndex::open(root.path()).expect("index");
    let before = index
        .projection("search")
        .expect("projection")
        .expect("row");
    if let EngineEvent::ConversationInputCommitted { agent_turn, .. } = &mut commit {
        *agent_turn = 2;
    }
    journal.append_batch([commit]).expect("raw invalid event");
    assert!(synchronize(root.path(), "search", &journal.read_view()).is_err());
    let after = index
        .projection("search")
        .expect("projection")
        .expect("row");
    assert_eq!(after.source, before.source);
    assert_eq!(after.input_claims, before.input_claims);
}

#[test]
fn search_rejects_reusing_a_consumed_source_after_restart() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "search").expect("journal");
    let events = user(0, 1, "only once");
    let mut duplicate = events[1].clone();
    journal.append_batch(events).expect("committed input");
    synchronize(root.path(), "search", &journal.read_view()).expect("published input");
    let expected = journal.read_view().prefix_identity();
    drop(journal);
    let mut journal = SegmentedJournal::open(root.path(), "search").expect("reopen");
    duplicate.meta_mut().expect("metadata").sequence_id = SequenceId(2);
    journal
        .append_batch([duplicate])
        .expect("duplicate source claim");
    assert!(synchronize(root.path(), "search", &journal.read_view()).is_err());
    assert_eq!(
        SessionIndex::open(root.path())
            .expect("index")
            .projection("search")
            .expect("projection")
            .expect("row")
            .source,
        expected
    );
}
