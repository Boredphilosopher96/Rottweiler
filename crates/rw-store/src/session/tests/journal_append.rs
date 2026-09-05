use super::*;

#[test]
fn empty_session_gc_removes_only_unlocked_turnless_directories() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let mut removable = SessionEventLog::open(root.path(), "session-removable")
        .unwrap_or_else(|error| panic!("empty log must open: {error}"));
    removable
        .append(serde_json::json!({"type": "session_created"}))
        .unwrap_or_else(|error| panic!("startup event must append: {error}"));
    drop(removable);
    let active = SessionEventLog::open(root.path(), "session-active")
        .unwrap_or_else(|error| panic!("active log must open: {error}"));
    let preserved = SessionEventLog::open(root.path(), "session-preserved")
        .unwrap_or_else(|error| panic!("preserved log must open: {error}"));
    std::fs::write(
        root.path().join("sessions/session-preserved/metadata.json"),
        b"{}",
    )
    .unwrap_or_else(|error| panic!("metadata fixture must write: {error}"));
    drop(preserved);
    let mut meaningful = SessionEventLog::open(root.path(), "session-meaningful")
        .unwrap_or_else(|error| panic!("meaningful log must open: {error}"));
    meaningful
        .append(serde_json::json!({"type": "turn_started"}))
        .unwrap_or_else(|error| panic!("turn event must append: {error}"));
    drop(meaningful);

    let removed = garbage_collect_empty_sessions(root.path())
        .unwrap_or_else(|error| panic!("empty session collection must work: {error}"));
    assert_eq!(removed, vec!["session-removable"]);
    assert!(!root.path().join("sessions/session-removable").exists());
    assert!(
        root.path()
            .join("sessions/session-active/journal/active.jsonl")
            .is_file()
    );
    assert!(
        root.path()
            .join("sessions/session-preserved/metadata.json")
            .is_file()
    );
    assert!(
        root.path()
            .join("sessions/session-meaningful/journal/active.jsonl")
            .is_file()
    );
    drop(active);
}

#[test]
fn killed_partial_tail_is_truncated_and_sequence_resumes() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let mut log = SessionEventLog::open(root.path(), "session-1")
        .unwrap_or_else(|error| panic!("log must open: {error}"));
    log.append(FixtureEvent {
        kind: "user".to_owned(),
        text: "complete".to_owned(),
    })
    .unwrap_or_else(|error| panic!("event must append: {error}"));
    let mut file = OpenOptions::new()
        .append(true)
        .open(log.path().join("active.jsonl"))
        .unwrap_or_else(|error| panic!("tail file must open: {error}"));
    file.write_all(br#"{"schema_version":1,"sequence":1,"event":{"kind":"assistant""#)
        .unwrap_or_else(|error| panic!("partial tail must write: {error}"));
    file.sync_data()
        .unwrap_or_else(|error| panic!("partial tail must sync: {error}"));
    drop(file);
    drop(log);

    let mut recovered = SessionEventLog::open(root.path(), "session-1")
        .unwrap_or_else(|error| panic!("partial tail must recover: {error}"));
    assert_eq!(recovered.next_sequence(), 1);
    recovered
        .append(FixtureEvent {
            kind: "assistant".to_owned(),
            text: "resumed".to_owned(),
        })
        .unwrap_or_else(|error| panic!("resumed event must append: {error}"));
    let events = recovered
        .load::<FixtureEvent>()
        .unwrap_or_else(|error| panic!("events must load: {error}"));
    assert_eq!(
        events,
        vec![
            EventEnvelope {
                schema_version: 1,
                sequence: SequenceId(0),
                event: FixtureEvent {
                    kind: "user".to_owned(),
                    text: "complete".to_owned(),
                },
            },
            EventEnvelope {
                schema_version: 1,
                sequence: SequenceId(1),
                event: FixtureEvent {
                    kind: "assistant".to_owned(),
                    text: "resumed".to_owned(),
                },
            },
        ]
    );
}

