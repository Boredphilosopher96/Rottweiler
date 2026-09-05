#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{JournalAppendPlan, MAX_SEGMENT_BYTES};
use crate::session::{SessionEventLog, SessionEventPageLimits, SessionStoreError};
use rw_types::SequenceId;
use serde::Serialize;
use serde_json::{Value, json};
use std::cell::Cell;

#[test]
fn planned_encoding_matches_the_public_envelopes_and_publishes_after_append() {
    let root = tempfile::tempdir().expect("root");
    let mut log = SessionEventLog::open(root.path(), "prepared").expect("log");
    let events = vec![json!({"text":"first\n\""}), json!({"text":"second"})];
    let plan = JournalAppendPlan::measure(SequenceId(0), &events).expect("plan");
    let prepared = plan.encode(&events).expect("encode");
    assert_eq!(prepared.encoded_bytes(), plan.encoded_bytes());
    assert_eq!(prepared.retained_bytes(), prepared.encoded_bytes());
    let empty = log.read_view();
    assert_eq!(empty.last_sequence(), None);
    log.append_prepared(prepared).expect("durable append");
    assert_eq!(empty.last_sequence(), None);
    assert_eq!(log.last_sequence(), Some(SequenceId(1)));
    let page = log
        .read_view()
        .page::<Value>(None, SessionEventPageLimits::default())
        .expect("page");
    assert_eq!(
        page.events
            .into_iter()
            .map(|envelope| envelope.event)
            .collect::<Vec<_>>(),
        events
    );
    drop(log);
    let reopened = SessionEventLog::open(root.path(), "prepared").expect("reopen");
    assert_eq!(reopened.last_sequence(), Some(SequenceId(1)));
}

#[test]
fn changed_writer_prefix_rejects_before_any_bytes_or_identity_change() {
    let root = tempfile::tempdir().expect("root");
    let mut log = SessionEventLog::open(root.path(), "prepared").expect("log");
    let events = [json!("stale")];
    let pending = JournalAppendPlan::measure(SequenceId(0), &events)
        .expect("plan")
        .encode(&events)
        .expect("encode");
    log.append(json!("committed")).expect("advance");
    let before = log.read_view();
    assert!(matches!(
        log.append_prepared(pending),
        Err(SessionStoreError::UnexpectedEventSequence { .. })
    ));
    assert_eq!(log.read_view().prefix_identity(), before.prefix_identity());
    assert_eq!(log.read_view().total_bytes(), before.total_bytes());
}

#[test]
fn plan_bounds_escaped_bytes_and_rejects_sequence_overflow() {
    assert!(JournalAppendPlan::measure(SequenceId(u64::MAX), &[json!(0)]).is_err());
    assert!(
        JournalAppendPlan::measure(SequenceId(0), &[json!("\0".repeat(MAX_SEGMENT_BYTES / 5))])
            .is_err()
    );
}

#[test]
fn changed_serialization_cannot_grow_the_reserved_buffer() {
    struct Changing(Cell<bool>);
    impl Serialize for Changing {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            if self.0.replace(true) {
                serializer.serialize_str("larger-than-measured")
            } else {
                serializer.serialize_str("x")
            }
        }
    }
    let events = [Changing(Cell::new(false))];
    let plan = JournalAppendPlan::measure(SequenceId(0), &events).expect("measure short");
    assert!(plan.encode(&events).is_err());
}

#[test]
fn changed_event_count_rejects_before_encoding() {
    let events = [json!(0), json!(1)];
    let plan = JournalAppendPlan::measure(SequenceId(0), &events).expect("plan");
    assert!(plan.encode(&events[..1]).is_err());
}

#[test]
fn prepared_write_rejects_changed_descriptors() {
    let root = tempfile::tempdir().expect("root");
    let mut log = SessionEventLog::open(root.path(), "prepared").expect("log");
    log.append(json!("initial")).expect("append");
    let events = [json!("next")];
    let pending = JournalAppendPlan::measure(SequenceId(1), &events)
        .expect("plan")
        .encode(&events)
        .expect("encode");
    std::fs::write(log.path().join("active.jsonl"), b"replaced\n").expect("corrupt opened file");
    assert!(log.append_prepared(pending).is_err());
    assert_eq!(log.last_sequence(), Some(SequenceId(0)));
}

#[test]
fn partial_prepared_write_restores_the_committed_prefix_and_allows_retry() {
    let root = tempfile::tempdir().expect("root");
    let mut log = SessionEventLog::open(root.path(), "prepared").expect("log");
    log.append(json!("initial")).expect("initial");
    let before = log.read_view();
    let bytes = std::fs::read(log.path().join("active.jsonl")).expect("bytes");
    let events = [json!("retry")];
    let plan = JournalAppendPlan::measure(SequenceId(1), &events).expect("plan");
    let fault = crate::session::install_append_fault(7, false);
    assert!(matches!(
        log.append_prepared(plan.encode(&events).expect("encode")),
        Err(SessionStoreError::Io(_))
    ));
    assert_eq!(log.read_view().prefix_identity(), before.prefix_identity());
    assert_eq!(
        std::fs::read(log.path().join("active.jsonl")).expect("restored"),
        bytes
    );
    drop(fault);
    log.append_prepared(plan.encode(&events).expect("retry encoding"))
        .expect("retry");
    drop(log);
    let reopened = SessionEventLog::open(root.path(), "prepared").expect("reopen");
    assert_eq!(reopened.last_sequence(), Some(SequenceId(1)));
    assert_eq!(
        reopened
            .load::<Value>()
            .expect("read")
            .into_iter()
            .map(|e| e.event)
            .collect::<Vec<_>>(),
        vec![json!("initial"), json!("retry")]
    );
}
