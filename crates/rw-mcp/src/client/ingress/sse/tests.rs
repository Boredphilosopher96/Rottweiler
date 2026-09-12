#![allow(clippy::expect_used)]
use super::*;

fn events(input: &[u8], chunk_size: usize, limit: usize) -> Vec<SseEvent> {
    let mut parser = SseParser::new(limit).expect("parser admission");
    let mut output = Vec::new();
    for chunk in input.chunks(chunk_size) {
        let mut offset = 0;
        while offset < chunk.len() {
            let (consumed, event) = parser.push(&chunk[offset..]).expect("SSE framing");
            assert!(consumed > 0);
            offset += consumed;
            if let Some(event) = event {
                output.push(event);
            }
        }
    }
    parser.finish();
    output
}

#[test]
fn bom_crlf_utf8_and_multiline_fields_are_invariant_under_every_chunk_boundary() {
    let wire = "\u{feff}: ignored\r\nevent: message\r\nid: λ-1\r\nretry: 250\r\ndata: {\"text\":\r\ndata: \"λ\"}\r\n\r\n";
    for chunk in 1..=wire.len() {
        let output = events(wire.as_bytes(), chunk, 1024);
        assert_eq!(output.len(), 1, "chunk {chunk}");
        let event = &output[0];
        assert_eq!(
            event.data.as_ref().expect("data").bytes,
            b"{\"text\":\n\"\xce\xbb\"}"
        );
        assert_eq!(event.metadata.event.as_deref(), Some("message"));
        assert_eq!(event.metadata.id.as_deref(), Some("λ-1"));
        assert_eq!(event.metadata.retry, Some(250));
    }
}

#[test]
fn dispatch_stops_before_the_next_event_and_metadata_survives_parser_drop() {
    let first = b"id: retained\ndata: first\n\n";
    let mut wire = first.to_vec();
    wire.extend_from_slice(b"data: second\n\n");
    let mut parser = SseParser::new(64).expect("parser");
    let (consumed, first_event) = parser.push(&wire).expect("first");
    assert_eq!(consumed, first.len());
    let event = first_event.expect("first event");
    let retained_id = Arc::clone(&event.metadata);
    let (_, second) = parser.push(&wire[consumed..]).expect("second");
    assert_eq!(
        second.expect("second event").data.expect("data").bytes,
        b"second"
    );
    drop(event);
    parser.finish();
    assert_eq!(retained_id.id.as_deref(), Some("retained"));
}

#[test]
fn eof_discards_unterminated_event_and_incomplete_utf8_without_dispatch() {
    for suffix in [b"data: lost".as_slice(), b"data: lost\n", b"data: \xce"] {
        let mut wire = b"data: kept\n\n".to_vec();
        wire.extend_from_slice(suffix);
        let output = events(&wire, 1, 64);
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].data.as_ref().expect("data").bytes, b"kept");
    }
}

#[test]
fn empty_data_id_reset_nul_id_and_metadata_only_events_preserve_framing() {
    let output = events(
        b": comment\n\nid: old\n\nid: bad\0id\nid:\ndata:\ndata: \n\nretry: +12\n\n",
        3,
        64,
    );
    assert_eq!(output.len(), 3);
    assert!(output[0].data.is_none());
    assert_eq!(output[0].metadata.id.as_deref(), Some("old"));
    assert_eq!(output[1].metadata.id.as_deref(), Some(""));
    assert_eq!(output[1].data.as_ref().expect("empty lines").bytes, b"\n");
    assert_eq!(output[2].metadata.retry, Some(12));
}

#[test]
fn invalid_and_duplicate_fields_fail_without_accepting_a_following_event() {
    for wire in [
        b"event: a\nevent: b\n\n".as_slice(),
        b"id: a\nid: b\n\n",
        b"retry: 1\nretry: 2\n\n",
        b"retry: no\n\n",
        b"unknown: ignored?\n\n",
        b"data\n\n",
        b"data: \xff\n\n",
        b"id: \xff\n\n",
    ] {
        let mut parser = SseParser::new(64).expect("parser");
        assert!(parser.push(wire).is_err(), "{wire:?}");
        assert!(
            parser.push(b"data: next\n\n").is_err(),
            "error remains terminal"
        );
    }
}

#[test]
fn data_limit_is_exact_and_metadata_and_comment_work_have_separate_bounds() {
    let output = events(b"data: 12345678\n\n", 2, 8);
    assert_eq!(output[0].data.as_ref().expect("exact data").bytes.len(), 8);
    let mut parser = SseParser::new(8).expect("parser");
    assert!(parser.push(b"data: 123456789\n\n").is_err());
    let mut parser = SseParser::new(8).expect("parser");
    let mut wire = b"id: ".to_vec();
    wire.extend(std::iter::repeat_n(b'x', METADATA_BYTES + 1));
    assert!(parser.push(&wire).is_err());
    let mut parser = SseParser::new(8).expect("parser");
    parser.push(b":").expect("comment prefix");
    let comments = vec![b'x'; 4 * METADATA_BYTES + 8];
    assert!(parser.push(&comments).is_err(), "ignored work is bounded");
}

#[test]
fn long_data_line_streams_into_one_frame_without_a_second_line_buffer() {
    let length = 1024 * 1024;
    let mut parser = SseParser::new(length).expect("parser");
    parser.push(b"data:").expect("prefix");
    let chunk = vec![b'x'; 16 * 1024];
    for _ in 0..length / chunk.len() {
        assert!(parser.push(&chunk).expect("data chunk").1.is_none());
        assert!(parser.line.value.is_empty());
        assert_eq!(parser.line.value.capacity(), METADATA_BYTES);
    }
    let event = parser.push(b"\n\n").expect("dispatch").1.expect("event");
    let data = event.data.expect("data");
    assert_eq!(data.bytes.len(), length);
    assert!(data.bytes.iter().all(|byte| *byte == b'x'));
    assert_eq!(
        data.parser_working_bytes().expect("physical charge"),
        3 * data.bytes.capacity()
    );
}
