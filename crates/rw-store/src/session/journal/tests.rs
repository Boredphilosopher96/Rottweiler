#![allow(clippy::expect_used)]
use super::*;
use serde_json::{Value, json};
use tempfile::tempdir;

fn page_limits(events: usize) -> SessionEventPageLimits {
    SessionEventPageLimits {
        max_page_events: events,
        ..SessionEventPageLimits::default()
    }
}

#[test]
fn catalog_growth_keeps_existing_entries_and_prefix_queries_stable() {
    let catalog = SegmentCatalog::default();
    let segment = |index| Segment {
        first: index,
        next: index + 1,
        bytes: 1,
        digest: blake3::hash(&index.to_le_bytes()),
        name: index.to_string(),
    };
    catalog.push(segment(0));
    let pointer = {
        let entries = catalog.entries.read().expect("catalog");
        std::ptr::from_ref(&entries[0]) as usize
    };
    let first = catalog.prefix(1);
    for index in 1..16_384 {
        catalog.push(segment(index));
    }
    assert_eq!(catalog.prefix(1), first);
    assert_eq!(catalog.prefix(16_384).0, 16_384);
    assert_eq!(catalog.partition(128, |entry| entry.next <= 97), 97);
    assert_eq!(catalog.partition(128, |entry| entry.next <= 20_000), 128);
    let entries = catalog.entries.read().expect("catalog");
    assert_eq!(std::ptr::from_ref(&entries[0]) as usize, pointer);
    assert_eq!(entries.chunks.len(), 64);
}

#[test]
fn views_pin_their_tail_across_append_rotation_and_writer_reopen() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "rotation").expect("journal");
    journal
        .append_batch([json!({"text":"first"})])
        .expect("first append");
    let first = journal.read_view();
    journal
        .append_batch([json!({"text":"x".repeat(SEGMENT_TARGET_BYTES)})])
        .expect("rotating append");
    let second = journal.read_view();
    assert_eq!(first.last_sequence(), Some(SequenceId(0)));
    assert_eq!(
        first
            .page::<Value>(None, page_limits(10))
            .expect("old page")
            .events
            .len(),
        1
    );
    assert_eq!(
        second
            .page::<Value>(Some(SequenceId(0)), page_limits(10))
            .expect("new page")
            .events
            .len(),
        1
    );
    assert_eq!(second.verify_all().expect("verify").events, 2);
    drop(journal);
    let mut reopened = SegmentedJournal::open(root.path(), "rotation")
        .expect("reopen without read-view writer lock");
    reopened
        .append_batch([json!({"text":"third"})])
        .expect("third append");
    assert_eq!(second.verify_all().expect("pinned verification").events, 2);
    assert_eq!(
        reopened
            .read_view()
            .verify_all()
            .expect("latest verification")
            .events,
        3
    );
}

#[test]
fn pages_are_bounded_and_cursor_exclusive() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "pages").expect("journal");
    journal
        .append_batch((0..200).map(|value| json!({"value":value})))
        .expect("append");
    let view = journal.read_view();
    let page = view
        .page::<Value>(Some(SequenceId(178)), page_limits(10))
        .expect("page");
    assert_eq!(page.events.len(), 10);
    assert_eq!(page.events[0].sequence, SequenceId(179));
    assert_eq!(page.next_cursor, Some(SequenceId(188)));
    assert_eq!(page.events_before_page, 179);
    assert_eq!(page.events_after_page, 11);
    assert!(page.has_more);
    let tail = view
        .page::<Value>(Some(SequenceId(199)), page_limits(10))
        .expect("tail");
    assert!(tail.events.is_empty());
    assert!(!tail.has_more);
    assert!(matches!(
        view.page::<Value>(Some(SequenceId(200)), page_limits(10)),
        Err(SessionStoreError::EventPageCursorAhead)
    ));
    assert!(matches!(
        view.page::<Value>(None, page_limits(0)),
        Err(SessionStoreError::InvalidEventPageLimits)
    ));
    let tiny = SessionEventPageLimits {
        max_page_bytes: 1,
        ..page_limits(10)
    };
    assert!(matches!(
        view.page::<Value>(None, tiny),
        Err(SessionStoreError::EventPageByteLimitTooSmall { .. })
    ));
}

