#![allow(clippy::expect_used)]
use super::{Output, Sanitized, VALUE_BYTES, render, visit_sections};
use crate::history::{EngineEvent, EventEnvelope, FixtureRedactor, TranscriptFormat};
use rw_types::{EventMeta, PROTOCOL_VERSION, SequenceId, SessionId, TurnId};
use serde_json::json;
use std::cell::Cell;

fn event(text: &str) -> EventEnvelope<EngineEvent> {
    EventEnvelope {
        schema_version: 1,
        sequence: SequenceId(0),
        event: EngineEvent::TextDelta {
            meta: EventMeta {
                protocol_version: PROTOCOL_VERSION,
                session_id: SessionId("export".into()),
                sequence_id: SequenceId(0),
                emitted_at: "2026-01-01T00:00:00Z".into(),
                caused_by: None,
            },
            turn_id: TurnId("turn".into()),
            text: text.into(),
        },
    }
}

#[test]
fn each_format_admits_escaped_output_and_final_delimiter_before_growth() {
    let events = [event("<>&\"'\n🦀")];
    let redactor = FixtureRedactor::default();
    for format in [
        TranscriptFormat::Json,
        TranscriptFormat::Markdown,
        TranscriptFormat::Html,
    ] {
        let reference = render("export", &events, format, &redactor, 8192).expect("reference");
        let exact =
            render("export", &events, format, &redactor, reference.len()).expect("exact cap");
        assert_eq!(exact, reference);
        assert!(exact.capacity() <= reference.len());
        assert!(render("export", &events, format, &redactor, reference.len() - 1).is_err());
    }
    let mut escaped = Output::new(7);
    assert!(escaped.html("&&").is_err());
    let bytes = escaped.finish();
    assert!(bytes.len() <= 7);
    assert!(bytes.capacity() <= 7);
}

#[test]
fn section_delivery_does_not_collect_the_history_before_rendering() {
    let prepared = Cell::new(0);
    let delivered = Cell::new(0);
    let events = (0..32).map(|index| {
        prepared.set(prepared.get() + 1);
        Ok(json!({"sequence":index, "event":{"type":"ui_notification", "title":"note", "message":"body"}}))
    });
    visit_sections(events, 4096, |_| {
        delivered.set(delivered.get() + 1);
        assert!(
            prepared.get() - delivered.get() <= 1,
            "only one incoming section may accompany delivery"
        );
        Ok(())
    })
    .expect("stream sections");
    assert_eq!(delivered.get(), 32);
}

#[test]
fn merged_sections_preserve_chunk_boundaries_and_first_metadata() {
    let events = [
        json!({"sequence":"0", "event":{"type":"text_delta", "turn_id":"turn", "text":"first\n"}}),
        json!({"sequence":"1", "event":{"type":"text_delta", "turn_id":"turn", "text":"second"}}),
    ];
    let mut sections = Vec::new();
    visit_sections(events.clone().into_iter().map(Ok), 12, |section| {
        sections.push(section);
        Ok(())
    })
    .expect("exact merged section");
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].body, "first\nsecond");
    assert_eq!(sections[0].metadata[0].1, "0");
    assert!(visit_sections(events.into_iter().map(Ok), 11, |_| Ok(())).is_err());
}

#[test]
fn source_admission_rejects_before_materializing_an_unadmitted_value() {
    let events = [event("text")];
    let redactor = FixtureRedactor::default();
    let mut source = Sanitized::new(&events, &redactor);
    source.remaining = 0;
    assert!(
        source
            .next()
            .expect("event")
            .expect_err("source byte admission")
            .to_string()
            .contains("encoded admission")
    );
    let oversized = [event(&"x".repeat(16 * 1024 * 1024))];
    assert!(
        Sanitized::new(&oversized, &redactor)
            .next()
            .expect("oversized event")
            .is_err()
    );
    assert_eq!(VALUE_BYTES, 64 * 1024 * 1024);
}
