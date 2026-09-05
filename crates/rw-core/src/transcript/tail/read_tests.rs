#![allow(clippy::expect_used)]
use super::tests::{begin, project, text};
use super::*;
use crate::transcript::tests::{meta, turn};
use rw_types::transcript_tail::*;
use rw_types::{SequenceId, SessionId, TurnId};

fn request(part: TranscriptTailPart) -> TranscriptTailRead {
    TranscriptTailRead {
        expected: None,
        part,
        max_items: TRANSCRIPT_TAIL_PAGE_ITEMS as u16,
        max_bytes: TRANSCRIPT_TAIL_MIN_PAGE_BYTES as u32,
    }
}
fn ready(
    projector: &crate::transcript::TranscriptProjector,
    request: &TranscriptTailRead,
) -> TranscriptTailPage {
    let TranscriptTailResult::Ready { page } =
        read_transcript_tail(projector.index(), &SessionId("semantic".into()), request)
            .expect("read")
    else {
        panic!("not ready");
    };
    page
}
#[test]
fn citations_page_without_lifetime_decode_and_fence_changes_across_response_and_rewind() {
    let uri = "\0".repeat(rw_types::citation_admission::MAX_CITATION_TEXT_BYTES);
    let (_root, mut journal, mut projector) = project(&[
        begin(0, 1),
        EngineEvent::CitationDelta {
            meta: meta(1),
            turn_id: TurnId("1".into()),
            uri: uri.clone(),
            title: None,
        },
        EngineEvent::CitationDelta {
            meta: meta(2),
            turn_id: TurnId("1".into()),
            uri,
            title: Some(String::new()),
        },
    ]);
    let first = ready(
        &projector,
        &request(TranscriptTailPart::Citations { offset: 0 }),
    );
    let TranscriptTailContent::Citations {
        ref items,
        next_offset: Some(1),
        ..
    } = first.content
    else {
        panic!("one bounded citation");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].source, SequenceId(1));
    assert!(serde_json::to_vec(&first).expect("encoded").len() <= TRANSCRIPT_TAIL_MIN_PAGE_BYTES);
    journal
        .append_batch([text(3, "a live append")])
        .expect("append");
    projector.advance(&journal.read_view()).expect("advance");
    let mut second = request(TranscriptTailPart::Citations { offset: 1 });
    second.expected = Some(first.identity.clone());
    let page = ready(&projector, &second);
    assert_eq!(page.view.through, Some(SequenceId(3)));
    let TranscriptTailContent::Citations {
        items,
        next_offset: None,
        ..
    } = page.content
    else {
        panic!("second page");
    };
    assert_eq!(items[0].source, SequenceId(2));
    journal
        .append_batch([turn(4, 1, Role::Assistant, vec![])])
        .expect("commit");
    projector
        .advance(&journal.read_view())
        .expect("commit projection");
    assert!(matches!(
        read_transcript_tail(projector.index(), &SessionId("semantic".into()), &second)
            .expect("fence"),
        TranscriptTailResult::Changed { .. }
    ));
    let current = ready(&projector, &request(TranscriptTailPart::Text {}));
    let TranscriptTailContent::Text { preview } = &current.content else {
        panic!("text");
    };
    assert_eq!(preview.text, "");
    journal
        .append_batch([EngineEvent::ConversationRewound {
            meta: meta(5),
            to_agent_turn: 1,
            operation_id: "source-rewind".into(),
            unrestorable_paths: vec![],
        }])
        .expect("rewind");
    while projector
        .advance(&journal.read_view())
        .expect("rewind projection")
        .has_more
    {}
    let expected = TranscriptTailRead {
        expected: Some(current.identity),
        ..request(TranscriptTailPart::Text {})
    };
    assert!(matches!(
        read_transcript_tail(projector.index(), &SessionId("semantic".into()), &expected)
            .expect("rewind fence"),
        TranscriptTailResult::Changed { .. }
    ));
}

#[test]
fn tool_page_has_exact_provider_identity_source_and_bounded_prefix() {
    let (_root, _journal, projector) = project(&[
        begin(0, 1),
        crate::transcript::tests::start(1, 0),
        EngineEvent::ToolOutputDelta {
            meta: meta(2),
            turn_id: TurnId("1".into()),
            tool_call_id: rw_types::ToolCallId("reused-provider-id".into()),
            invocation_id: rw_types::ToolInvocationId("invocation-0".into()),
            stream: rw_types::ToolOutputStream::Stdout,
            chunk: "x".repeat(TRANSCRIPT_TAIL_TOOL_BYTES + 1),
        },
    ]);
    let page = ready(
        &projector,
        &request(TranscriptTailPart::Tools { offset: 0 }),
    );
    let TranscriptTailContent::Tools {
        items,
        next_offset: None,
        ..
    } = page.content
    else {
        panic!("tools");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].tool_call_id.0, "reused-provider-id");
    assert_eq!(items[0].source, SequenceId(1));
    assert_eq!(items[0].arguments.source.sequence, SequenceId(1));
    assert_eq!(items[0].output.text.len(), TRANSCRIPT_TAIL_TOOL_BYTES);
    assert!(items[0].output.truncated);
}

#[test]
fn invalid_page_admission_does_not_read_or_advance_source() {
    let (_root, _journal, projector) = project(&[begin(0, 1), text(1, "hello")]);
    let head = projector.index().head().expect("head");
    for malformed in [
        TranscriptTailRead {
            max_items: 0,
            ..request(TranscriptTailPart::Text {})
        },
        TranscriptTailRead {
            max_bytes: 0,
            ..request(TranscriptTailPart::Text {})
        },
        request(TranscriptTailPart::Citations { offset: 1 }),
        request(TranscriptTailPart::Tools { offset: u16::MAX }),
    ] {
        assert!(
            read_transcript_tail(projector.index(), &SessionId("semantic".into()), &malformed)
                .is_err()
        );
        assert_eq!(projector.index().head().expect("unchanged"), head);
    }
}
