use super::*;

#[test]
fn sqlite_index_lists_updates_and_searches_transcripts() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let index =
        SessionIndex::open(root.path()).unwrap_or_else(|error| panic!("index must open: {error}"));
    let first = SessionSummary {
        id: "first".to_owned(),
        title: "Rust parser".to_owned(),
        updated_unix_ms: 10,
        cost_micros: 7,
        turn_count: 2,
    };
    let second = SessionSummary {
        id: "second".to_owned(),
        title: "TypeScript UI".to_owned(),
        updated_unix_ms: 20,
        cost_micros: 9,
        turn_count: 4,
    };
    index
        .upsert(&SessionProjection {
            summary: first.clone(),
            transcript: "implemented a resilient parser".to_owned(),
            projected_through: Some(SequenceId(3)),
        })
        .unwrap_or_else(|error| panic!("first index row must write: {error}"));
    index
        .upsert(&SessionProjection {
            summary: second.clone(),
            transcript: "rendered terminal cells".to_owned(),
            projected_through: Some(SequenceId(8)),
        })
        .unwrap_or_else(|error| panic!("second index row must write: {error}"));
    assert_eq!(
        index
            .list(10)
            .unwrap_or_else(|error| panic!("sessions must list: {error}")),
        vec![second.clone(), first.clone()]
    );
    assert_eq!(
        index
            .search("resilient", 10)
            .unwrap_or_else(|error| panic!("sessions must search: {error}")),
        vec![first.clone()]
    );
    let updated = SessionSummary {
        title: "Rust event parser".to_owned(),
        updated_unix_ms: 30,
        cost_micros: 11,
        ..first
    };
    index
        .upsert(&SessionProjection {
            summary: updated.clone(),
            transcript: "recovered a truncated transcript".to_owned(),
            projected_through: Some(SequenceId(11)),
        })
        .unwrap_or_else(|error| panic!("updated row must write: {error}"));
    assert!(
        index
            .search("resilient", 10)
            .unwrap_or_else(|error| panic!("old search must work: {error}"))
            .is_empty()
    );
    assert_eq!(
        index
            .search("truncated", 10)
            .unwrap_or_else(|error| panic!("updated search must work: {error}")),
        vec![updated]
    );
    assert_eq!(
        index
            .projection_status("first", Some(SequenceId(11)))
            .unwrap_or_else(|error| panic!("watermark must query: {error}")),
        ProjectionStatus::Current
    );
    assert!(matches!(
        index
            .projection_status("first", Some(SequenceId(12)))
            .unwrap_or_else(|error| panic!("stale watermark must query: {error}")),
        ProjectionStatus::Stale { .. }
    ));
}

#[test]
fn read_only_listing_rejects_a_index_missing_the_turn_count_column() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let connection = rusqlite::Connection::open(root.path().join("index.sqlite"))
        .unwrap_or_else(|error| panic!("fixture index must open: {error}"));
    connection
        .execute_batch(
            "CREATE TABLE sessions(
                   id TEXT NOT NULL UNIQUE,
                   title TEXT NOT NULL,
                   updated_unix_ms INTEGER NOT NULL,
                   cost_micros INTEGER NOT NULL,
                   transcript TEXT NOT NULL,
                   projected_sequence TEXT
                 );
                 INSERT INTO sessions VALUES(
                   'fixture-session','Fixture session',10,0,'fixture transcript','0'
                 );",
        )
        .unwrap_or_else(|error| panic!("fixture schema must write: {error}"));
    drop(connection);

    let before = std::fs::read(root.path().join("index.sqlite")).unwrap_or_default();
    assert!(matches!(
        SessionIndex::list_read_only(root.path(), 10),
        Err(SessionStoreError::UnsupportedSqliteSchema { table: "sessions" })
    ));
    assert_eq!(
        std::fs::read(root.path().join("index.sqlite")).unwrap_or_default(),
        before
    );
}

#[test]
fn opening_an_unsupported_index_rejects_without_mutating_database_bytes() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let mut log = SessionEventLog::open(root.path(), "fixture-session")
        .unwrap_or_else(|error| panic!("fixture event log must open: {error}"));
    for _ in 0..3 {
        log.append(serde_json::json!({"type": "user_message_accepted"}))
            .unwrap_or_else(|error| panic!("fixture turn must append: {error}"));
    }
    drop(log);
    let connection = rusqlite::Connection::open(root.path().join("index.sqlite"))
        .unwrap_or_else(|error| panic!("fixture index must open: {error}"));
    connection
        .execute_batch(
            "CREATE TABLE sessions(
                   id TEXT NOT NULL UNIQUE,
                   title TEXT NOT NULL,
                   updated_unix_ms INTEGER NOT NULL,
                   cost_micros INTEGER NOT NULL,
                   transcript TEXT NOT NULL,
                   projected_sequence TEXT
                 );
                 INSERT INTO sessions VALUES(
                   'fixture-session','Fixture session',10,0,'fixture transcript','2'
                 );",
        )
        .unwrap_or_else(|error| panic!("fixture schema must write: {error}"));
    drop(connection);

    let before = std::fs::read(root.path().join("index.sqlite")).unwrap_or_default();
    assert!(matches!(
        SessionIndex::open(root.path()),
        Err(SessionStoreError::UnsupportedSqliteSchema { table: "sessions" })
    ));
    assert_eq!(
        std::fs::read(root.path().join("index.sqlite")).unwrap_or_default(),
        before
    );
}

