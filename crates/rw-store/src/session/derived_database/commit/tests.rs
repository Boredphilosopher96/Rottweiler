#![allow(clippy::expect_used)]
use super::*;
use crate::session::{SessionEventLog, derived_database::DerivedDatabase};
use redb::{ReadableDatabase as _, TableDefinition};
use std::sync::atomic::Ordering;

const VALUES: TableDefinition<u64, u64> = TableDefinition::new("values");

fn put(owner: &DerivedDatabase, value: u64, charge: usize) {
    let transaction = owner.database.begin_write().expect("writer");
    transaction
        .open_table(VALUES)
        .expect("table")
        .insert(0, value)
        .expect("value");
    owner.commits.commit(transaction, charge).expect("commit");
}

fn read(owner: &DerivedDatabase) -> u64 {
    owner
        .database
        .begin_read()
        .expect("reader")
        .open_table(VALUES)
        .expect("table")
        .get(0)
        .expect("lookup")
        .expect("value")
        .value()
}

#[test]
fn derived_publication_is_visible_and_flushes_on_both_bounded_thresholds() {
    let root = tempfile::tempdir().expect("root");
    let journal = SessionEventLog::open(root.path(), "session").expect("journal");
    let view = journal.read_view();
    let owner = DerivedDatabase::open(&view, "recovery", 1024 * 1024, 16 * 1024 * 1024, false)
        .expect("projection");
    let initial = owner.counters.syncs.load(Ordering::Relaxed);
    for value in 1..MAX_COMMITS as u64 {
        put(&owner, value, 16);
        assert_eq!(read(&owner), value);
        assert_eq!(owner.counters.syncs.load(Ordering::Relaxed), initial);
    }
    put(&owner, 8, 16);
    let flushed = owner.counters.syncs.load(Ordering::Relaxed);
    assert!(flushed > initial);
    assert_eq!(owner.commits.0.lock().expect("accounting").commits, 0);
    put(&owner, 9, MAX_MUTATION_BYTES - 1);
    assert_eq!(owner.counters.syncs.load(Ordering::Relaxed), flushed);
    put(&owner, 10, 1);
    assert!(owner.counters.syncs.load(Ordering::Relaxed) > flushed);
    assert_eq!(
        owner.commits.0.lock().expect("accounting").mutation_bytes,
        0
    );
    put(&owner, 11, 16);
    drop(owner);
    let reopened = DerivedDatabase::open(&view, "recovery", 1024 * 1024, 16 * 1024 * 1024, false)
        .expect("clean reopen");
    assert!(!reopened.was_empty);
    assert_eq!(read(&reopened), 11);
}

#[test]
fn crash_discards_unflushed_projection_without_losing_authoritative_events() {
    const CHILD_ROOT: &str = "ROTTWEILER_DERIVED_PENDING_CRASH";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let mut journal =
            SessionEventLog::open(std::path::Path::new(&root), "crash").expect("journal");
        journal.append(73_u64).expect("durable source");
        let owner = DerivedDatabase::open(
            &journal.read_view(),
            "recovery",
            1024 * 1024,
            16 * 1024 * 1024,
            false,
        )
        .expect("projection");
        put(&owner, 73, 16);
        assert_eq!(read(&owner), 73);
        std::process::exit(73);
    }
    let root = tempfile::tempdir().expect("root");
    let test = format!(
        "{}::crash_discards_unflushed_projection_without_losing_authoritative_events",
        module_path!().strip_prefix("rw_store::").expect("module")
    );
    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", &test, "--nocapture"])
        .env(CHILD_ROOT, root.path())
        .status()
        .expect("crashed process");
    assert_eq!(status.code(), Some(73));
    let journal = SessionEventLog::open(root.path(), "crash").expect("durable source");
    assert_eq!(journal.read_view().verify_all().expect("source").events, 1);
    let owner = DerivedDatabase::open(
        &journal.read_view(),
        "recovery",
        1024 * 1024,
        16 * 1024 * 1024,
        false,
    )
    .expect("bounded rebuild owner");
    assert!(owner.was_empty);
    assert!(matches!(
        owner
            .database
            .begin_read()
            .expect("reader")
            .open_table(VALUES),
        Err(redb::TableError::TableDoesNotExist(_))
    ));
}