#[test]
fn partial_append_failure_rolls_back_and_the_writer_can_continue() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let mut log = SessionEventLog::open(root.path(), "partial-append")
        .unwrap_or_else(|error| panic!("log must open: {error}"));
    log.append(FixtureEvent {
        kind: "user".to_owned(),
        text: "durable prefix".to_owned(),
    })
    .unwrap_or_else(|error| panic!("prefix must append: {error}"));
    let before = std::fs::read(log.path().join("active.jsonl"))
        .unwrap_or_else(|error| panic!("prefix bytes must read: {error}"));

    let fault = install_append_fault(7, false);
    assert!(matches!(
        log.append(FixtureEvent {
            kind: "assistant".to_owned(),
            text: "must roll back".to_owned(),
        }),
        Err(SessionStoreError::Io(_))
    ));
    assert_eq!(
        std::fs::read(log.path().join("active.jsonl"))
            .unwrap_or_else(|error| panic!("rolled-back bytes: {error}")),
        before
    );
    drop(fault);

    log.append(FixtureEvent {
        kind: "assistant".to_owned(),
        text: "clean retry".to_owned(),
    })
    .unwrap_or_else(|error| panic!("retry must append: {error}"));
    drop(log);

    let recovered = SessionEventLog::open(root.path(), "partial-append")
        .unwrap_or_else(|error| panic!("clean retry must recover: {error}"));
    assert_eq!(recovered.next_sequence(), 2);
    assert_eq!(
        recovered
            .load::<FixtureEvent>()
            .unwrap_or_else(|error| panic!("recovered events must load: {error}"))
            .into_iter()
            .map(|event| event.event.text)
            .collect::<Vec<_>>(),
        vec!["durable prefix", "clean retry"]
    );
}

#[test]
fn trailing_malformed_record_with_newline_fails_closed_without_truncating() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let mut log = SessionEventLog::open(root.path(), "malformed-tail")
        .unwrap_or_else(|error| panic!("log must open: {error}"));
    log.append(FixtureEvent {
        kind: "user".to_owned(),
        text: "complete".to_owned(),
    })
    .unwrap_or_else(|error| panic!("event must append: {error}"));
    let path = log.path().join("active.jsonl");
    let mut file = OpenOptions::new()
        .append(true)
        .open(log.path().join("active.jsonl"))
        .unwrap_or_else(|error| panic!("tail file must open: {error}"));
    file.write_all(b"{\"schema_version\":1,\"sequence\":1,\"event\":\n")
        .unwrap_or_else(|error| panic!("malformed tail must write: {error}"));
    file.sync_data()
        .unwrap_or_else(|error| panic!("malformed tail must sync: {error}"));
    drop(file);
    drop(log);

    let before_open =
        std::fs::read(&path).unwrap_or_else(|error| panic!("corrupt bytes must read: {error}"));
    assert!(matches!(
        SessionEventLog::open(root.path(), "malformed-tail"),
        Err(SessionStoreError::Json(_))
    ));
    assert_eq!(
        std::fs::read(path).unwrap_or_else(|error| panic!("preserved bytes must read: {error}")),
        before_open
    );
}

#[test]
fn trailing_unsupported_version_with_newline_fails_closed_without_truncating() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let log = SessionEventLog::open(root.path(), "unsupported-tail")
        .unwrap_or_else(|error| panic!("log must open: {error}"));
    let path = log.path().join("active.jsonl");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap_or_else(|error| panic!("tail file must open: {error}"));
    file.write_all(b"{\"schema_version\":2,\"sequence\":\"0\",\"event\":{}}\n")
        .unwrap_or_else(|error| panic!("unsupported tail must write: {error}"));
    file.sync_data()
        .unwrap_or_else(|error| panic!("unsupported tail must sync: {error}"));
    drop(file);
    drop(log);

    let before_open =
        std::fs::read(&path).unwrap_or_else(|error| panic!("unsupported bytes must read: {error}"));
    assert!(matches!(
        SessionEventLog::open(root.path(), "unsupported-tail"),
        Err(SessionStoreError::UnsupportedEventVersion(2))
    ));
    assert_eq!(
        std::fs::read(path).unwrap_or_else(|error| panic!("preserved bytes must read: {error}")),
        before_open
    );
}