#[test]
fn derived_index_rebuild_replaces_stale_rows() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let stale = SessionProjection {
        summary: SessionSummary {
            id: "stale".to_owned(),
            title: "old".to_owned(),
            updated_unix_ms: 1,
            cost_micros: 0,
            turn_count: 1,
        },
        transcript: "obsolete".to_owned(),
        projected_through: Some(SequenceId(0)),
    };
    SessionIndex::open(root.path())
        .and_then(|index| index.upsert(&stale))
        .unwrap_or_else(|error| panic!("stale row must write: {error}"));
    let current = SessionProjection {
        summary: SessionSummary {
            id: "current".to_owned(),
            title: "new".to_owned(),
            updated_unix_ms: 2,
            cost_micros: 1,
            turn_count: 2,
        },
        transcript: "authoritative".to_owned(),
        projected_through: Some(SequenceId(u64::MAX)),
    };
    let accounting = accounting_entry(
        "current",
        1,
        u64::MAX,
        "2026-07-10T00:00:00.000Z",
        Cost::Monetary {
            amount_micros: 17,
            currency: "USD".to_owned(),
        },
    );
    let rebuilt = SessionIndex::rebuild(
        root.path(),
        std::slice::from_ref(&current),
        std::slice::from_ref(&accounting),
    )
    .unwrap_or_else(|error| panic!("index must rebuild: {error}"));
    assert!(rebuilt.get("stale").unwrap_or(None).is_none());
    assert_eq!(
        rebuilt.get("current").unwrap_or(None),
        Some(current.summary)
    );
    assert_eq!(
        rebuilt
            .projection_status("current", Some(SequenceId(u64::MAX)))
            .unwrap_or_else(|error| panic!("watermark must survive: {error}")),
        ProjectionStatus::Current
    );
    assert_eq!(
        AccountingLedger::open(root.path())
            .and_then(|ledger| ledger.entries_bounded(Some("current"), 4096))
            .unwrap_or_else(|error| panic!("rebuilt accounting must query: {error}")),
        vec![accounting]
    );
}

#[test]
fn read_only_search_never_creates_or_mutates_index_artifacts() {
    let absent = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    assert!(SessionIndex::search_read_only(absent.path(), "needle", 10).is_err());
    assert!(
        std::fs::read_dir(absent.path())
            .unwrap_or_else(|error| panic!("list absent root: {error}"))
            .next()
            .is_none()
    );

    let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let projection = SessionProjection {
        summary: SessionSummary {
            id: "searchable".to_owned(),
            title: "Needle session".to_owned(),
            updated_unix_ms: 7,
            cost_micros: 0,
            turn_count: 1,
        },
        transcript: "deterministic needle transcript".to_owned(),
        projected_through: Some(SequenceId(0)),
    };
    SessionIndex::open(root.path())
        .and_then(|index| index.upsert(&projection))
        .unwrap_or_else(|error| panic!("seed index: {error}"));
    let index_path = root.path().join("index.sqlite");
    let before = std::fs::read(&index_path).unwrap_or_else(|error| panic!("read index: {error}"));
    let wal_path = root.path().join("index.sqlite-wal");
    let shm_path = root.path().join("index.sqlite-shm");
    let wal_before = std::fs::read(&wal_path).ok();
    let shm_before = std::fs::read(&shm_path).ok();
    let found = SessionIndex::search_read_only(root.path(), "needle", 10)
        .unwrap_or_else(|error| panic!("read-only search: {error}"));
    assert_eq!(found, vec![projection.summary]);
    assert!(
        SessionIndex::search_read_only(root.path(), "\" OR ( needle", 10)
            .unwrap_or_else(|error| panic!("punctuation search: {error}"))
            .is_empty()
    );
    assert_eq!(
        std::fs::read(&index_path).unwrap_or_else(|error| panic!("reread index: {error}")),
        before
    );
    assert_eq!(std::fs::read(&wal_path).ok(), wal_before);
    assert_eq!(std::fs::read(&shm_path).ok(), shm_before);
}

