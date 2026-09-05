#![allow(clippy::expect_used)]
use rw_store::session::SessionEventLog;

use super::*;
use rw_core::{EventMeta, PROTOCOL_VERSION, SequenceId, SessionId};
use rw_store::session::{SessionProjection, SessionSummary};

fn fixture() -> Vec<EventEnvelope<EngineEvent>> {
    vec![EventEnvelope {
        schema_version: 1,
        sequence: SequenceId(0),
        event: EngineEvent::UiNotification {
            meta: EventMeta {
                protocol_version: PROTOCOL_VERSION,
                session_id: SessionId("golden".to_owned()),
                sequence_id: SequenceId(0),
                emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                caused_by: None,
            },
            plugin_id: "fixture".to_owned(),
            title: "<script>alert(1)</script>".to_owned(),
            message: "key sk-AbCdEf0123456789GhIjKlMn at /Users/alice/private".to_owned(),
        },
    }]
}

#[test]
fn export_formats_match_injection_safe_redacted_goldens() {
    let redactor = FixtureRedactor::default();
    for (format, expected) in [
        (
            TranscriptFormat::Markdown,
            include_bytes!("../../tests/golden/history.md").as_slice(),
        ),
        (
            TranscriptFormat::Html,
            include_bytes!("../../tests/golden/history.html").as_slice(),
        ),
        (
            TranscriptFormat::Json,
            include_bytes!("../../tests/golden/history.json").as_slice(),
        ),
    ] {
        let actual = export_transcript("golden", &fixture(), format, &redactor).expect("export");
        assert_eq!(actual, expected);
        assert!(!String::from_utf8_lossy(&actual).contains("sk-AbCd"));
        assert!(!String::from_utf8_lossy(&actual).contains("/Users/alice"));
    }
}

#[test]
fn replay_is_the_exact_engine_event_jsonl_seam() {
    let replay = replay_jsonl(&fixture()).expect("replay");
    let decoded: EngineEvent = serde_json::from_slice(&replay).expect("event JSON");
    assert_eq!(decoded, fixture()[0].event);
}

#[test]
fn read_only_session_listing_is_newest_first_and_bounded() {
    let storage = tempfile::tempdir().expect("storage");
    let index = SessionIndex::open(storage.path()).expect("index");
    for (id, updated) in [("older", 1), ("newer-b", 2), ("newer-a", 2)] {
        index
            .upsert(&SessionProjection {
                summary: SessionSummary {
                    id: id.to_owned(),
                    title: id.to_owned(),
                    updated_unix_ms: updated,
                    cost_micros: 0,
                    turn_count: 0,
                },
                transcript: String::new(),
                projected_through: None,
            })
            .expect("projection");
    }
    let listed = list_sessions(storage.path(), 2).expect("read-only list");
    assert_eq!(
        listed
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        ["newer-a", "newer-b"]
    );
    assert!(list_sessions(storage.path(), 0).is_err());
    assert!(list_sessions(storage.path(), 1_001).is_err());
}

#[test]
fn full_verification_checks_history_outside_a_recent_page() {
    let storage = tempfile::tempdir().expect("storage");
    let mut log = SessionEventLog::open(storage.path(), "golden").expect("writer");
    let mut first = fixture()[0].event.clone();
    if let EngineEvent::UiNotification { message, .. } = &mut first {
        *message = "x".repeat(1024 * 1024);
    }
    log.append(first).expect("large first segment");
    let mut second = fixture()[0].event.clone();
    second.meta_mut().expect("meta").sequence_id = SequenceId(1);
    log.append(second).expect("second segment");
    let view = log.read_view();
    assert!(
        verify_session(storage.path(), "golden").is_err(),
        "live writer is excluded"
    );
    let segment = fs::read_dir(log.path())
        .expect("segments")
        .map(|entry| entry.expect("segment").path())
        .find(|path| {
            path.file_name()
                .expect("name")
                .to_string_lossy()
                .starts_with("00000000000000000000-")
        })
        .expect("sealed segment");
    drop(log);
    assert_eq!(
        verify_session(storage.path(), "golden")
            .expect("full scan")
            .events,
        2
    );
    let mut bytes = fs::read(&segment).expect("bytes");
    bytes[0] = b'[';
    fs::write(segment, bytes).expect("corrupt history");
    assert_eq!(
        view.page::<EngineEvent>(
            Some(SequenceId(0)),
            rw_store::session::SessionEventPageLimits::default()
        )
        .expect("unrelated recent page")
        .events
        .len(),
        1
    );
    assert!(
        verify_session(storage.path(), "golden")
            .expect_err("historical bitrot")
            .to_string()
            .contains("checksum")
    );
}