#[test]
fn trailing_non_contiguous_record_with_newline_fails_closed_without_truncating() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let log = SessionEventLog::open(root.path(), "non-contiguous-tail")
        .unwrap_or_else(|error| panic!("log must open: {error}"));
    let path = log.path().join("active.jsonl");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap_or_else(|error| panic!("tail file must open: {error}"));
    file.write_all(b"{\"schema_version\":1,\"sequence\":\"1\",\"event\":{}}\n")
        .unwrap_or_else(|error| panic!("non-contiguous tail must write: {error}"));
    file.sync_data()
        .unwrap_or_else(|error| panic!("non-contiguous tail must sync: {error}"));
    drop(file);
    drop(log);

    let before_open = std::fs::read(&path)
        .unwrap_or_else(|error| panic!("non-contiguous bytes must read: {error}"));
    assert!(matches!(
        SessionEventLog::open(root.path(), "non-contiguous-tail"),
        Err(SessionStoreError::CorruptEvent(
            "non-contiguous event sequence"
        ))
    ));
    assert_eq!(
        std::fs::read(path).unwrap_or_else(|error| panic!("preserved bytes must read: {error}")),
        before_open
    );
}

#[test]
fn malformed_record_before_the_tail_fails_closed() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let mut log = SessionEventLog::open(root.path(), "malformed-middle")
        .unwrap_or_else(|error| panic!("log must open: {error}"));
    log.append(FixtureEvent {
        kind: "user".to_owned(),
        text: "complete".to_owned(),
    })
    .unwrap_or_else(|error| panic!("event must append: {error}"));
    let mut file = OpenOptions::new()
        .append(true)
        .open(log.path().join("active.jsonl"))
        .unwrap_or_else(|error| panic!("tail file must open: {error}"));
    file.write_all(b"{\"schema_version\":1,\"sequence\":1,\"event\":\n")
        .unwrap_or_else(|error| panic!("malformed middle must write: {error}"));
    file.write_all(
        br#"{"schema_version":1,"sequence":1,"event":{"kind":"assistant","text":"later"}}"#,
    )
    .unwrap_or_else(|error| panic!("later event must write: {error}"));
    file.write_all(b"\n")
        .unwrap_or_else(|error| panic!("later event delimiter must write: {error}"));
    file.sync_data()
        .unwrap_or_else(|error| panic!("malformed middle must sync: {error}"));
    drop(file);
    drop(log);

    assert!(matches!(
        SessionEventLog::open(root.path(), "malformed-middle"),
        Err(SessionStoreError::Json(_))
    ));
}

#[test]
fn failed_rollback_poisons_the_writer() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let mut log = SessionEventLog::open(root.path(), "poisoned-writer")
        .unwrap_or_else(|error| panic!("log must open: {error}"));
    let fault = install_append_fault(1, true);
    assert!(matches!(
        log.append(FixtureEvent {
            kind: "user".to_owned(),
            text: "will fail".to_owned(),
        }),
        Err(SessionStoreError::AppendRollbackFailed { .. })
    ));
    drop(fault);
    assert!(matches!(
        log.append(FixtureEvent {
            kind: "user".to_owned(),
            text: "must not write".to_owned(),
        }),
        Err(SessionStoreError::EventWriterPoisoned)
    ));
}

