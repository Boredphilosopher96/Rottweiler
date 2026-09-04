#![allow(clippy::expect_used)]

use super::super::journal::SegmentedJournal;
use super::*;
use tempfile::tempdir;

fn row(ordinal: u64) -> TranscriptIndexRow {
    TranscriptIndexRow {
        ordinal,
        key: format!("conversation:{ordinal}"),
        source: SequenceId(ordinal),
        revision: SequenceId(ordinal),
        agent_turn: Some(ordinal / 2),
        payload: format!("message {ordinal}").into_bytes(),
    }
}

fn populate(index: &mut TranscriptIndex, view: &JournalReadView, count: u64) {
    for start in (0..count).step_by(MAX_BATCH_ROWS) {
        let mutations = (start..count.min(start + MAX_BATCH_ROWS as u64))
            .map(|ordinal| TranscriptIndexMutation::Put(row(ordinal)))
            .collect::<Vec<_>>();
        let head = index.head().expect("head");
        index
            .apply(
                head.prefix,
                view,
                0,
                b"{}",
                start + (MAX_BATCH_ROWS as u64) < count,
                &mutations,
            )
            .expect("batch");
    }
}

#[test]
fn rejected_batch_rolls_back_rows_and_checkpoint_together() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "rollback").expect("journal");
    journal.append_batch(0..10).expect("events");
    let view = journal.read_view();
    let mut index = TranscriptIndex::open(&view, 1).expect("index");
    populate(&mut index, &view, 2);
    let before = index.head().expect("head");
    let mutations = [
        TranscriptIndexMutation::Put(row(2)),
        TranscriptIndexMutation::Put(row(7)),
    ];
    assert!(matches!(
        index.apply(before.prefix, &view, 0, b"uncommitted", false, &mutations),
        Err(TranscriptIndexError::Invalid("non-dense append"))
    ));
    assert_eq!(index.head().expect("unchanged head"), before);
    assert!(
        index
            .row("conversation:2")
            .expect("rolled back row")
            .is_none()
    );
    drop(index);
    assert_eq!(
        TranscriptIndex::open(&view, 1)
            .expect("reopen")
            .head()
            .expect("head"),
        before
    );
}

#[test]
#[ignore = "run with --release: redb debug builds intentionally walk the full index at open"]
fn qualify_10k_100k_index_work() {
    use std::time::Instant;
    let debug_build = std::hint::black_box(cfg!(debug_assertions));
    assert!(
        !debug_build,
        "qualify the production index open path with --release"
    );
    for count in [10_000_u64, 100_000] {
        let root = tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "qualification").expect("journal");
        for start in (0..count).step_by(1_000) {
            journal
                .append_batch(start..(start + 1_000).min(count))
                .expect("events");
        }
        let view = journal.read_view();
        let mut index = TranscriptIndex::open(&view, 1).expect("index");
        let started = Instant::now();
        populate(&mut index, &view, count);
        let build_ms = started.elapsed().as_secs_f64() * 1_000.0;
        drop(index);
        for first in [0, count / 2, count - 64] {
            let view = journal.read_view();
            let started = Instant::now();
            let mut index = TranscriptIndex::open(&view, 1).expect("cold index open");
            let open_ms = started.elapsed().as_secs_f64() * 1_000.0;
            let open_io = index.io_metrics();
            let started = Instant::now();
            let page = index.page(first, 64, MAX_PAGE_BYTES).expect("page");
            let page_ms = started.elapsed().as_secs_f64() * 1_000.0;
            let after = index.io_metrics();
            assert_eq!(page.rows.len(), 64);
            assert!(open_io.bytes_read < 1024 * 1024, "open read {open_io:?}");
            assert!(
                after.bytes_read - open_io.bytes_read < 128 * 1024,
                "page read {after:?}"
            );
            journal
                .append_batch(["late output"])
                .expect("durable revision");
            let updated_view = journal.read_view();
            let mut updated = row(first);
            updated.revision = updated_view.last_sequence().expect("revision");
            updated.payload = b"late output".to_vec();
            let started = Instant::now();
            index
                .apply(
                    view.prefix_identity(),
                    &updated_view,
                    0,
                    b"{}",
                    false,
                    &[TranscriptIndexMutation::Put(updated)],
                )
                .expect("update");
            let update_ms = started.elapsed().as_secs_f64() * 1_000.0;
            let update = index.io_metrics();
            assert!(
                update.bytes_written - after.bytes_written < 512 * 1024,
                "single row writes {update:?}"
            );
            eprintln!(
                "{}",
                serde_json::json!({"rows":count,"first":first,"build_ms":build_ms,"open_ms":open_ms,"open_bytes_read":open_io.bytes_read,"page_ms":page_ms,"page_bytes_read":after.bytes_read-open_io.bytes_read,"retained_page_bytes":page.retained_bytes,"update_ms":update_ms,"update_bytes_written":update.bytes_written-after.bytes_written,"update_syncs":update.syncs-after.syncs})
            );
        }
    }
}