#[test]
fn replay_rejects_event_identity_outside_its_durable_envelope() {
    let storage = tempfile::tempdir().expect("storage");
    let mut log = SessionEventLog::open(storage.path(), "history").expect("event log");
    let mut event = fixture()[0].event.clone();
    event.meta_mut().expect("durable meta").session_id = SessionId("other".to_owned());
    log.append(event).expect("mismatched event fixture");
    drop(log);

    let error = load_events(storage.path(), "history").expect_err("identity must fail closed");
    assert!(error.to_string().contains("identity"));
    assert!(
        verify_session(storage.path(), "history")
            .expect_err("verify identity")
            .to_string()
            .contains("identity")
    );
}

#[test]
fn verification_rejects_an_unsupported_event_protocol_version() {
    let storage = tempfile::tempdir().expect("storage");
    let mut log = SessionEventLog::open(storage.path(), "golden").expect("writer");
    let mut event = fixture()[0].event.clone();
    event.meta_mut().expect("meta").protocol_version = PROTOCOL_VERSION + 1;
    log.append(event).expect("unsupported protocol fixture");
    drop(log);
    assert!(verify_session(storage.path(), "golden").is_err());
    assert!(load_events(storage.path(), "golden").is_err());
}

#[test]
#[allow(clippy::too_many_lines)]
fn readable_transcript_groups_conversation_tools_plans_and_accounting() {
    let event = |sequence: u64, event: Value| {
        serde_json::json!({
            "schema_version": 1,
            "sequence": sequence.to_string(),
            "event": event,
        })
    };
    let events = vec![
        event(
            0,
            serde_json::json!({
                "type": "user_message_accepted",
                "meta": {"emitted_at": "2026-01-01T00:00:00Z"},
                "agent_turn": "1",
                "content": "Build the feature",
            }),
        ),
        event(
            1,
            serde_json::json!({
                "type": "text_delta",
                "meta": {"emitted_at": "2026-01-01T00:00:01Z"},
                "turn_id": "turn-1",
                "text": "I will ",
            }),
        ),
        event(
            2,
            serde_json::json!({
                "type": "text_delta",
                "meta": {"emitted_at": "2026-01-01T00:00:02Z"},
                "turn_id": "turn-1",
                "text": "do that.",
            }),
        ),
        event(
            3,
            serde_json::json!({
                "type": "tool_call_started",
                "meta": {"emitted_at": "2026-01-01T00:00:03Z"},
                "turn_id": "turn-1",
                "tool_call_id": "tool-1",
                "name": "read",
                "args": {"path": "README.md"},
            }),
        ),
        event(
            4,
            serde_json::json!({
                "type": "tool_call_finished",
                "meta": {"emitted_at": "2026-01-01T00:00:04Z"},
                "turn_id": "turn-1",
                "tool_call_id": "tool-1",
                "output": {"type": "text", "text": "contents"},
                "is_error": false,
            }),
        ),
        event(
            5,
            serde_json::json!({
                "type": "plan_submitted",
                "meta": {"emitted_at": "2026-01-01T00:00:05Z"},
                "artifact": {
                    "title": "Implementation",
                    "summary_md": "Make the change safely.",
                    "steps": [{
                        "description": "Edit the code",
                        "files_touched": ["src/main.rs"],
                        "verification": "cargo test",
                    }],
                    "open_questions": [],
                },
            }),
        ),
        event(
            6,
            serde_json::json!({
                "type": "turn_finished",
                "meta": {"emitted_at": "2026-01-01T00:00:06Z"},
                "turn_id": "turn-1",
                "status": "completed",
                "usage": {
                    "input_tokens": "10",
                    "output_tokens": "5",
                    "cache_read_tokens": "2",
                    "cache_write_tokens": "0",
                    "reasoning_tokens": "1",
                },
                "cost": {"kind": "monetary", "amount_micros": "42", "currency": "USD"},
            }),
        ),
    ];

    let sections = transcript_sections(&events).expect("readable transcript");
    assert_eq!(
        sections
            .iter()
            .map(|section| section.title.as_str())
            .collect::<Vec<_>>(),
        vec![
            "User",
            "Assistant",
            "Tool call: read",
            "Tool result",
            "Plan: Implementation",
            "Turn finished",
        ]
    );
    assert_eq!(sections[1].body, "I will do that.");
    assert!(sections[4].body.contains("Verify: cargo test"));
    assert!(sections[5].body.contains("input: 10"));
    assert!(sections[5].body.contains("42 micros USD"));
}