#[test]
fn referenced_segment_work_respects_scan_budgets_and_reports_actual_io() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "budgets").expect("journal");
    journal
        .append_batch((0..200).map(|value| json!({"value":value})))
        .expect("append");
    let view = journal.read_view();
    let (page, metrics) = view
        .page_with_metrics::<Value>(Some(SequenceId(198)), page_limits(1))
        .expect("tail");
    assert_eq!(page.events.len(), 1);
    assert_eq!(
        metrics,
        JournalReadMetrics {
            bytes_read: view.total_bytes,
            records_scanned: 200,
            records_decoded: 1,
            segments_read: 1,
        }
    );
    let (_, empty_metrics) = view
        .page_with_metrics::<Value>(Some(SequenceId(199)), page_limits(1))
        .expect("empty tail");
    assert_eq!(empty_metrics, JournalReadMetrics::default());
    assert!(matches!(
        view.page::<Value>(
            Some(SequenceId(198)),
            SessionEventPageLimits {
                max_scan_bytes: view.total_bytes - 1,
                ..page_limits(1)
            }
        ),
        Err(SessionStoreError::EventScanBytesExceeded { .. })
    ));
    assert!(matches!(
        view.page::<Value>(
            Some(SequenceId(198)),
            SessionEventPageLimits {
                max_scan_events: 199,
                ..page_limits(1)
            }
        ),
        Err(SessionStoreError::EventScanCountExceeded { .. })
    ));
    assert!(matches!(
        view.page::<Value>(
            None,
            SessionEventPageLimits {
                max_scan_events: 0,
                ..page_limits(1)
            }
        ),
        Err(SessionStoreError::InvalidEventPageLimits)
    ));
}

#[test]
fn rejected_batch_leaves_the_committed_prefix_and_writer_usable() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "oversized").expect("journal");
    journal.append_batch([json!("first")]).expect("append");
    let identity = journal.read_view().prefix_identity();
    assert!(matches!(
        journal.append_batch([json!("x".repeat(MAX_SEGMENT_BYTES))]),
        Err(SessionStoreError::EventRecordTooLarge { .. })
    ));
    assert_eq!(journal.read_view().prefix_identity(), identity);
    journal
        .append_batch([json!("next")])
        .expect("usable after rejection");
    assert_eq!(journal.read_view().verify_all().expect("verify").events, 2);
}

#[test]
fn segment_publication_never_overwrites_a_collision_and_poison_is_explicit() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "collision").expect("journal");
    journal.append_batch([json!("first")]).expect("append");
    let name = Segment::name(0, 1, journal.active_bytes, journal.active_hash.finalize());
    let collision = journal.path().join(name);
    fs::write(&collision, b"existing").expect("collision");
    assert!(journal.seal().is_err());
    assert_eq!(fs::read(collision).expect("read collision"), b"existing");
    assert!(matches!(
        journal.append_batch([json!("second")]),
        Err(SessionStoreError::EventWriterPoisoned)
    ));
}

#[test]
fn prefix_identity_survives_rotation_and_recovery_after_active_rename() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "publication").expect("journal");
    journal.append_batch([json!("first")]).expect("append");
    let identity = journal.read_view().prefix_identity();
    journal.seal().expect("seal");
    assert_eq!(journal.read_view().prefix_identity(), identity);
    journal.append_batch([json!("second")]).expect("append");
    let expected = journal.read_view().prefix_identity();
    let name = Segment::name(
        journal.active_first,
        journal.next_sequence,
        journal.active_bytes,
        journal.active_hash.finalize(),
    );
    journal
        .directory
        .rename("active.jsonl", &name)
        .expect("simulate crash after publication before new active");
    drop(journal);
    let mut recovered = SegmentedJournal::open(root.path(), "publication").expect("recover");
    assert_eq!(recovered.read_view().prefix_identity(), expected);
    recovered.append_batch([json!("third")]).expect("continue");
    assert_eq!(
        recovered.read_view().verify_all().expect("verify").events,
        3
    );
}