#[test]
fn real_index_pages_seek_first_middle_tail_and_reopen_without_body_copy() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "pages").expect("journal");
    journal.append_batch(0..10_000).expect("events");
    let view = journal.read_view();
    let mut index = TranscriptIndex::open(&view, 1).expect("index");
    populate(&mut index, &view, 10_000);
    for first in [0, 5_000, 9_936] {
        let page = index.page(first, 64, MAX_PAGE_BYTES).expect("page");
        assert_eq!(page.head.total_rows, 10_000);
        assert_eq!(page.head.prefix, view.prefix_identity());
        assert_eq!(page.rows.len(), 64);
        assert_eq!(page.rows[0], row(first));
        assert_eq!(page.rows[63], row(first + 63));
        assert!(page.retained_bytes < 64 * 100);
    }
    assert_eq!(
        index.row("conversation:5432").expect("lookup"),
        Some(row(5_432))
    );
    drop(index);
    let reopened = TranscriptIndex::open(&view, 1).expect("reopened");
    assert_eq!(
        reopened.page(9_999, 1, MAX_PAGE_BYTES).expect("last").rows,
        [row(9_999)]
    );
}

#[test]
fn old_materialized_pages_keep_their_revision_after_late_update() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "revisions").expect("journal");
    journal.append_batch(0..5).expect("events");
    let old = journal.read_view();
    let mut index = TranscriptIndex::open(&old, 1).expect("index");
    populate(&mut index, &old, 5);
    let page = index.page(0, 5, MAX_PAGE_BYTES).expect("old page");
    journal.append_batch(["late output"]).expect("append");
    let next = journal.read_view();
    let mut revised = row(0);
    revised.revision = SequenceId(5);
    revised.payload = b"finished tool output".to_vec();
    index
        .apply(
            old.prefix_identity(),
            &next,
            0,
            b"{}",
            false,
            &[TranscriptIndexMutation::Put(revised.clone())],
        )
        .expect("update");
    assert_eq!(page.rows[0], row(0));
    assert_eq!(index.row("conversation:0").expect("updated"), Some(revised));
    assert_eq!(index.head().expect("head").generation, 0);
    let mut unversioned = row(0);
    unversioned.revision = SequenceId(5);
    assert!(matches!(
        index.apply(
            next.prefix_identity(),
            &next,
            0,
            b"{}",
            false,
            &[TranscriptIndexMutation::Put(unversioned)]
        ),
        Err(TranscriptIndexError::Invalid(
            "row identity/revision changed"
        ))
    ));
    assert!(matches!(
        index.apply(old.prefix_identity(), &next, 0, b"{}", false, &[]),
        Err(TranscriptIndexError::Stale)
    ));
}

#[test]
fn interrupted_transaction_discards_only_derived_state_and_rebuilds() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "interrupted").expect("journal");
    journal.append_batch(0..5).expect("events");
    let view = journal.read_view();
    let mut index = TranscriptIndex::open(&view, 1).expect("index");
    populate(&mut index, &view, 5);
    drop(index);
    let path = root
        .path()
        .join("sessions/interrupted/journal/derived/transcript.redb");
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("database")
        .set_len(47)
        .expect("torn database");
    assert!(TranscriptIndex::open(&view, 1).is_err());
    let rebuilt = TranscriptIndex::rebuild(&view, 1).expect("explicit rebuild");
    assert_eq!(rebuilt.head().expect("head").total_rows, 0);
    assert_eq!(
        rebuilt.head().expect("head").prefix,
        JournalPrefixIdentity::empty()
    );
    assert_eq!(view.verify_all().expect("raw verification").events, 5);
}

#[test]
fn allocation_limits_fail_before_mutation_and_index_lock_is_independent() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "limits").expect("journal");
    journal.append_batch([1]).expect("event");
    let view = journal.read_view();
    let mut index = TranscriptIndex::open(&view, 1).expect("index");
    assert!(matches!(
        TranscriptIndex::open(&view, 1),
        Err(TranscriptIndexError::Busy)
    ));
    let mut oversized = row(0);
    oversized.payload = vec![0; MAX_ROW_BYTES + 1];
    assert!(matches!(
        index.apply(
            JournalPrefixIdentity::empty(),
            &view,
            0,
            b"",
            false,
            &[TranscriptIndexMutation::Put(oversized)]
        ),
        Err(TranscriptIndexError::Limit(_))
    ));
    assert_eq!(index.head().expect("head").total_rows, 0);
    populate(&mut index, &view, 1);
    assert!(matches!(
        index.page(0, MAX_PAGE_ROWS + 1, MAX_PAGE_BYTES),
        Err(TranscriptIndexError::Limit(_))
    ));
    assert!(matches!(
        index.page(0, 1, 1),
        Err(TranscriptIndexError::Limit(_))
    ));
    journal
        .append_batch([2])
        .expect("raw writer proceeds while index is open");
}

