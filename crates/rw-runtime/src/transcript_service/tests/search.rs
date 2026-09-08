use super::*;
use rw_types::session_search::SessionSearchMatch;

fn matched(fixture: &Fixture, sequence: u64) -> SessionSearchMatch {
    let prefix = fixture.journal.read_view().prefix_identity();
    SessionSearchMatch {
        session_id: SessionId("semantic".into()),
        source_sequence: SequenceId(sequence),
        through: SequenceId(prefix.next_sequence.checked_sub(1).expect("nonempty")),
        digest: prefix.digest,
    }
}
fn search(
    fixture: &Fixture,
    source: SessionSearchMatch,
) -> Result<TranscriptReadResult, HostError> {
    fixture.service.read(
        &SessionId("semantic".into()),
        &rw_types::session_read::SessionReadScope::Session {},
        &TranscriptRead {
            known_view: None,
            position: TranscriptPosition::SearchMatch { source },
            max_items: 10,
            max_bytes: 64 * 1024,
        },
    )
}

#[test]
fn search_input_source_resolves_claimed_row_and_rejects_stale_or_removed_hits() {
    let mut fixture = Fixture::new(10, "searchable body");
    let source = matched(&fixture, 17);
    let retained = matched(&fixture, 3);
    let page = ready(search(&fixture, source.clone()).expect("search"));
    assert_eq!(
        page.anchor,
        TranscriptAnchor::Exact {
            item: TranscriptItemId(SequenceId(17))
        }
    );
    let mut wrong = source.clone();
    wrong.digest[0] ^= 1;
    assert!(search(&fixture, wrong).is_err());
    let mut wrong = source.clone();
    wrong.session_id = SessionId("another".into());
    assert!(search(&fixture, wrong).is_err());
    let mut accepted = source.clone();
    accepted.source_sequence = SequenceId(16);
    assert!(
        search(&fixture, accepted).is_err(),
        "accepted body is not its selected committed source"
    );
    fixture
        .journal
        .append(&EngineEvent::ConversationRewound {
            meta: meta(20),
            to_agent_turn: 3,
            operation_id: "rewind-search".into(),
            unrestorable_paths: vec![],
        })
        .expect("rewind");
    fixture
        .registration
        .publisher
        .publish(fixture.journal.read_view());
    assert!(
        search(&fixture, source).is_err(),
        "removed matches cannot select a nearby row"
    );
    let page = ready(search(&fixture, retained).expect("retained search"));
    assert_eq!(
        page.anchor,
        TranscriptAnchor::Exact {
            item: TranscriptItemId(SequenceId(3))
        }
    );
}

#[test]
fn search_finished_tool_anchors_its_started_row_and_rejects_non_document_sources() {
    let mut fixture = Fixture::new(0, "");
    fixture
        .journal
        .append_batch([
            EngineEvent::TurnStarted {
                meta: meta(0),
                turn_id: rw_types::TurnId("1".into()),
            },
            EngineEvent::ToolCallStarted {
                meta: meta(1),
                turn_id: rw_types::TurnId("1".into()),
                tool_call_id: rw_types::ToolCallId("provider".into()),
                invocation_id: rw_types::ToolInvocationId("host".into()),
                name: "read".into(),
                args: serde_json::json!({}),
                call_index: 0,
            },
            EngineEvent::ToolCallFinished {
                payloads: Vec::new(),
                meta: meta(2),
                turn_id: rw_types::TurnId("1".into()),
                tool_call_id: rw_types::ToolCallId("provider".into()),
                invocation_id: rw_types::ToolInvocationId("host".into()),
                output: rw_types::ToolOutput::Text {
                    text: "needle".repeat(64 * 1024),
                },
                presentation: None,
                is_error: false,
                call_index: 0,
            },
        ])
        .expect("tool sources");
    fixture
        .registration
        .publisher
        .publish(fixture.journal.read_view());
    let page = ready(search(&fixture, matched(&fixture, 2)).expect("tool hit"));
    assert_eq!(
        page.anchor,
        TranscriptAnchor::Exact {
            item: TranscriptItemId(SequenceId(1))
        }
    );
    assert!(
        search(&fixture, matched(&fixture, 1)).is_err(),
        "arguments were not indexed as a result"
    );
    assert!(search(&fixture, matched(&fixture, 0)).is_err());
    fixture
        .journal
        .append(&EngineEvent::ConversationRewound {
            meta: meta(3),
            to_agent_turn: 0,
            operation_id: "remove-tool".into(),
            unrestorable_paths: vec![],
        })
        .expect("rewind");
    fixture
        .registration
        .publisher
        .publish(fixture.journal.read_view());
    assert!(search(&fixture, matched(&fixture, 2)).is_err());
}