#[test]
#[ignore = "writes 1M events; run explicitly for storage scaling evidence"]
fn journal_tail_read_scaling_metrics() {
    use std::time::Instant;
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "scaling").expect("journal");
    let mut count = 0;
    for target in [10_000_u64, 100_000, 1_000_000] {
        while count < target {
            let end = (count + 1_000).min(target);
            journal
                .append_batch(
                    (count..end)
                        .map(|value| json!({"value":value,"message":"bounded tail fixture"})),
                )
                .expect("batch");
            count = end;
        }
        let identity = journal.read_view().prefix_identity();
        let active_bytes = journal.active_bytes;
        let segment_count = journal.segments.len() + 1;
        drop(journal);
        let started = Instant::now();
        journal = SegmentedJournal::open(root.path(), "scaling").expect("reopen");
        let open_micros = started.elapsed().as_micros();
        assert_eq!(journal.read_view().prefix_identity(), identity);
        let view_started = Instant::now();
        let view = journal.read_view();
        let capture_nanos = view_started.elapsed().as_nanos();
        let read_started = Instant::now();
        let (page, metrics) = view
            .page_with_metrics::<Value>(Some(SequenceId(target - 101)), page_limits(100))
            .expect("tail");
        let read_micros = read_started.elapsed().as_micros();
        assert_eq!(page.events.len(), 100);
        assert_eq!(page.events[0].sequence, SequenceId(target - 100));
        assert!(!page.has_more);
        assert_eq!(metrics.records_decoded, 100);
        assert!(metrics.bytes_read <= 2 * MAX_SEGMENT_BYTES as u64);
        assert!(metrics.segments_read <= 2);
        println!(
            "{}",
            json!({
                "events":target,"total_bytes":view.total_bytes,"segments":segment_count,
                "tail_bytes_read":metrics.bytes_read,"tail_records_scanned":metrics.records_scanned,
                "tail_records_decoded":metrics.records_decoded,"tail_segments_read":metrics.segments_read,
                "page_bytes":page.page_bytes,"open_active_bytes":active_bytes,
                "open_micros":open_micros,"capture_nanos":capture_nanos,"tail_read_micros":read_micros,
                "scope":"store only; open excludes engine projection; timings are diagnostic, not calibrated gates"
            })
        );
    }
}

#[test]
fn offline_views_capture_only_unowned_journals_and_release_ownership_before_reading() {
    let root = tempdir().expect("root");
    assert!(
        JournalReadView::open_existing(root.path(), "absent")
            .expect("absent")
            .is_none()
    );
    let mut journal = SegmentedJournal::open(root.path(), "offline").expect("journal");
    journal.append_batch([json!("first")]).expect("append");
    let identity = journal.read_view().prefix_identity();
    assert!(JournalReadView::open_existing(root.path(), "offline").is_err());
    drop(journal);
    let view = JournalReadView::open_existing(root.path(), "offline")
        .expect("capture")
        .expect("exists");
    assert_eq!(view.prefix_identity(), identity);
    let mut reopened =
        SegmentedJournal::open(root.path(), "offline").expect("capture releases ownership");
    reopened.append_batch([json!("second")]).expect("append");
    assert_eq!(view.verify_all().expect("verify offline view").events, 1);
    assert_eq!(
        view.page::<Value>(None, page_limits(10))
            .expect("page")
            .events
            .len(),
        1
    );
}