#[test]
fn rebuild_hides_partial_order_and_only_publishes_dense_ordinals() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "rewind").expect("journal");
    journal.append_batch(0..5).expect("events");
    let view = journal.read_view();
    let mut index = TranscriptIndex::open(&view, 1).expect("index");
    populate(&mut index, &view, 5);
    index
        .apply(
            view.prefix_identity(),
            &view,
            1,
            b"repacking",
            true,
            &[TranscriptIndexMutation::Delete("conversation:2".into())],
        )
        .expect("start");
    assert!(matches!(
        index.page(0, 5, MAX_PAGE_BYTES),
        Err(TranscriptIndexError::Rebuilding)
    ));
    index
        .apply(
            view.prefix_identity(),
            &view,
            1,
            b"repacking",
            true,
            &[
                TranscriptIndexMutation::Move {
                    key: "conversation:3".into(),
                    ordinal: 2,
                },
                TranscriptIndexMutation::Move {
                    key: "conversation:4".into(),
                    ordinal: 3,
                },
            ],
        )
        .expect("bounded moves");
    index
        .apply(view.prefix_identity(), &view, 1, b"{}", false, &[])
        .expect("publish");
    let page = index.page(0, 5, MAX_PAGE_BYTES).expect("page");
    assert_eq!(page.head.total_rows, 4);
    assert_eq!(
        page.rows.iter().map(|row| row.ordinal).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    assert_eq!(page.rows[2].source, SequenceId(3));
}

#[test]
fn descriptor_pinning_and_link_rejection_protect_original_files() {
    use std::os::unix::fs::symlink;
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "paths").expect("journal");
    journal.append_batch([1]).expect("event");
    let view = journal.read_view();
    let directory = root.path().join("sessions/paths/journal/derived");
    let mut index = TranscriptIndex::open(&view, 1).expect("index");
    let database = directory.join("transcript.redb");
    std::fs::rename(&database, directory.join("pinned.redb")).expect("rename");
    std::fs::write(&database, b"leave this replacement alone").expect("replacement");
    populate(&mut index, &view, 1);
    assert_eq!(
        std::fs::read(&database).expect("replacement"),
        b"leave this replacement alone"
    );
    drop(index);
    std::fs::remove_file(&database).expect("remove replacement");
    symlink(directory.join("pinned.redb"), &database).expect("symlink");
    assert!(TranscriptIndex::open(&view, 1).is_err());
    std::fs::remove_file(&database).expect("remove symlink");
    std::fs::hard_link(directory.join("pinned.redb"), &database).expect("hard link");
    assert!(matches!(
        TranscriptIndex::open(&view, 1),
        Err(TranscriptIndexError::Invalid("unsafe index descriptor"))
    ));
}

#[test]
fn process_exit_during_index_transaction_never_publishes_partial_checkpoint() {
    const CHILD_ROOT: &str = "RW_TRANSCRIPT_INDEX_ABORT_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let view = JournalReadView::open_existing(std::path::Path::new(&root), "crash")
            .expect("view")
            .expect("session");
        let index = TranscriptIndex::open(&view, 1).expect("index");
        let transaction = index.database.begin_write().expect("transaction");
        let prefix = view.prefix_identity();
        transaction
            .open_table(HEAD)
            .expect("head")
            .insert(
                0,
                (
                    1,
                    99,
                    prefix.next_sequence,
                    prefix.digest.as_slice(),
                    b"uncommitted".as_slice(),
                    false,
                ),
            )
            .expect("update");
        std::process::exit(73);
    }
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "crash").expect("journal");
    journal.append_batch(0..5).expect("events");
    let view = journal.read_view();
    let mut index = TranscriptIndex::open(&view, 1).expect("index");
    populate(&mut index, &view, 5);
    let before = index.head().expect("head");
    drop(index);
    drop(journal);
    let name = format!(
        "{}::process_exit_during_index_transaction_never_publishes_partial_checkpoint",
        module_path!()
            .strip_prefix("rw_store::")
            .expect("test module")
    );
    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", &name, "--nocapture"])
        .env(CHILD_ROOT, root.path())
        .status()
        .expect("child");
    assert_eq!(status.code(), Some(73));
    match TranscriptIndex::open(&view, 1) {
        Ok(index) => assert_eq!(index.head().expect("head"), before),
        Err(TranscriptIndexError::Storage(redb::Error::RepairAborted)) => {
            let index = TranscriptIndex::rebuild(&view, 1).expect("explicit repair");
            assert_eq!(
                index.head().expect("head").prefix,
                JournalPrefixIdentity::empty()
            );
        }
        Err(error) => panic!("unexpected index reopen error: {error}"),
    }
    assert_eq!(view.verify_all().expect("raw history").events, 5);
}

