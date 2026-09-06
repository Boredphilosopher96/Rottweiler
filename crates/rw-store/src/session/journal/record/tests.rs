#![allow(clippy::expect_used)]
use super::*;
use crate::session::journal::{SEGMENT_TARGET_BYTES, SegmentedJournal};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

static DECODE_CALLS: AtomicUsize = AtomicUsize::new(0);
#[derive(Debug)]
struct Observed;
impl DecodeAllocation for Observed {
    fn decode_node_bytes() -> Option<usize> {
        Value::decode_node_bytes()
    }
}
impl<'de> Deserialize<'de> for Observed {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        DECODE_CALLS.fetch_add(1, Ordering::SeqCst);
        Value::deserialize(deserializer)?;
        Ok(Self)
    }
}

#[test]
fn remaining_allowance_is_checked_before_any_typed_deserialization() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "remaining").expect("journal");
    journal
        .append_batch([json!({"items":vec![Value::Null; 128]})])
        .expect("append");
    let view = journal.read_view();
    let admitted = view
        .record_with_decode_limit::<Value>(SequenceId(0), MAX_JOURNAL_DECODE_BYTES)
        .expect("read");
    let remaining = admitted.decode_bytes - 1;
    DECODE_CALLS.store(0, Ordering::SeqCst);
    assert!(
        matches!(view.record_with_decode_limit::<Observed>(SequenceId(0), remaining),
        Err(SessionStoreError::EventDecodeLimitTooSmall { required_bytes, max_bytes })
        if required_bytes == admitted.decode_bytes && max_bytes == remaining)
    );
    assert_eq!(DECODE_CALLS.load(Ordering::SeqCst), 0);
    view.record_with_decode_limit::<Observed>(SequenceId(0), admitted.decode_bytes)
        .expect("exact allowance");
    assert_eq!(DECODE_CALLS.load(Ordering::SeqCst), 1);
    for invalid in [0, MAX_JOURNAL_DECODE_BYTES + 1] {
        assert!(matches!(
            view.record_with_decode_limit::<Value>(SequenceId(0), invalid),
            Err(SessionStoreError::InvalidEventDecodeLimit)
        ));
    }
}

#[test]
fn exact_reads_keep_pinned_prefix_and_inspect_one_containing_segment() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "indexed").expect("journal");
    journal.append_batch([json!({"value":0})]).expect("append");
    let pinned = journal.read_view();
    for value in 1..5 {
        journal
            .append_batch([json!({"value":value,"body":"x".repeat(SEGMENT_TARGET_BYTES)})])
            .expect("rotate");
    }
    let record = pinned
        .record_with_decode_limit::<Value>(SequenceId(0), MAX_JOURNAL_DECODE_BYTES)
        .expect("pinned read");
    assert_eq!(record.envelope.sequence, SequenceId(0));
    assert_eq!(record.metrics.records_decoded, 1);
    assert!(matches!(
        pinned.record_with_decode_limit::<Value>(SequenceId(1), MAX_JOURNAL_DECODE_BYTES),
        Err(SessionStoreError::EventPageCursorAhead)
    ));
    for sequence in 0..5 {
        let record = journal
            .read_view()
            .record_with_decode_limit::<Value>(SequenceId(sequence), MAX_JOURNAL_DECODE_BYTES)
            .expect("exact read");
        assert_eq!(record.envelope.sequence, SequenceId(sequence));
        assert_eq!(record.metrics.records_decoded, 1);
        assert_eq!(record.metrics.segments_read, 1);
        assert!(record.metrics.bytes_read <= MAX_SEGMENT_BYTES as u64);
    }
}

#[test]
fn source_read_rejects_changed_bytes_in_its_containing_segment() {
    use std::os::unix::fs::FileExt as _;
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "changed").expect("journal");
    journal.append_batch([json!({"value":0})]).expect("append");
    let view = journal.read_view();
    view.active.write_at(b"!", 0).expect("corrupt descriptor");
    assert!(
        view.record_with_decode_limit::<Value>(SequenceId(0), MAX_JOURNAL_DECODE_BYTES)
            .is_err()
    );
}