#[test]
fn historical_prefix_reopens_after_growth_rotation_and_writer_restart() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "historical").expect("journal");
    let empty = journal.read_view().prefix_identity();
    journal.append_batch([json!("first")]).expect("first");
    let first = journal.read_view().prefix_identity();
    journal.append_batch([json!("second")]).expect("second");
    let second = journal.read_view().prefix_identity();
    journal
        .append_batch([json!("x".repeat(SEGMENT_TARGET_BYTES))])
        .expect("rotate");
    let third = journal.read_view().prefix_identity();
    journal
        .append_batch([json!("fourth")])
        .expect("rotate again");
    drop(journal);
    let view = JournalReadView::open_existing(root.path(), "historical")
        .expect("offline")
        .expect("exists");
    for identity in [empty, first, second, third, view.prefix_identity()] {
        let prefix = view.at_prefix(identity).expect("historical prefix");
        assert_eq!(prefix.prefix_identity(), identity);
        assert_eq!(
            prefix.verify_all().expect("verify").events,
            identity.next_sequence
        );
        assert_eq!(
            prefix
                .page::<Value>(None, page_limits(10))
                .expect("page")
                .events
                .len() as u64,
            identity.next_sequence
        );
    }
    let mut changed = first;
    changed.digest[0] ^= 1;
    assert!(view.at_prefix(changed).is_err());
    changed.next_sequence = 5;
    assert!(matches!(
        view.at_prefix(changed),
        Err(SessionStoreError::EventPageCursorAhead)
    ));
    let first_segment =
        journal_path(root.path(), "historical").join(&view.segments.get(0).expect("segment").name);
    let mut bytes = fs::read(&first_segment).expect("read");
    bytes[0] ^= 1;
    fs::write(first_segment, bytes).expect("corrupt boundary");
    assert!(view.at_prefix(first).is_err());
}

fn journal_path(root: &Path, session: &str) -> PathBuf {
    root.join("sessions").join(session).join("journal")
}

#[test]
fn derived_indexes_have_an_independent_directory_and_do_not_change_raw_identity() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "derived").expect("journal");
    journal.append_batch([json!("first")]).expect("append");
    let view = journal.read_view();
    let identity = view.prefix_identity();
    journal
        .derived_directory()
        .expect("writer derived directory");
    fs::write(
        journal.path().join("derived/transcript.index"),
        b"derived projection",
    )
    .expect("index");
    drop(journal);
    let offline = JournalReadView::open_existing(root.path(), "derived")
        .expect("capture")
        .expect("exists");
    offline
        .derived_directory()
        .expect("offline derived directory without raw writer");
    let reopened =
        SegmentedJournal::open(root.path(), "derived").expect("independent raw ownership");
    assert_eq!(reopened.read_view().prefix_identity(), identity);
    assert_eq!(view.verify_all().expect("raw integrity").events, 1);
}

#[test]
fn old_bitrot_is_detected_on_access_or_explicit_full_verification() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "bitrot").expect("journal");
    journal
        .append_batch([json!({"text":"x".repeat(SEGMENT_TARGET_BYTES)})])
        .expect("first append");
    journal
        .append_batch([json!({"text":"latest"})])
        .expect("seal old segment");
    let view = journal.read_view();
    let old = journal
        .path()
        .join(&view.segments.get(0).expect("segment").name);
    let mut bytes = fs::read(&old).expect("read old segment");
    bytes[0] = b'[';
    fs::write(old, bytes).expect("simulate old bitrot");
    assert_eq!(
        view.page::<Value>(Some(SequenceId(0)), page_limits(1))
            .expect("unrelated tail")
            .events
            .len(),
        1
    );
    assert!(view.page::<Value>(None, page_limits(1)).is_err());
    assert!(view.verify_all().is_err());
}

#[test]
fn incomplete_active_tail_repairs_but_complete_corruption_fails() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "repair").expect("journal");
    journal
        .append_batch([json!({"first":true})])
        .expect("append");
    let active = journal.path().join("active.jsonl");
    let length = fs::metadata(&active).expect("metadata").len();
    drop(journal);
    fs::OpenOptions::new()
        .append(true)
        .open(&active)
        .expect("active")
        .write_all(b"{partial")
        .expect("torn append");
    let repaired = SegmentedJournal::open(root.path(), "repair").expect("repair");
    assert_eq!(repaired.next_sequence(), 1);
    assert_eq!(fs::metadata(&active).expect("metadata").len(), length);
    drop(repaired);
    fs::OpenOptions::new()
        .append(true)
        .open(&active)
        .expect("active")
        .write_all(b"{invalid}\n")
        .expect("corrupt complete record");
    assert!(SegmentedJournal::open(root.path(), "repair").is_err());
}