#[test]
fn maximum_body_batch_has_bounded_writes_and_every_byte_is_page_reachable() {
    let root = tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "large").expect("journal");
    journal.append_batch(0..128).expect("events");
    let view = journal.read_view();
    let mut index = TranscriptIndex::open(&view, 1).expect("index");
    let mutations = (0..128)
        .map(|ordinal| {
            let mut row = row(ordinal);
            row.payload = vec![u8::try_from(ordinal).expect("byte"); MAX_ROW_BYTES];
            TranscriptIndexMutation::Put(row)
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        index.apply(
            JournalPrefixIdentity::empty(),
            &view,
            0,
            b"",
            false,
            &mutations
        ),
        Err(TranscriptIndexError::Limit("batch bytes"))
    ));
    for (batch_index, batch) in mutations.chunks(32).enumerate() {
        let before = index.io_metrics();
        let head = index.head().expect("head");
        index
            .apply(
                head.prefix,
                &view,
                0,
                &vec![0; MAX_CHECKPOINT_BYTES],
                batch_index < 3,
                batch,
            )
            .expect("bounded body batch");
        let after = index.io_metrics();
        assert!(
            after.bytes_written - before.bytes_written < 4 * 1024 * 1024,
            "bounded batch writes {after:?}"
        );
    }
    let mut ordinal = 0;
    while ordinal < 128 {
        let page = index.page(ordinal, 64, MAX_PAGE_BYTES).expect("page");
        assert!(!page.rows.is_empty());
        assert!(page.retained_bytes <= MAX_PAGE_BYTES);
        assert!(page.rows.len() < 64);
        for row in page.rows {
            assert_eq!(row.ordinal, ordinal);
            assert_eq!(
                row.payload,
                vec![u8::try_from(ordinal).expect("byte"); MAX_ROW_BYTES]
            );
            ordinal += 1;
        }
    }
    let rejected = vec![TranscriptIndexMutation::Delete("missing".into()); MAX_BATCH_ROWS + 1];
    let before = index.io_metrics();
    assert!(matches!(
        index.apply(view.prefix_identity(), &view, 0, b"", true, &rejected),
        Err(TranscriptIndexError::Limit("batch/checkpoint"))
    ));
    assert_eq!(index.io_metrics().bytes_written, before.bytes_written);
    assert_eq!(index.head().expect("unchanged head").total_rows, 128);
}

#[test]
fn identical_prefixes_cannot_publish_into_another_sessions_index() {
    let root = tempdir().expect("root");
    let mut first = SegmentedJournal::open(root.path(), "first").expect("first");
    let mut second = SegmentedJournal::open(root.path(), "second").expect("second");
    let mut index = TranscriptIndex::open(&first.read_view(), 1).expect("index");
    let original = index.head().expect("head");
    assert_eq!(
        first.read_view().prefix_identity(),
        second.read_view().prefix_identity()
    );
    assert!(matches!(
        index.apply(
            original.prefix,
            &second.read_view(),
            0,
            b"foreign",
            false,
            &[]
        ),
        Err(TranscriptIndexError::Invalid("foreign journal"))
    ));
    first
        .append_batch(["identical bytes"])
        .expect("first append");
    second
        .append_batch(["identical bytes"])
        .expect("second append");
    assert_eq!(
        first.read_view().prefix_identity(),
        second.read_view().prefix_identity()
    );
    assert!(matches!(
        index.apply(
            original.prefix,
            &second.read_view(),
            0,
            b"foreign",
            false,
            &[TranscriptIndexMutation::Put(row(0))]
        ),
        Err(TranscriptIndexError::Invalid("foreign journal"))
    ));
    assert_eq!(index.head().expect("unchanged head"), original);
    index
        .apply(
            original.prefix,
            &first.read_view(),
            0,
            b"own",
            false,
            &[TranscriptIndexMutation::Put(row(0))],
        )
        .expect("own journal");
    let head = index.head().expect("head");
    assert!(matches!(
        index.apply(head.prefix, &second.read_view(), 0, b"foreign", false, &[]),
        Err(TranscriptIndexError::Invalid("foreign journal"))
    ));
    assert_eq!(index.head().expect("unchanged head"), head);
}
