#![allow(clippy::expect_used)]
use super::super::super::{SessionIndex, SessionProjection, SessionSummary};
use super::*;
use crate::session::{
    SessionIndexReadControl, index_read::read_index, journal::JournalPrefixIdentity,
};
use rw_types::SequenceId;

fn projection(next: u64, title: &str) -> SessionProjection {
    SessionProjection {
        summary: SessionSummary {
            id: "writer".into(),
            title: title.into(),
            updated_unix_ms: 1,
            cost_micros: 0,
            turn_count: 1,
        },
        explicit_title: true,
        complete: true,
        source: JournalPrefixIdentity {
            next_sequence: next,
            digest: [1; 32],
        },
        input_claims: vec![1],
    }
}

#[test]
fn updates_and_clones_reuse_the_connection_and_reopen_persisted_state() {
    let root = tempfile::tempdir().expect("root");
    let index = SessionIndex::open(root.path()).expect("writer");
    index
        .connection()
        .expect("connection")
        .execute_batch(
            "CREATE TEMP TABLE connection_marker(value); INSERT INTO connection_marker VALUES(17);",
        )
        .expect("connection-local marker");
    let clone = index.clone();
    for next in 1..=20 {
        clone.upsert(&projection(next, "needle")).expect("update");
    }
    let guard = index.connection().expect("same connection");
    assert_eq!(
        guard
            .query_row("SELECT value FROM connection_marker", [], |row| row
                .get::<_, i64>(0))
            .expect("marker"),
        17
    );
    assert_eq!(
        guard
            .query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))
            .expect("durability"),
        2
    );
    assert_eq!(
        guard
            .query_row("PRAGMA cache_size", [], |row| row.get::<_, i64>(0))
            .expect("cache"),
        -1024
    );
    drop(guard);
    drop(clone);
    drop(index);
    let reopened = SessionIndex::open(root.path()).expect("fresh owner");
    assert_eq!(
        reopened.projection("writer").expect("read"),
        Some(projection(20, "needle"))
    );
    assert!(
        reopened
            .connection()
            .expect("fresh connection")
            .prepare("SELECT value FROM connection_marker")
            .is_err()
    );
}

#[test]
fn failed_document_transaction_rolls_back_then_the_same_writer_retries() {
    let root = tempfile::tempdir().expect("root");
    let index = SessionIndex::open(root.path()).expect("writer");
    let first = projection(1, "initial");
    index.upsert(&first).expect("initial");
    let next = projection(2, "published");
    let failed = index.apply_page(Some(first.source), &next, |writer| {
        writer.text(1, SequenceId(1), 0, "rollbackneedle")?;
        Err(SessionStoreError::CorruptProjectionWatermark)
    });
    assert!(matches!(
        failed,
        Err(SessionStoreError::CorruptProjectionWatermark)
    ));
    assert_eq!(
        index.projection("writer").expect("cursor"),
        Some(first.clone())
    );
    assert!(
        index
            .search("rollbackneedle", 1)
            .expect("rolled back document")
            .is_empty()
    );
    index
        .apply_page(Some(first.source), &next, |writer| {
            writer.text(1, SequenceId(1), 0, "retryneedle")
        })
        .expect("retry");
    assert_eq!(
        index.search("retryneedle", 1).expect("published document"),
        vec![next.summary]
    );
}

#[test]
fn reader_snapshot_and_cancellation_are_independent_of_the_retained_writer() {
    let root = tempfile::tempdir().expect("root");
    let index = SessionIndex::open(root.path()).expect("writer");
    index.upsert(&projection(1, "before")).expect("first");
    let (entered, enter) = std::sync::mpsc::sync_channel(1);
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let path = root.path().to_owned();
    let reader = std::thread::spawn(move || {
        read_index(&path, &SessionIndexReadControl::new(), |connection| {
            let read = || {
                connection.query_row("SELECT title FROM sessions WHERE id='writer'", [], |row| {
                    row.get::<_, String>(0)
                })
            };
            assert_eq!(read()?, "before");
            entered.send(()).expect("entered");
            released
                .recv_timeout(Duration::from_secs(1))
                .expect("release reader");
            assert_eq!(read()?, "before", "snapshot must not move with the writer");
            Ok(())
        })
        .expect("snapshot")
    });
    enter
        .recv_timeout(Duration::from_secs(1))
        .expect("reader entered");
    index
        .upsert(&projection(2, "after"))
        .expect("writer proceeds beside reader");
    release.send(()).expect("release");
    reader.join().expect("reader settled");
    let cancelled = SessionIndexReadControl::new();
    cancelled.cancel();
    assert!(matches!(
        SessionIndex::search_hits_read_only(root.path(), "after", 1, &cancelled),
        Err(SessionStoreError::IndexReadInterrupted)
    ));
    index
        .upsert(&projection(3, "uncancelled"))
        .expect("writer has no reader progress hook");
    assert_eq!(
        SessionIndex::search_read_only(root.path(), "uncancelled", 1)
            .expect("new reader")
            .len(),
        1
    );
}

#[test]
fn externally_changed_schema_is_rejected_by_the_retained_owner() {
    let root = tempfile::tempdir().expect("root");
    let index = SessionIndex::open(root.path()).expect("writer");
    index.upsert(&projection(1, "original")).expect("first");
    Connection::open(root.path().join("index.sqlite"))
        .expect("other schema writer")
        .execute_batch("ALTER TABLE sessions ADD COLUMN unexpected TEXT")
        .expect("change schema");
    assert!(matches!(
        index.upsert(&projection(2, "invalid")),
        Err(SessionStoreError::UnsupportedSqliteSchema { table: "sessions" })
    ));
    let rebuilt = SessionIndex::reset_derived(root.path()).expect("explicit derived reset");
    rebuilt
        .upsert(&projection(1, "rebuilt"))
        .expect("new source");
    assert_eq!(
        index
            .projection("writer")
            .expect("valid schema cookie change"),
        Some(projection(1, "rebuilt"))
    );
}

#[cfg(unix)]
#[test]
fn replaced_database_and_root_fail_closed_without_writing_the_replacement() {
    for replace_root in [false, true] {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("root");
        let index = SessionIndex::open(&root).expect("writer");
        index.upsert(&projection(1, "original")).expect("first");
        if replace_root {
            fs::rename(&root, parent.path().join("retired-root")).expect("replace root");
        } else {
            fs::rename(root.join("index.sqlite"), root.join("retired.sqlite"))
                .expect("replace database");
            // The replacement has its own WAL namespace, never the old connection's WAL.
            fs::rename(
                root.join("index.sqlite-wal"),
                root.join("retired.sqlite-wal"),
            )
            .expect("old WAL");
            fs::rename(
                root.join("index.sqlite-shm"),
                root.join("retired.sqlite-shm"),
            )
            .expect("old SHM");
        }
        let replacement = SessionIndex::open(&root).expect("replacement writer");
        replacement
            .upsert(&projection(9, "replacement"))
            .expect("replacement source");
        assert!(matches!(
            index.upsert(&projection(2, "forbidden")),
            Err(SessionStoreError::UnsafeSessionIndex)
        ));
        assert_eq!(
            replacement.projection("writer").expect("unchanged"),
            Some(projection(9, "replacement"))
        );
        drop(index);
        assert_eq!(
            replacement
                .projection("writer")
                .expect("replacement survives old close"),
            Some(projection(9, "replacement"))
        );
    }
}