#[test]
fn recovery_snapshot_keeps_exact_source_and_rows_across_repeated_flushes() {
    use crate::session::recovery_index::{
        RecoveryIndex, RecoveryKey, RecoveryMutation, RecoveryProjection, RecoveryRow,
    };
    let root = tempfile::tempdir().expect("root");
    let mut journal = SessionEventLog::open(root.path(), "snapshots").expect("journal");
    let mut index = RecoveryIndex::open(&journal.read_view(), RecoveryProjection::Conversation, 1)
        .expect("index");
    let key = RecoveryKey {
        namespace: 1,
        scope: 0,
        ordinal: 0,
    };
    let mut prefix = journal.read_view().prefix_identity();
    let mut held = None;
    let initial_syncs = index.io_metrics().syncs;
    for value in 0..(MAX_COMMITS * 4) {
        journal.append(value as u64).expect("durable source");
        let advance = journal.read_view().prove_advance(prefix).expect("advance");
        let bytes = (value as u64).to_le_bytes();
        index
            .apply(
                &advance,
                &bytes,
                &[RecoveryMutation::Put(RecoveryRow {
                    key,
                    payload: bytes.to_vec(),
                })],
                &[],
            )
            .expect("atomic projection");
        prefix = advance.next().prefix_identity();
        if value == 0 {
            held = Some(index.read().expect("held first snapshot"));
        }
        let old = held.as_ref().expect("first snapshot");
        assert_eq!(old.head().prefix.next_sequence, 1);
        assert_eq!(old.head().checkpoint, 0_u64.to_le_bytes());
        assert_eq!(
            old.get(key).expect("old row").expect("present").payload,
            0_u64.to_le_bytes()
        );
        assert_eq!(
            old.bind_source(&journal.read_view())
                .expect("exact prefix")
                .prefix_identity(),
            old.head().prefix
        );
        let current = index.read().expect("current snapshot");
        assert_eq!(current.head().prefix, prefix);
        assert_eq!(
            current
                .get(key)
                .expect("current row")
                .expect("present")
                .payload,
            bytes
        );
        if (value + 1) % MAX_COMMITS == 0 {
            assert!(index.io_metrics().syncs >= initial_syncs + ((value + 1) / MAX_COMMITS) as u64);
        }
    }
    drop(index);
    assert!(matches!(
        RecoveryIndex::open(&journal.read_view(), RecoveryProjection::Conversation, 1),
        Err(crate::session::recovery_index::RecoveryIndexError::Busy)
    ));
    drop(held);
    let reopened = RecoveryIndex::open(&journal.read_view(), RecoveryProjection::Conversation, 1)
        .expect("settled reopen");
    assert_eq!(reopened.head().expect("head").prefix, prefix);
    assert_eq!(
        reopened
            .read()
            .expect("snapshot")
            .get(key)
            .expect("row")
            .expect("present")
            .payload,
        (MAX_COMMITS as u64 * 4 - 1).to_le_bytes()
    );
}

#[derive(Debug)]
struct FailingFlush {
    inner: redb::backends::InMemoryBackend,
    fail: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl redb::StorageBackend for FailingFlush {
    fn len(&self) -> std::io::Result<u64> {
        self.inner.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        self.inner.read(offset, out)
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.inner.set_len(len)
    }
    fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        self.inner.write(offset, data)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        if self.fail.load(Ordering::Relaxed) {
            Err(std::io::Error::other("injected flush failure"))
        } else {
            self.inner.sync_data()
        }
    }
}

#[test]
fn failed_flush_preserves_obligation_and_database_requires_reopen() {
    let fail = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let database = redb::Database::builder()
        .create_with_backend(FailingFlush {
            inner: redb::backends::InMemoryBackend::new(),
            fail: fail.clone(),
        })
        .expect("database");
    let policy = DerivedCommitPolicy::default();
    for value in 0..MAX_COMMITS - 1 {
        let transaction = database.begin_write().expect("writer");
        transaction
            .open_table(VALUES)
            .expect("table")
            .insert(0, value as u64)
            .expect("write");
        policy.commit(transaction, 16).expect("pending commit");
    }
    let transaction = database.begin_write().expect("flush writer");
    transaction
        .open_table(VALUES)
        .expect("table")
        .insert(0, 999)
        .expect("write");
    fail.store(true, Ordering::Relaxed);
    assert!(policy.commit(transaction, 16).is_err());
    let pending = policy.0.lock().expect("accounting");
    assert_eq!(pending.commits, MAX_COMMITS - 1);
    assert_eq!(pending.mutation_bytes, (MAX_COMMITS - 1) * 16);
    drop(pending);
    fail.store(false, Ordering::Relaxed);
    // redb latches a failed physical commit. Clearing the injected fault does not
    // make the old allocator state safe to reuse; the source-backed owner reopens.
    assert!(database.begin_write().is_err());
}
