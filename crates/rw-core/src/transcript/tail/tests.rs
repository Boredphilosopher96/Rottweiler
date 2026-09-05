#![allow(clippy::expect_used)]
use super::*;
use crate::transcript::{
    TranscriptProjector,
    tests::{finish, meta, start, turn},
};
use rw_store::session::journal::SegmentedJournal;
use rw_types::{SequenceId, ToolOutputStream, TurnId};

fn begin(sequence: u64, turn: u64) -> EngineEvent {
    EngineEvent::TurnStarted {
        meta: meta(sequence),
        turn_id: TurnId(turn.to_string()),
    }
}
fn text(sequence: u64, body: &str) -> EngineEvent {
    EngineEvent::TextDelta {
        meta: meta(sequence),
        turn_id: TurnId("1".into()),
        text: body.into(),
    }
}
fn project(events: &[EngineEvent]) -> (tempfile::TempDir, SegmentedJournal, TranscriptProjector) {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "semantic").expect("journal");
    journal.append_batch(events).expect("source");
    let view = journal.read_view();
    let mut projector = TranscriptProjector::open(&view).expect("projector");
    while projector.advance(&view).expect("bounded advance").has_more {}
    (root, journal, projector)
}
fn bytes(projector: &TranscriptProjector, first: u16, len: usize) -> Vec<u8> {
    let count = len.div_ceil(MAX_AUXILIARY_CELL_BYTES) as u16;
    let result = projector
        .index()
        .auxiliary_range(first, count, len)
        .expect("bounded read");
    assert_eq!(result.len(), len);
    result
}

#[test]
fn fragmented_unicode_tail_matches_whole_input_and_survives_reopen() {
    let body = "x🙂界".repeat(9000);
    let mut events = vec![begin(0, 1)];
    for (index, chunk) in body.as_bytes().chunks(4000).enumerate() {
        // The repeated unit is eight bytes, so these fragments preserve UTF-8 boundaries.
        events.push(text(
            index as u64 + 1,
            std::str::from_utf8(chunk).expect("utf8"),
        ));
    }
    let (_root, journal, projector) = project(&events);
    let expected = chunks::utf8_prefix(&body, TRANSCRIPT_TAIL_TEXT_BYTES);
    assert_eq!(
        bytes(&projector, TEXT_FIRST, expected.len()),
        expected.as_bytes()
    );
    assert!(projector.tail_state().expect("tail").text_truncated);
    let captured = projector.index().head().expect("head").prefix;
    drop(projector);
    let projector = TranscriptProjector::open(&journal.read_view()).expect("reopen");
    assert_eq!(projector.index().head().expect("head").prefix, captured);
    assert_eq!(
        bytes(&projector, TEXT_FIRST, expected.len()),
        expected.as_bytes()
    );
    let (_whole_root, _whole_journal, whole) = project(&[begin(0, 1), text(1, &body)]);
    assert_eq!(
        bytes(&whole, TEXT_FIRST, expected.len()),
        bytes(&projector, TEXT_FIRST, expected.len())
    );
}

#[test]
fn response_epochs_replace_fixed_cells_and_independent_reasoning_extent() {
    let (_root, mut journal, mut projector) = project(&[
        begin(0, 1),
        text(1, "old text"),
        EngineEvent::ThinkingDelta {
            meta: meta(2),
            turn_id: TurnId("1".into()),
            text: "reason".into(),
            signature: None,
        },
    ]);
    assert_eq!(bytes(&projector, THINKING_FIRST, 6), b"reason");
    for epoch in 0..20_u64 {
        let sequence = 3 + epoch * 2;
        journal
            .append_batch([
                turn(sequence, 1, Role::Assistant, vec![]),
                text(sequence + 1, "new"),
            ])
            .expect("response");
        projector.advance(&journal.read_view()).expect("advance");
        let state = projector.tail_state().expect("state");
        assert_eq!(state.epoch, sequence);
        assert_eq!(state.text_bytes, 3);
        assert_eq!(state.thinking_bytes, 0);
        assert_eq!(bytes(&projector, TEXT_FIRST, 3), b"new");
    }
    // Only fixed text/reasoning keys were written; another epoch does not allocate a namespace.
    assert!(
        projector
            .index()
            .auxiliary_cell(TEXT_FIRST + 1)
            .expect("cell")
            .is_none()
    );
    assert_eq!(
        journal
            .read_view()
            .prefix_identity()
            .next_sequence
            .checked_sub(1)
            .map(SequenceId),
        Some(SequenceId(42))
    );
}

#[test]
fn worst_escaped_citation_fits_one_transaction_and_roundtrips_exactly() {
    let uri = "\0".repeat(rw_types::citation_admission::MAX_CITATION_TEXT_BYTES);
    let (_root, _journal, projector) = project(&[
        begin(0, 1),
        EngineEvent::CitationDelta {
            meta: meta(1),
            turn_id: TurnId("1".into()),
            uri: uri.clone(),
            title: None,
        },
    ]);
    let state = projector.tail_state().expect("tail");
    assert_eq!(state.citation_count, 1);
    assert_eq!(state.citation_utf8_bytes, uri.len());
    let encoded = bytes(
        &projector,
        CITATION_DATA_FIRST,
        state.citation_encoded_bytes,
    );
    let value: serde_json::Value = serde_json::from_slice(&encoded).expect("citation");
    assert_eq!(value["uri"], uri);
    let index = projector
        .index()
        .auxiliary_cell(CITATION_INDEX_FIRST)
        .expect("cell")
        .expect("index");
    assert_eq!(read_u64(&index[8..16]), 1);
    assert_eq!(read_u32(&index[20..24]) as usize, encoded.len());
}

fn output(sequence: u64, invocation: u32, stream: ToolOutputStream, body: &str) -> EngineEvent {
    EngineEvent::ToolOutputDelta {
        meta: meta(sequence),
        turn_id: TurnId("1".into()),
        tool_call_id: rw_types::ToolCallId("reused-provider-id".into()),
        invocation_id: rw_types::ToolInvocationId(format!("invocation-{invocation}")),
        stream,
        chunk: body.into(),
    }
}
#[test]
fn tool_preview_uses_invocation_identity_and_reuses_retired_slot() {
    let (_root, mut journal, mut projector) = project(&[
        begin(0, 1),
        start(1, 0),
        output(2, 0, ToolOutputStream::Stdout, "out"),
        output(3, 0, ToolOutputStream::Stderr, "error"),
    ]);
    let expected = b"out\n[stderr]\nerror";
    assert_eq!(bytes(&projector, TOOL_DATA_FIRST, expected.len()), expected);
    journal
        .append_batch([
            finish(4, 0, "full final result"),
            start(5, 1),
            output(6, 1, ToolOutputStream::Stdout, "new"),
        ])
        .expect("next invocation");
    projector.advance(&journal.read_view()).expect("advance");
    assert_eq!(projector.tail_state().expect("tail").tools_count, 1);
    assert_eq!(bytes(&projector, TOOL_DATA_FIRST, 3), b"new");
    let cell = projector
        .index()
        .auxiliary_cell(TOOL_INDEX)
        .expect("cell")
        .expect("metadata");
    assert_eq!(read_u64(&cell[8..16]), 5);
    assert_eq!(read_u32(&cell[16..20]), 3);
}