#[cfg(unix)]
#[test]
fn export_refuses_symlink_or_storage_targets_without_mutating_events() {
    use std::os::unix::fs::symlink;

    let storage = tempfile::tempdir().expect("storage");
    let session = storage.path().join("sessions/history");
    fs::create_dir_all(session.join("journal")).expect("journal directory");
    let events = session.join("journal/active.jsonl");
    fs::write(&events, b"canary").expect("events");
    let output = tempfile::tempdir().expect("output");
    let planted = output.path().join("transcript.md");
    symlink(&events, &planted).expect("planted symlink");
    assert!(write_transcript_export(storage.path(), &planted, b"replacement", true).is_err());
    assert_eq!(fs::read(&events).expect("events unchanged"), b"canary");
    assert!(
        write_transcript_export(storage.path(), &session.join("export.md"), b"x", false).is_err()
    );
    assert_eq!(
        fs::read(&events).expect("events still unchanged"),
        b"canary"
    );
}

#[cfg(unix)]
#[test]
fn export_parent_swap_stays_bound_to_the_opened_directory_descriptor() {
    use std::os::unix::fs::symlink;

    let storage = tempfile::tempdir().expect("storage");
    let session = storage.path().join("sessions/history");
    fs::create_dir_all(session.join("journal")).expect("journal directory");
    let events = session.join("journal/active.jsonl");
    fs::write(&events, b"event-canary").expect("events");

    let output = tempfile::tempdir().expect("output");
    let parent = output.path().join("safe");
    let moved = output.path().join("moved");
    fs::create_dir(&parent).expect("safe parent");
    let canonical_parent = fs::canonicalize(&parent).expect("canonical parent");
    let parent_for_swap = parent.clone();
    let moved_for_swap = moved.clone();
    let session_for_swap = session.clone();
    write_transcript_export_unix(
        &canonical_parent,
        std::ffi::OsStr::new("transcript.md"),
        b"safe export",
        false,
        move || {
            fs::rename(&parent_for_swap, &moved_for_swap).into_diagnostic()?;
            symlink(&session_for_swap, &parent_for_swap).into_diagnostic()?;
            Ok(())
        },
    )
    .expect("descriptor-bound export");

    assert_eq!(
        fs::read(moved.join("transcript.md")).expect("export"),
        b"safe export"
    );
    assert_eq!(
        fs::read(&events).expect("events unchanged"),
        b"event-canary"
    );
    assert!(!session.join("transcript.md").exists());
}

#[cfg(unix)]
#[test]
fn export_no_clobber_is_atomic_against_a_destination_creation_race() {
    let output = tempfile::tempdir().expect("output");
    let parent = fs::canonicalize(output.path()).expect("canonical output");
    let destination = parent.join("transcript.md");
    let destination_for_race = destination.clone();
    let result = write_transcript_export_unix(
        &parent,
        std::ffi::OsStr::new("transcript.md"),
        b"replacement",
        false,
        move || {
            fs::write(&destination_for_race, b"planted").into_diagnostic()?;
            Ok(())
        },
    );
    assert!(result.is_err());
    assert_eq!(fs::read(destination).expect("planted output"), b"planted");
    assert!(fs::read_dir(&parent).expect("output entries").all(|entry| {
        !entry
            .expect("output entry")
            .file_name()
            .to_string_lossy()
            .starts_with(".rottweiler-export-")
    }));
}