#[test]
fn batch_append_assigns_exact_envelopes_and_serialization_is_all_or_nothing() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let mut log = SessionEventLog::open(root.path(), "batch")
        .unwrap_or_else(|error| panic!("log must open: {error}"));
    let appended = log
        .append_batch([
            FixtureEvent {
                kind: "turn_started".to_owned(),
                text: "one".to_owned(),
            },
            FixtureEvent {
                kind: "user_message".to_owned(),
                text: "two".to_owned(),
            },
        ])
        .unwrap_or_else(|error| panic!("batch must append: {error}"));
    assert_eq!(
        appended
            .iter()
            .map(|envelope| envelope.sequence)
            .collect::<Vec<_>>(),
        vec![SequenceId(0), SequenceId(1)]
    );
    assert_eq!(
        log.load::<FixtureEvent>()
            .unwrap_or_else(|error| panic!("batch must load: {error}")),
        appended
    );

    let before = std::fs::read(log.path().join("active.jsonl"))
        .unwrap_or_else(|error| panic!("batch log must read: {error}"));
    assert!(
        log.append_batch([
            FailableEvent {
                text: "serializable",
                fail: false,
            },
            FailableEvent {
                text: "fails",
                fail: true,
            },
        ])
        .is_err()
    );
    assert_eq!(log.next_sequence(), 2);
    assert_eq!(
        std::fs::read(log.path().join("active.jsonl"))
            .unwrap_or_else(|error| panic!("batch log must reread: {error}")),
        before
    );
}

#[test]
fn five_hundred_event_batch_round_trips_contiguously() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let mut log = SessionEventLog::open(root.path(), "five-hundred")
        .unwrap_or_else(|error| panic!("log must open: {error}"));
    let events = (0..500).map(|index| FixtureEvent {
        kind: "sample".to_owned(),
        text: index.to_string(),
    });
    let appended = log
        .append_batch(events)
        .unwrap_or_else(|error| panic!("batch must append: {error}"));
    assert_eq!(appended.len(), 500);
    assert_eq!(
        appended.last().map(|event| event.sequence),
        Some(SequenceId(499))
    );
    assert_eq!(
        log.load::<FixtureEvent>()
            .unwrap_or_else(|error| panic!("batch must load: {error}")),
        appended
    );
}

#[test]
fn unknown_envelope_fields_reject_committed_records_without_rewriting() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let log = SessionEventLog::open(root.path(), "invalid-fields")
        .unwrap_or_else(|error| panic!("log: {error}"));
    let path = log.path().join("active.jsonl");
    let bytes =
        b"{\"schema_version\":1,\"sequence\":\"0\",\"event\":{},\"unrecognized_field\":true}\n";
    std::fs::write(&path, bytes).unwrap_or_else(|error| panic!("fixture: {error}"));
    drop(log);
    assert!(SessionEventLog::open(root.path(), "invalid-fields").is_err());
    assert_eq!(
        std::fs::read(path).unwrap_or_else(|error| panic!("read: {error}")),
        bytes
    );
}

#[test]
fn event_envelope_requires_each_declared_field() {
    let valid = serde_json::json!({"schema_version": 1, "sequence": "0", "event": {}});
    for field in ["schema_version", "sequence", "event"] {
        let mut value = valid.clone();
        value
            .as_object_mut()
            .unwrap_or_else(|| panic!("object"))
            .remove(field);
        assert!(serde_json::from_value::<EventEnvelope<serde_json::Value>>(value).is_err());
    }
}

#[test]
fn persisted_sequence_is_authoritative_decimal_string() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let mut log = SessionEventLog::open(root.path(), "sequence")
        .unwrap_or_else(|error| panic!("log must open: {error}"));
    assert_eq!(log.last_sequence(), None);
    let persisted = log
        .append(FixtureEvent {
            kind: "user".to_owned(),
            text: "hello".to_owned(),
        })
        .unwrap_or_else(|error| panic!("event must persist: {error}"));
    assert_eq!(persisted.sequence, SequenceId(0));
    let raw = std::fs::read_to_string(log.path().join("active.jsonl"))
        .unwrap_or_else(|error| panic!("log must read: {error}"));
    assert!(raw.contains("\"sequence\":\"0\""));
    assert!(!raw.contains("\"sequence\":0"));
    assert_eq!(log.last_sequence(), Some(SequenceId(0)));
    let before = raw;
    assert!(
        log.append_expected(
            SequenceId(2),
            FixtureEvent {
                kind: "assistant".to_owned(),
                text: "must not write".to_owned(),
            }
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(log.path().join("active.jsonl"))
            .unwrap_or_else(|error| panic!("unchanged log must read: {error}")),
        before
    );
}