#[test]
fn read_only_search_sees_committed_wal_rows_without_mutating_artifacts() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    SessionIndex::open(root.path()).unwrap_or_else(|error| panic!("seed index: {error}"));
    let writer = rusqlite::Connection::open(root.path().join("index.sqlite"))
        .unwrap_or_else(|error| panic!("held writer: {error}"));
    writer
        .execute_batch("PRAGMA wal_autocheckpoint=0;")
        .unwrap_or_else(|error| panic!("disable autocheckpoint: {error}"));
    let projection = SessionProjection {
        summary: SessionSummary {
            id: "wal-fresh".to_owned(),
            title: "Fresh WAL needle".to_owned(),
            updated_unix_ms: 11,
            cost_micros: 0,
            turn_count: 1,
        },
        transcript: "committed only in the held writer WAL".to_owned(),
        projected_through: Some(SequenceId(4)),
    };
    upsert_projection(&writer, &projection)
        .unwrap_or_else(|error| panic!("WAL projection: {error}"));

    let index_path = root.path().join("index.sqlite");
    let wal_path = root.path().join("index.sqlite-wal");
    let shm_path = root.path().join("index.sqlite-shm");
    let before = [
        std::fs::read(&index_path).unwrap_or_else(|error| panic!("main db: {error}")),
        std::fs::read(&wal_path).unwrap_or_else(|error| panic!("WAL: {error}")),
        std::fs::read(&shm_path).unwrap_or_else(|error| panic!("SHM: {error}")),
    ];
    assert!(
        !before[1].is_empty(),
        "fixture must retain committed WAL bytes"
    );

    let found = SessionIndex::search_read_only(root.path(), "needle", 10)
        .unwrap_or_else(|error| panic!("fresh read-only search: {error}"));
    assert_eq!(found, vec![projection.summary]);
    assert_eq!(
        [
            std::fs::read(&index_path).unwrap_or_else(|error| panic!("main db after: {error}")),
            std::fs::read(&wal_path).unwrap_or_else(|error| panic!("WAL after: {error}")),
            std::fs::read(&shm_path).unwrap_or_else(|error| panic!("SHM after: {error}")),
        ],
        before
    );
    drop(writer);
}

#[test]
fn read_only_search_rejects_an_oversized_sparse_main_index_before_copying() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    SessionIndex::open(root.path()).unwrap_or_else(|error| panic!("seed index: {error}"));
    let index_path = root.path().join("index.sqlite");
    OpenOptions::new()
        .write(true)
        .open(&index_path)
        .and_then(|file| file.set_len(MAX_SEARCH_INDEX_BYTES + 1))
        .unwrap_or_else(|error| panic!("make sparse oversized index: {error}"));

    assert!(matches!(
        SessionIndex::search_read_only(root.path(), "needle", 10),
        Err(SessionStoreError::SessionIndexSnapshotTooLarge {
            component: "index.sqlite",
            max_bytes: MAX_SEARCH_INDEX_BYTES,
        })
    ));
}

#[test]
fn read_only_search_rejects_an_oversized_sparse_wal_before_copying() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    SessionIndex::open(root.path()).unwrap_or_else(|error| panic!("seed index: {error}"));
    let wal_path = root.path().join("index.sqlite-wal");
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&wal_path)
        .and_then(|file| file.set_len(MAX_SEARCH_INDEX_WAL_BYTES + 1))
        .unwrap_or_else(|error| panic!("make sparse oversized WAL: {error}"));

    assert!(matches!(
        SessionIndex::search_read_only(root.path(), "needle", 10),
        Err(SessionStoreError::SessionIndexSnapshotTooLarge {
            component: "index.sqlite-wal",
            max_bytes: MAX_SEARCH_INDEX_WAL_BYTES,
        })
    ));
}

#[cfg(unix)]
#[test]
fn read_only_search_rejects_symlink_and_hardlink_indexes() {
    use std::os::unix::fs::symlink;

    let target = tempdir().unwrap_or_else(|error| panic!("target tempdir: {error}"));
    SessionIndex::open(target.path()).unwrap_or_else(|error| panic!("seed target index: {error}"));
    let target_index = target.path().join("index.sqlite");

    let linked = tempdir().unwrap_or_else(|error| panic!("linked tempdir: {error}"));
    symlink(&target_index, linked.path().join("index.sqlite"))
        .unwrap_or_else(|error| panic!("index symlink: {error}"));
    assert!(SessionIndex::search_read_only(linked.path(), "needle", 10).is_err());

    let hard = tempdir().unwrap_or_else(|error| panic!("hardlink tempdir: {error}"));
    std::fs::hard_link(&target_index, hard.path().join("index.sqlite"))
        .unwrap_or_else(|error| panic!("index hardlink: {error}"));
    assert!(SessionIndex::search_read_only(hard.path(), "needle", 10).is_err());
}
