#![allow(clippy::expect_used)]

use super::*;
use serde_json::{Value, json};
use std::os::unix::fs::FileExt as _;
use tempfile::tempdir;

fn limits(count: usize) -> SessionEventPageLimits {
    SessionEventPageLimits {
        max_page_events: count,
        ..SessionEventPageLimits::default()
    }
}

#[test]
fn verified_page_covers_initial_and_every_processed_cut_without_rereading() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "cuts").expect("journal");
    journal.append_batch(0..20).expect("append");
    let view = journal.read_view();
    let verified = view
        .verified_page::<u64>(Some(SequenceId(4)), limits(10))
        .expect("page");
    assert_eq!(verified.page.events.len(), 10);
    assert_eq!(verified.metrics.segments_read, 1);
    assert_eq!(verified.metrics.bytes_read, view.total_bytes());
    let previous = view
        .prefix_through(Some(SequenceId(4)))
        .expect("previous")
        .prefix_identity();
    for sequence in 4..15 {
        let expected = view
            .prefix_through(Some(SequenceId(sequence)))
            .expect("expected");
        let advance = verified
            .proof
            .advance(previous, Some(SequenceId(sequence)))
            .expect("proof");
        assert_eq!(advance.next().prefix_identity(), expected.prefix_identity());
        assert_eq!(advance.next().total_bytes(), expected.total_bytes());
        assert_eq!(
            advance
                .next()
                .page::<u64>(None, limits(30))
                .expect("read cut")
                .events
                .len(),
            usize::try_from(sequence).expect("small cursor") + 1
        );
    }
    assert!(verified.proof.prefix_through(Some(SequenceId(3))).is_err());
    assert!(verified.proof.prefix_through(Some(SequenceId(15))).is_err());
    let later = verified
        .proof
        .prefix_through(Some(SequenceId(10)))
        .expect("later");
    assert!(
        verified
            .proof
            .advance(later.prefix_identity(), Some(SequenceId(9)))
            .is_err()
    );
    let mut wrong = previous;
    wrong.digest[0] ^= 1;
    assert!(verified.proof.verify_prefix(wrong).is_err());
}

#[test]
fn empty_tail_proof_preserves_full_origin_with_no_payload_io() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "tail").expect("journal");
    let empty = journal
        .read_view()
        .verified_page::<u64>(None, limits(10))
        .expect("empty");
    assert!(empty.page.events.is_empty());
    assert_eq!(empty.metrics.bytes_read, 0);
    assert_eq!(
        empty
            .proof
            .prefix_through(None)
            .expect("empty cut")
            .prefix_identity(),
        JournalPrefixIdentity::empty()
    );
    journal.append_batch(0..20).expect("append");
    let view = journal.read_view();
    let tail = view
        .verified_page::<u64>(view.last_sequence(), limits(10))
        .expect("tail");
    assert!(tail.page.events.is_empty());
    assert_eq!(tail.metrics.bytes_read, 0);
    let recovered = tail
        .proof
        .prefix_through(view.last_sequence())
        .expect("tail cut");
    assert_eq!(recovered.prefix_identity(), view.prefix_identity());
    assert_eq!(
        recovered
            .page::<u64>(None, limits(30))
            .expect("retained origin")
            .events
            .len(),
        20
    );
}

#[test]
fn proof_event_and_descriptor_bounds_return_truthful_partial_pages() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "events").expect("journal");
    journal.append_batch(0..300).expect("append");
    let page = journal
        .read_view()
        .verified_page::<u64>(None, limits(300))
        .expect("bounded page");
    assert_eq!(page.page.events.len(), 256);
    assert_eq!(page.page.next_cursor, Some(SequenceId(255)));
    assert!(page.page.has_more);
    let mut segments = SegmentedJournal::open(root.path(), "segments").expect("segments");
    for n in 0..10 {
        segments
            .append_batch([json!({"n":n,"body":"x".repeat(SEGMENT_TARGET_BYTES)})])
            .expect("segment");
    }
    let page = segments
        .read_view()
        .verified_page::<Value>(
            None,
            SessionEventPageLimits {
                max_page_events: 20,
                max_page_bytes: 16 * 1024 * 1024,
                max_scan_bytes: 16 * 1024 * 1024,
                ..limits(20)
            },
        )
        .expect("descriptor bound");
    assert_eq!(page.metrics.segments_read, 8);
    assert_eq!(page.page.events.len(), 8);
    assert!(page.page.has_more);
    assert_eq!(
        page.proof
            .prefix_through(page.page.next_cursor)
            .expect("last cut")
            .last_sequence(),
        Some(SequenceId(7))
    );
}

#[test]
fn page_proof_retains_descriptor_across_rotation_and_detects_later_corruption_on_read() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "pin").expect("journal");
    journal.append_batch(0..10).expect("append");
    let page = journal
        .read_view()
        .verified_page::<u64>(None, limits(5))
        .expect("page");
    journal
        .append_batch([json!({"body":"x".repeat(SEGMENT_TARGET_BYTES)})])
        .expect("rotate");
    drop(journal);
    let advance = page
        .proof
        .advance(JournalPrefixIdentity::empty(), Some(SequenceId(4)))
        .expect("advance after rotation");
    assert_eq!(
        advance
            .next()
            .page::<u64>(None, limits(10))
            .expect("pinned")
            .events
            .len(),
        5
    );
    // Corruption after validation does not cause another proof I/O pass. Any read
    // through its retained descriptor still verifies the pinned checksum.
    advance
        .next()
        .active
        .write_at(b"!", 0)
        .expect("corrupt pinned descriptor");
    assert!(
        page.proof
            .advance(JournalPrefixIdentity::empty(), Some(SequenceId(4)))
            .is_ok()
    );
    assert!(advance.next().page::<u64>(None, limits(10)).is_err());
}