#[test]
fn single_writer_lock_is_independent_of_segment_rotation() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "writer").expect("journal");
    journal
        .append_batch([json!({"text":"x".repeat(SEGMENT_TARGET_BYTES)})])
        .expect("append");
    journal
        .append_batch([json!({"text":"next"})])
        .expect("rotate");
    assert!(SegmentedJournal::open(root.path(), "writer").is_err());
}

#[cfg(unix)]
#[test]
fn unsafe_segment_descriptors_and_pinned_active_mutation_fail_closed() {
    use std::os::unix::fs::symlink;
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "unsafe").expect("journal");
    journal
        .append_batch([json!({"text":"x".repeat(SEGMENT_TARGET_BYTES)})])
        .expect("first append");
    journal
        .append_batch([json!({"text":"active"})])
        .expect("rotate");
    let view = journal.read_view();
    let sealed = journal
        .path()
        .join(&view.segments.get(0).expect("segment").name);
    let backup = root.path().join("backup");
    fs::rename(&sealed, &backup).expect("move segment");
    symlink(&backup, &sealed).expect("symlink");
    assert!(view.page::<Value>(None, page_limits(1)).is_err());
    fs::remove_file(&sealed).expect("unlink symlink");
    fs::hard_link(&backup, &sealed).expect("hardlink");
    assert!(view.page::<Value>(None, page_limits(1)).is_err());
    fs::write(journal.path().join("active.jsonl"), b"changed\n").expect("mutate active");
    assert!(
        view.page::<Value>(Some(SequenceId(0)), page_limits(1))
            .is_err()
    );
}
#[test]
fn unsupported_lifetime_layout_is_rejected_without_creating_an_empty_journal() {
    let root = tempdir().expect("root");
    let session = root.path().join("sessions/legacy");
    fs::create_dir_all(&session).expect("session");
    let original = b"{\"schema_version\":1,\"sequence\":\"0\",\"event\":{}}\n";
    fs::write(session.join("events.jsonl"), original).expect("old layout");
    assert!(matches!(
        SegmentedJournal::open(root.path(), "legacy"),
        Err(SessionStoreError::UnsupportedJournalLayout)
    ));
    assert!(matches!(
        JournalReadView::open_existing(root.path(), "legacy"),
        Err(SessionStoreError::UnsupportedJournalLayout)
    ));
    assert!(!session.join("journal").exists());
    assert_eq!(
        fs::read(session.join("events.jsonl")).expect("unchanged"),
        original
    );
}
#[test]
fn cursor_prefix_matches_captured_identity_after_growth_and_rotation() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "cursor-prefix").expect("journal");
    journal
        .append_batch([json!({"payload": "x".repeat(1024 * 1024)})])
        .expect("first");
    let first = journal.read_view();
    journal
        .append_batch([json!({"payload": "second"})])
        .expect("rotate");
    let grown = journal.read_view();
    let prefix = grown
        .prefix_through(Some(SequenceId(0)))
        .expect("cursor prefix");
    assert_eq!(prefix.prefix_identity(), first.prefix_identity());
    assert!(Arc::ptr_eq(&prefix.segments, &grown.segments));
    assert!(Arc::ptr_eq(&first.segments, &grown.segments));
    assert_eq!(prefix.last_sequence(), Some(SequenceId(0)));
    assert_eq!(
        grown.prefix_through(None).expect("empty").prefix_identity(),
        JournalPrefixIdentity::empty()
    );
    assert!(matches!(
        grown.prefix_through(Some(SequenceId(2))),
        Err(SessionStoreError::EventPageCursorAhead)
    ));
    assert!(matches!(
        grown.prefix_through(Some(SequenceId(u64::MAX))),
        Err(SessionStoreError::SequenceOverflow)
    ));
}
