#![allow(clippy::expect_used)]
use super::*;
use rw_types::{EventMeta, SequenceId, SessionId, TurnId};

fn text(value: String) -> EngineEvent {
    EngineEvent::TextDelta {
        meta: EventMeta {
            protocol_version: rw_types::PROTOCOL_VERSION,
            session_id: SessionId("print".into()),
            sequence_id: SequenceId(0),
            emitted_at: "2026-09-05T00:00:00.000Z".into(),
            caused_by: None,
        },
        turn_id: TurnId("1".into()),
        text: value,
    }
}

#[test]
fn rejected_aggregate_growth_keeps_prior_json_and_allocations_unchanged() {
    let mut aggregate = PrintAggregate::new("print");
    aggregate.limit = 16 * 1024;
    aggregate.push(text("first".into())).expect("first result");
    let original = serde_json::to_vec(&aggregate).expect("result");
    let capacities = (aggregate.text.capacity(), aggregate.events.capacity());
    let failure = aggregate
        .push(text("x".repeat(16 * 1024)))
        .expect_err("admission");
    assert!(failure.to_string().contains("--output-format stream-json"));
    assert_eq!(
        serde_json::to_vec(&aggregate).expect("unchanged result"),
        original
    );
    assert_eq!(
        (aggregate.text.capacity(), aggregate.events.capacity()),
        capacities
    );
    aggregate
        .push(text(" second".into()))
        .expect("small result after rejection");
    assert_eq!(aggregate.text, "first second");
}

#[test]
fn json_event_count_is_bounded_even_for_empty_deltas() {
    let mut aggregate = PrintAggregate::new("print");
    for _ in 0..MAX_JSON_EVENTS {
        aggregate
            .push(text(String::new()))
            .expect("admitted empty delta");
    }
    assert!(aggregate.push(text(String::new())).is_err());
    assert_eq!(aggregate.events.len(), MAX_JSON_EVENTS);
}

#[test]
fn streaming_formats_do_not_accumulate_large_output_history() {
    for format in [OutputFormat::Text, OutputFormat::StreamJson] {
        let mut output = PrintOutput::new("print", format);
        for _ in 0..300 {
            output
                .push(text("x".repeat(256 * 1024)))
                .expect("streamed result");
        }
        assert!(output.aggregate.is_none());
        assert!(!output.ends_newline);
        output.push(text("\n".into())).expect("newline");
        output.push(text(String::new())).expect("empty delta");
        assert!(output.ends_newline);
    }
}

#[test]
fn admitted_json_preserves_public_events_text_and_exact_fields() {
    let mut aggregate = PrintAggregate::new("print");
    let event = text("héllo\n".into());
    let expected_event = serde_json::to_value(&event).expect("public event");
    aggregate.push(event).expect("admission");
    assert_eq!(
        serde_json::to_value(&aggregate).expect("JSON"),
        serde_json::json!({
            "session_id":"print", "status":null, "text":"héllo\n",
            "usage":{"input_tokens":"0","output_tokens":"0","cache_read_tokens":"0","cache_write_tokens":"0","reasoning_tokens":"0"},
            "events":[expected_event]
        })
    );
}
