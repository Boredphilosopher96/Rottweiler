#![allow(clippy::expect_used)]
use super::*;
use rusqlite::Connection;
use rw_types::{AccountingAttribution, Cost, TurnId, Usage};
use tempfile::tempdir;

fn charge() -> TurnAccountingEntry {
    TurnAccountingEntry {
        session_id: "charged".into(),
        turn_id: TurnId("1".into()),
        sequence_id: SequenceId(0),
        emitted_at_utc: UtcTimestamp::parse("2026-09-04T00:00:00.000Z").expect("time"),
        utc_day: UtcDayKey::parse("2026-09-04").expect("day"),
        attribution: AccountingAttribution::Main,
        usage: Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        },
        cost: Cost::Monetary {
            amount_micros: 5,
            currency: "USD".into(),
        },
    }
}

#[test]
fn derived_rebuild_preserves_accounting_and_reservations() {
    let root = tempdir().expect("root");
    let ledger = AccountingLedger::open(root.path()).expect("ledger");
    ledger.record(&charge()).expect("charge");
    let connection = Connection::open(root.path().join("index.sqlite")).expect("db");
    connection.execute_batch("CREATE TABLE reservations(id TEXT PRIMARY KEY, micros INTEGER NOT NULL); INSERT INTO reservations VALUES ('uncertain',17);").expect("independent authority");
    drop(connection);
    let index = SessionIndex::reset_derived(root.path()).expect("reset derived search");
    assert!(index.list(10).expect("empty derived index").is_empty());
    assert_eq!(
        ledger.entries_bounded(None, 4096).expect("ledger survives"),
        vec![charge()]
    );
    let connection = Connection::open(root.path().join("index.sqlite")).expect("db");
    let reserved: i64 = connection
        .query_row(
            "SELECT micros FROM reservations WHERE id='uncertain'",
            [],
            |row| row.get(0),
        )
        .expect("reservation survives");
    assert_eq!(reserved, 17);
}

#[test]
fn corrupt_database_is_not_deleted_as_a_search_repair() {
    let root = tempdir().expect("root");
    let path = root.path().join("index.sqlite");
    std::fs::write(&path, b"unknown authoritative bytes").expect("fixture");
    assert!(SessionIndex::reset_derived(root.path()).is_err());
    assert_eq!(
        std::fs::read(path).expect("preserved"),
        b"unknown authoritative bytes"
    );
}

#[test]
fn explicit_search_rebuild_can_replace_an_unsupported_derived_schema() {
    let root = tempdir().expect("root");
    let ledger = AccountingLedger::open(root.path()).expect("ledger");
    ledger.record(&charge()).expect("charge");
    let connection = Connection::open(root.path().join("index.sqlite")).expect("db");
    connection.execute_batch("CREATE TABLE sessions(id TEXT, obsolete TEXT); INSERT INTO sessions VALUES ('old','old');").expect("unsupported derived table");
    drop(connection);
    assert!(matches!(
        SessionIndex::open(root.path()),
        Err(SessionStoreError::UnsupportedSqliteSchema { table: "sessions" })
    ));
    let index = SessionIndex::reset_derived(root.path()).expect("explicit derived repair");
    assert!(index.list(10).expect("empty current index").is_empty());
    assert_eq!(
        ledger
            .entries_bounded(None, 4096)
            .expect("ledger preserved"),
        vec![charge()]
    );
}

#[test]
fn additional_turn_uniqueness_is_rejected_even_with_current_columns() {
    let root = tempdir().expect("root");
    let path = root.path().join("index.sqlite");
    let connection = Connection::open(&path).expect("db");
    let schema = sqlite_schema::ACCOUNTING_SCHEMA.replace(
        "PRIMARY KEY(session_id,sequence_id)",
        "PRIMARY KEY(session_id,sequence_id), UNIQUE(session_id,turn_id)",
    );
    connection
        .execute_batch(&schema)
        .expect("unexpected constraint");
    drop(connection);
    let before = std::fs::read(&path).expect("before");
    assert!(matches!(
        AccountingLedger::open(root.path()),
        Err(SessionStoreError::UnsupportedSqliteSchema {
            table: "turn_accounting"
        })
    ));
    assert_eq!(std::fs::read(path).expect("after"), before);
}

#[test]
fn separately_created_fixture_unique_index_is_rejected() {
    let root = tempdir().expect("root");
    AccountingLedger::open(root.path()).expect("ledger");
    let connection = Connection::open(root.path().join("index.sqlite")).expect("db");
    connection
        .execute_batch("CREATE UNIQUE INDEX old_turn_key ON turn_accounting(session_id,turn_id);")
        .expect("unexpected constraint");
    drop(connection);
    assert!(matches!(
        AccountingLedger::open(root.path()),
        Err(SessionStoreError::UnsupportedSqliteSchema {
            table: "turn_accounting"
        })
    ));
}

#[test]
fn database_initializer_process() {
    use std::io::Read as _;
    let Some(root) = std::env::var_os("RW_TEST_SQLITE_INITIALIZER_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    std::io::stdin().read_exact(&mut [0]).expect("start signal");
    let index = SessionIndex::open(&root).expect("concurrent index open");
    let ledger = AccountingLedger::open(&root).expect("concurrent accounting open");
    let mut entry = charge();
    entry.session_id = std::env::var("RW_TEST_SQLITE_INITIALIZER_ID").expect("identity");
    ledger.record(&entry).expect("independent charge");
    assert!(index.list(10).expect("index read").is_empty());
}

#[test]
fn independent_processes_initialize_search_and_accounting_atomically() {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let root = tempdir().expect("root");
    let executable = std::env::current_exe().expect("test executable");
    let mut children = (0..8)
        .map(|id| {
            Command::new(&executable)
                .args([
                    "--exact",
                    "session::sqlite_schema_tests::database_initializer_process",
                    "--nocapture",
                ])
                .env("RW_TEST_SQLITE_INITIALIZER_ROOT", root.path())
                .env("RW_TEST_SQLITE_INITIALIZER_ID", format!("process-{id}"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn database initializer")
        })
        .collect::<Vec<_>>();
    for child in &mut children {
        child
            .stdin
            .take()
            .expect("signal pipe")
            .write_all(&[1])
            .expect("start child");
    }
    // Reap every child before asserting, including when an initializer fails.
    let results = children
        .into_iter()
        .map(|child| child.wait_with_output().expect("reap child"))
        .collect::<Vec<_>>();
    for result in results {
        assert!(
            result.status.success(),
            "initializer failed: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let entries = AccountingLedger::open(root.path())
        .expect("ledger")
        .entries_bounded(None, 4096)
        .expect("charges");
    assert_eq!(entries.len(), 8);
    assert!(entries.iter().all(|entry| entry.cost == charge().cost));
}
