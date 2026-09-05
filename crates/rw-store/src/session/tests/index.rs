#![allow(clippy::expect_used)]
use super::*;

fn summary(id: &str, title: &str, updated: i64) -> SessionSummary {
    SessionSummary {
        id: id.into(),
        title: title.into(),
        updated_unix_ms: updated,
        cost_micros: 0,
        turn_count: 1,
    }
}
fn projection(summary: SessionSummary, next_sequence: u64) -> SessionProjection {
    SessionProjection {
        summary,
        explicit_title: true,
        complete: true,
        source: crate::session::journal::JournalPrefixIdentity {
            next_sequence,
            digest: [0; 32],
        },
    }
}

#[test]
fn sqlite_search_matches_terms_across_documents_and_atomically_rewinds() {
    let root = tempdir().expect("root");
    let index = SessionIndex::open(root.path()).expect("index");
    let first = projection(summary("first", "Rust parser", 10), 4);
    index
        .apply_page(None, &first, |writer| {
            writer.text(1, SequenceId(0), 0, "implemented a resilient parser")?;
            writer.text(2, SequenceId(3), 0, "rendered terminal cells")
        })
        .expect("bounded page");
    assert_eq!(
        index
            .search("resilient terminal", 10)
            .expect("session-wide conjunction"),
        vec![first.summary.clone()]
    );
    let second = projection(summary("second", "TypeScript UI", 20), 2);
    index
        .apply_page(None, &second, |writer| {
            writer.text(1, SequenceId(1), 0, "other document")
        })
        .expect("second");
    assert_eq!(
        index.list(10).expect("list"),
        vec![second.summary, first.summary.clone()]
    );
    let updated = projection(summary("first", "Rust event parser", 30), 12);
    index
        .apply_page(Some(first.source), &updated, |writer| {
            writer.rewind(1)?;
            writer.text(2, SequenceId(11), 0, "recovered a truncated transcript")
        })
        .expect("rewind and append");
    assert!(
        index
            .search("terminal", 10)
            .expect("discarded document")
            .is_empty()
    );
    assert_eq!(
        index
            .search("resilient truncated", 10)
            .expect("retained and new"),
        vec![updated.summary.clone()]
    );
    assert_eq!(
        index
            .projection_status("first", Some(SequenceId(11)))
            .expect("cursor"),
        ProjectionStatus::Current
    );
    assert!(matches!(
        index
            .projection_status("first", Some(SequenceId(12)))
            .expect("cursor"),
        ProjectionStatus::Stale { .. }
    ));
    assert!(
        index
            .apply_page(Some(first.source), &updated, |writer| writer.rewind(0))
            .is_err()
    );
    assert_eq!(
        index
            .search("resilient truncated", 10)
            .expect("CAS rejected before effects"),
        vec![updated.summary]
    );
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
        explicit_title: false,
        complete: true,
        source: crate::session::journal::JournalPrefixIdentity {
            next_sequence: 1,
            digest: [0; 32],
        },
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
        explicit_title: false,
        complete: true,
        source: crate::session::journal::JournalPrefixIdentity {
            next_sequence: u64::MAX,
            digest: [0; 32],
        },
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
    let ledger = AccountingLedger::open(root.path()).expect("ledger");
    ledger.record(&accounting).expect("durable accounting");
    let rebuilt = SessionIndex::reset_derived(root.path()).expect("reset only derived rows");
    rebuilt
        .upsert(&current)
        .expect("bounded metadata projection");
    assert!(rebuilt.get("stale").unwrap_or(None).is_none());
    assert_eq!(
        rebuilt.get("current").unwrap_or(None),
        Some(current.summary)
    );
    assert_eq!(
        rebuilt
            .projection_status("current", Some(SequenceId(u64::MAX - 1)))
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
fn read_only_search_preserves_stored_rows_and_does_not_create_a_missing_index() {
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
        explicit_title: false,
        complete: true,
        source: crate::session::journal::JournalPrefixIdentity {
            next_sequence: 1,
            digest: [0; 32],
        },
    };
    SessionIndex::open(root.path())
        .and_then(|index| index.upsert(&projection))
        .unwrap_or_else(|error| panic!("seed index: {error}"));
    let index_path = root.path().join("index.sqlite");
    let before = std::fs::read(&index_path).unwrap_or_else(|error| panic!("read index: {error}"));
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
}

#[test]
fn read_only_search_sees_committed_wal_rows_without_writing_data() {
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
        explicit_title: false,
        complete: true,
        source: crate::session::journal::JournalPrefixIdentity {
            next_sequence: 5,
            digest: [0; 32],
        },
    };
    upsert_projection(&writer, &projection)
        .unwrap_or_else(|error| panic!("WAL projection: {error}"));

    let index_path = root.path().join("index.sqlite");
    let wal_path = root.path().join("index.sqlite-wal");
    let before = [
        std::fs::read(&index_path).unwrap_or_else(|error| panic!("main db: {error}")),
        std::fs::read(&wal_path).unwrap_or_else(|error| panic!("WAL: {error}")),
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
        ],
        before
    );
    drop(writer);
}

#[test]
fn read_only_search_does_not_copy_large_sparse_database_extents() {
    let root = tempdir().expect("root");
    let index = SessionIndex::open(root.path()).expect("index");
    let projection = projection(summary("large", "needle", 1), 1);
    index.upsert(&projection).expect("projection");
    let path = root.path().join("index.sqlite");
    OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("file")
        .set_len(512 * 1024 * 1024)
        .expect("sparse extent");
    assert_eq!(
        SessionIndex::search_read_only(root.path(), "needle", 10).expect("indexed read"),
        vec![projection.summary]
    );
    assert_eq!(
        std::fs::metadata(path).expect("file").len(),
        512 * 1024 * 1024
    );
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

#[test]
fn missing_or_contradictory_search_triggers_require_a_derived_rebuild() {
    for replacement in [
        "",
        "CREATE TRIGGER search_documents_ai AFTER INSERT ON search_documents BEGIN SELECT 1; END;",
    ] {
        let root = tempdir().expect("root");
        let index = SessionIndex::open(root.path()).expect("index");
        let row = projection(summary("session", "needle", 1), 1);
        index.upsert(&row).expect("initial row");
        let connection =
            rusqlite::Connection::open(root.path().join("index.sqlite")).expect("connection");
        connection
            .execute_batch("DROP TRIGGER search_documents_ai;")
            .expect("remove trigger");
        connection.execute_batch(replacement).expect("replacement");
        assert!(matches!(
            SessionIndex::search_read_only(root.path(), "needle", 10),
            Err(SessionStoreError::UnsupportedSqliteSchema {
                table: "search_documents"
            })
        ));
        assert!(SessionIndex::open(root.path()).is_err());
        let rebuilt = SessionIndex::reset_derived(root.path()).expect("rebuild derived owner");
        rebuilt.upsert(&row).expect("regenerated row");
        assert_eq!(
            rebuilt.search("needle", 10).expect("valid trigger"),
            vec![row.summary]
        );
    }
}
