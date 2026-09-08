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
