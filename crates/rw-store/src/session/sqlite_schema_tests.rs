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
fn derived_rebuild_preserves_authority_and_rolls_back_on_accounting_conflict() {
    let root = tempdir().expect("root");
    let ledger = AccountingLedger::open(root.path()).expect("ledger");
    ledger.record(&charge()).expect("charge");
    let connection = Connection::open(root.path().join("index.sqlite")).expect("db");
    connection.execute_batch("CREATE TABLE reservations(id TEXT PRIMARY KEY, micros INTEGER NOT NULL); INSERT INTO reservations VALUES ('uncertain',17);").expect("independent authority");
    drop(connection);
    let row = SessionProjection {
        summary: SessionSummary {
            id: "visible".into(),
            title: "new".into(),
            updated_unix_ms: 1,
            cost_micros: 0,
            turn_count: 1,
        },
        transcript: "searchable".into(),
        projected_through: Some(SequenceId(1)),
    };
    let index =
        SessionIndex::rebuild(root.path(), std::slice::from_ref(&row), &[]).expect("rebuild");
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
    let mut conflict = charge();
    conflict.cost = Cost::Monetary {
        amount_micros: 6,
        currency: "USD".into(),
    };
    assert!(matches!(
        SessionIndex::rebuild(root.path(), &[], &[conflict]),
        Err(SessionStoreError::AccountingConflict)
    ));
    assert_eq!(
        index
            .get("visible")
            .expect("derived transaction rolled back"),
        Some(row.summary)
    );
    assert_eq!(
        index
            .search("searchable", 10)
            .expect("search rolled back")
            .len(),
        1
    );
    assert_eq!(
        ledger
            .entries_bounded(None, 4096)
            .expect("ledger unchanged"),
        vec![charge()]
    );
}

#[test]
fn corrupt_database_is_not_deleted_as_a_search_repair() {
    let root = tempdir().expect("root");
    let path = root.path().join("index.sqlite");
    std::fs::write(&path, b"unknown authoritative bytes").expect("fixture");
    assert!(SessionIndex::rebuild(root.path(), &[], &[]).is_err());
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
    let index = SessionIndex::rebuild(root.path(), &[], &[]).expect("explicit derived repair");
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