#[cfg(unix)]
#[test]
fn a_session_log_has_one_process_writer() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let log = SessionEventLog::open(root.path(), "single-writer")
        .unwrap_or_else(|error| panic!("first writer must open: {error}"));
    assert!(SessionEventLog::open(root.path(), "single-writer").is_err());
    drop(log);
    SessionEventLog::open(root.path(), "single-writer")
        .unwrap_or_else(|error| panic!("writer lock must release: {error:?}"));
}

#[cfg(unix)]
#[test]
fn session_log_rejects_symlink_escape_components() {
    use std::os::unix::fs::symlink;

    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let outside = tempdir().unwrap_or_else(|error| panic!("outside must create: {error}"));
    std::fs::create_dir_all(root.path().join("sessions/file-link/journal"))
        .unwrap_or_else(|error| panic!("session directory must create: {error}"));
    let outside_log = outside.path().join("outside.jsonl");
    std::fs::write(&outside_log, b"outside")
        .unwrap_or_else(|error| panic!("outside log must write: {error}"));
    symlink(
        &outside_log,
        root.path().join("sessions/file-link/journal/active.jsonl"),
    )
    .unwrap_or_else(|error| panic!("event symlink must create: {error}"));
    assert!(SessionEventLog::open(root.path(), "file-link").is_err());
    assert_eq!(
        std::fs::read(&outside_log)
            .unwrap_or_else(|error| panic!("outside log must read: {error}")),
        b"outside"
    );

    symlink(outside.path(), root.path().join("sessions/directory-link"))
        .unwrap_or_else(|error| panic!("directory symlink must create: {error}"));
    assert!(SessionEventLog::open(root.path(), "directory-link").is_err());
    assert!(!outside.path().join("journal").exists());
}

#[cfg(unix)]
#[test]
fn session_log_fifo_child() {
    let Some(root) = std::env::var_os("ROTTWEILER_TEST_FIFO_SESSION_ROOT") else {
        return;
    };
    let result = SessionEventLog::open(std::path::Path::new(&root), "fifo");
    assert!(matches!(
        result,
        Err(super::SessionStoreError::UnsafeEventFileType)
    ));
}

#[cfg(unix)]
#[test]
fn fifo_event_log_is_rejected_in_a_non_hanging_subprocess() {
    use std::{process::Command, thread, time::Duration};

    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let directory = root.path().join("sessions/fifo/journal");
    std::fs::create_dir_all(&directory)
        .unwrap_or_else(|error| panic!("session directory must create: {error}"));
    assert!(
        Command::new("mkfifo")
            .arg(directory.join("active.jsonl"))
            .status()
            .unwrap_or_else(|error| panic!("mkfifo must run: {error}"))
            .success()
    );
    let executable = std::env::current_exe()
        .unwrap_or_else(|error| panic!("test executable must resolve: {error}"));
    let mut child = Command::new(executable)
        .arg("--exact")
        .arg("session::tests::journal_append::session_log_fifo_child")
        .arg("--nocapture")
        .env("ROTTWEILER_TEST_FIFO_SESSION_ROOT", root.path())
        .spawn()
        .unwrap_or_else(|error| panic!("FIFO test child must spawn: {error}"));
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child
            .try_wait()
            .unwrap_or_else(|error| panic!("FIFO test child must poll: {error}"))
        {
            assert!(status.success(), "FIFO test child failed: {status}");
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("opening a FIFO event log blocked for more than three seconds");
        }
        thread::sleep(Duration::from_millis(10));
    }
}
