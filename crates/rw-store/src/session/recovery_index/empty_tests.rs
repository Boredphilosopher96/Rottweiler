#![allow(clippy::expect_used)]
use super::*;
use crate::session::journal::SegmentedJournal;

fn row() -> RecoveryMutation {
    RecoveryMutation::Put(RecoveryRow {
        key: RecoveryKey {
            namespace: 1,
            scope: 0,
            ordinal: 0,
        },
        payload: b"selected-source".to_vec(),
    })
}

#[test]
fn exact_empty_open_read_and_reopen_keep_only_the_exclusive_namespace() {
    let root = tempfile::tempdir().expect("root");
    let journal = SegmentedJournal::open(root.path(), "empty").expect("journal");
    let source = journal.read_view();
    for projection in [
        RecoveryProjection::Conversation,
        RecoveryProjection::Subagents,
        RecoveryProjection::Controls,
        RecoveryProjection::Routing,
        RecoveryProjection::Tasks,
        RecoveryProjection::Fork,
    ] {
        let file = root
            .path()
            .join("sessions/empty/journal/derived")
            .join(format!("{}.redb", projection.directory_name()));
        for _ in 0..2 {
            let index = RecoveryIndex::open(&source, projection, 1).expect("empty owner");
            let read = index.read().expect("empty snapshot");
            assert_eq!(index.io_metrics(), RecoveryIndexIo::default());
            assert_eq!(read.head().prefix, JournalPrefixIdentity::empty());
            assert!(read.head().checkpoint.is_empty());
            assert!(read.lookup(1, b"source").expect("lookup").is_none());
            assert!(read.last_before(1, 0, 10).expect("previous").is_none());
            let page = read.page(1, 0, Some(10), 1, 1024).expect("page");
            assert!(page.rows.is_empty());
            assert_eq!(page.next_cursor, Some(10));
            assert!(!page.has_more);
            assert!(!file.exists(), "empty source cannot need a database cache");
            assert!(read.page(1, 0, Some(u64::MAX), 1, 1024).is_err());
            drop(index);
            assert!(matches!(
                RecoveryIndex::open(&source, projection, 1),
                Err(RecoveryIndexError::Busy)
            ));
            drop(read);
        }
        assert!(!file.exists());
    }
}

#[test]
fn first_append_initializes_once_while_the_empty_snapshot_keeps_its_source_and_lock() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "first").expect("journal");
    let mut index = RecoveryIndex::open(&journal.read_view(), RecoveryProjection::Conversation, 1)
        .expect("empty owner");
    let empty = index.read().expect("empty snapshot");
    journal
        .append_batch([42_u64])
        .expect("authoritative append");
    let proof = journal
        .read_view()
        .prove_advance(JournalPrefixIdentity::empty())
        .expect("verified advance");
    index
        .apply(&proof, b"checkpoint", &[row()], &[])
        .expect("first publication");
    assert!(index.io_metrics().bytes_written > 0);
    assert!(
        empty
            .get(RecoveryKey {
                namespace: 1,
                scope: 0,
                ordinal: 0
            })
            .expect("old snapshot")
            .is_none()
    );
    assert_eq!(
        empty
            .bind_source(&journal.read_view())
            .expect("old exact source")
            .prefix_identity(),
        JournalPrefixIdentity::empty()
    );
    let current = index.read().expect("current snapshot");
    assert_eq!(current.head().prefix, proof.next().prefix_identity());
    assert_eq!(
        current
            .page(1, 0, None, 1, 1024)
            .expect("selected row")
            .rows[0]
            .payload,
        b"selected-source"
    );
    drop(current);
    drop(index);
    assert!(matches!(
        RecoveryIndex::open(&journal.read_view(), RecoveryProjection::Conversation, 1),
        Err(RecoveryIndexError::Busy)
    ));
    drop(empty);
    let reopened = RecoveryIndex::open(&journal.read_view(), RecoveryProjection::Conversation, 1)
        .expect("reopened initialized owner");
    assert_eq!(
        reopened.head().expect("persisted checkpoint").checkpoint,
        b"checkpoint"
    );
}

#[test]
fn crash_after_first_lazy_publication_rebuilds_from_the_exact_durable_source() {
    const ROOT_ENV: &str = "ROTTWEILER_EMPTY_INDEX_CRASH_ROOT";
    const TEST: &str = "session::recovery_index::empty_tests::crash_after_first_lazy_publication_rebuilds_from_the_exact_durable_source";
    if let Some(root) = std::env::var_os(ROOT_ENV) {
        let mut journal =
            SegmentedJournal::open(std::path::Path::new(&root), "crash").expect("child journal");
        let mut index =
            RecoveryIndex::open(&journal.read_view(), RecoveryProjection::Conversation, 1)
                .expect("lazy owner");
        assert_eq!(index.io_metrics(), RecoveryIndexIo::default());
        journal.append_batch([42_u64]).expect("durable source");
        let proof = journal
            .read_view()
            .prove_advance(JournalPrefixIdentity::empty())
            .expect("proof");
        index
            .apply(&proof, b"checkpoint", &[row()], &[])
            .expect("publication");
        std::process::exit(0);
    }
    let root = tempfile::tempdir().expect("root");
    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", TEST, "--nocapture"])
        .env(ROOT_ENV, root.path())
        .status()
        .expect("crash child");
    assert!(status.success());
    let journal = SegmentedJournal::open(root.path(), "crash").expect("journal survives");
    let source = journal.read_view();
    let page = source
        .page::<u64>(None, crate::session::SessionEventPageLimits::default())
        .expect("durable events");
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].event, 42);
    let mut index = RecoveryIndex::open(&source, RecoveryProjection::Conversation, 1)
        .expect("bounded crash reopen");
    let head = index.head().expect("reset head");
    assert_eq!(head.prefix, JournalPrefixIdentity::empty());
    let proof = source.prove_advance(head.prefix).expect("rebuild proof");
    index
        .apply(&proof, b"checkpoint", &[row()], &[])
        .expect("source reconstruction");
    assert_eq!(
        index.head().expect("reconstructed source").prefix,
        source.prefix_identity()
    );
    assert_eq!(
        index
            .read()
            .expect("reconstructed view")
            .page(1, 0, None, 1, 1024)
            .expect("row")
            .rows[0]
            .payload,
        b"selected-source"
    );
}
