use super::*;

#[test]
fn unsupported_accounting_schema_rejects_without_changing_rows_or_bytes() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let path = root.path().join("index.sqlite");
    let connection = rusqlite::Connection::open(&path)
        .unwrap_or_else(|error| panic!("fixture index must open: {error}"));
    connection
        .execute_batch(
            "CREATE TABLE fixture_marker(value TEXT NOT NULL); \
                 INSERT INTO fixture_marker(value) VALUES ('preserved'); \
                 CREATE TABLE turn_accounting( \
                   session_id TEXT NOT NULL, turn_id TEXT NOT NULL, \
                   sequence_id TEXT NOT NULL, emitted_at_utc TEXT NOT NULL, \
                   utc_day TEXT NOT NULL, cost_json TEXT NOT NULL, \
                   PRIMARY KEY(session_id,sequence_id), UNIQUE(session_id,turn_id) \
                 ); \
                 INSERT INTO turn_accounting( \
                   session_id,turn_id,sequence_id,emitted_at_utc,utc_day,cost_json \
                 ) VALUES ( \
                   'fixture-accounting','1','0','2026-01-01T00:00:00.000Z', \
                   '2026-01-01', \
                   '{\"kind\":\"monetary\",\"amount_micros\":\"5\",\"currency\":\"USD\"}' \
                 );",
        )
        .unwrap_or_else(|error| panic!("fixture schema must create: {error}"));
    drop(connection);

    let before = std::fs::read(&path).unwrap_or_default();
    assert!(matches!(
        AccountingLedger::open(root.path()),
        Err(SessionStoreError::UnsupportedSqliteSchema {
            table: "turn_accounting"
        })
    ));
    assert!(matches!(
        SessionIndex::rebuild(root.path(), &[], &[]),
        Err(SessionStoreError::UnsupportedSqliteSchema {
            table: "turn_accounting"
        })
    ));
    assert_eq!(std::fs::read(&path).unwrap_or_default(), before);
    let connection = rusqlite::Connection::open(&path)
        .unwrap_or_else(|error| panic!("inspect rejected database: {error}"));
    let rows: i64 = connection
        .query_row("SELECT count(*) FROM turn_accounting", [], |row| row.get(0))
        .unwrap_or_default();
    assert_eq!(rows, 1);
    let marker: String = connection
        .query_row("SELECT value FROM fixture_marker", [], |row| row.get(0))
        .unwrap_or_default();
    assert_eq!(marker, "preserved");
}

#[test]
fn bounded_read_only_accounting_filters_utc_and_never_mutates_live_index() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let ledger = AccountingLedger::open(root.path())
        .unwrap_or_else(|error| panic!("ledger must open: {error}"));
    ledger
        .reconcile(&[
            accounting_entry(
                "first",
                1,
                0,
                "2026-07-09T23:59:59.999Z",
                Cost::Monetary {
                    amount_micros: 1,
                    currency: "USD".to_owned(),
                },
            ),
            accounting_entry(
                "second",
                1,
                0,
                "2026-07-10T00:00:00.000Z",
                Cost::SubscriptionQuota {
                    used: Some("1".to_owned()),
                    unit: Some("request".to_owned()),
                },
            ),
        ])
        .unwrap_or_else(|error| panic!("fixtures must reconcile: {error}"));
    drop(ledger);
    let paths = [
        root.path().join("index.sqlite"),
        root.path().join("index.sqlite-wal"),
        root.path().join("index.sqlite-shm"),
    ];
    let before = paths
        .iter()
        .map(|path| std::fs::read(path).ok())
        .collect::<Vec<_>>();
    let entries = AccountingLedger::entries_read_only_bounded(
        root.path(),
        &utc_timestamp("2026-07-10T00:00:00.000Z"),
        &utc_timestamp("2026-07-10T23:59:59.999Z"),
        10,
    )
    .unwrap_or_else(|error| panic!("read-only entries must load: {error}"));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].session_id, "second");
    let after = paths
        .iter()
        .map(|path| std::fs::read(path).ok())
        .collect::<Vec<_>>();
    assert_eq!(after, before);

    assert!(matches!(
        AccountingLedger::entries_read_only_bounded(
            root.path(),
            &utc_timestamp("2026-07-09T00:00:00.000Z"),
            &utc_timestamp("2026-07-10T23:59:59.999Z"),
            1,
        ),
        Err(SessionStoreError::AccountingResultTooLarge { max_entries: 1 })
    ));
}

#[test]
fn read_only_accounting_rejects_corrupt_typed_rows() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    AccountingLedger::open(root.path())
        .and_then(|ledger| {
            ledger.record(&accounting_entry(
                "corrupt",
                1,
                0,
                "2026-07-10T12:00:00.000Z",
                Cost::Unavailable {
                    reason: "fixture".to_owned(),
                },
            ))
        })
        .unwrap_or_else(|error| panic!("fixture must record: {error}"));
    rusqlite::Connection::open(root.path().join("index.sqlite"))
        .and_then(|connection| {
            connection.execute("UPDATE turn_accounting SET usage_json='not-json'", [])
        })
        .unwrap_or_else(|error| panic!("fixture must corrupt row: {error}"));
    assert!(
        AccountingLedger::entries_read_only_bounded(
            root.path(),
            &utc_timestamp("2026-07-10T00:00:00.000Z"),
            &utc_timestamp("2026-07-10T23:59:59.999Z"),
            10,
        )
        .is_err()
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn accounting_totals_cross_utc_day_without_erasing_nonpriced_costs() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let ledger = AccountingLedger::open(root.path())
        .unwrap_or_else(|error| panic!("ledger must open: {error}"));
    let entries = vec![
        accounting_entry(
            "session-a",
            1,
            0,
            "2026-01-01T23:59:30.000Z",
            Cost::Monetary {
                amount_micros: 100,
                currency: "USD".to_owned(),
            },
        ),
        accounting_entry(
            "session-a",
            2,
            1,
            "2026-01-02T00:00:10.000Z",
            Cost::AiCredits {
                credits_micros: 200,
                nominal_amount_micros: None,
                currency: None,
            },
        ),
        accounting_entry(
            "session-b",
            1,
            0,
            "2026-01-02T00:00:20.000Z",
            Cost::Monetary {
                amount_micros: 300,
                currency: "USD".to_owned(),
            },
        ),
        accounting_entry(
            "session-a",
            3,
            2,
            "2026-01-02T00:00:30.000Z",
            Cost::SubscriptionQuota {
                used: Some("1".to_owned()),
                unit: Some("request".to_owned()),
            },
        ),
        accounting_entry(
            "session-a",
            4,
            3,
            "2026-01-02T00:00:40.000Z",
            Cost::Unavailable {
                reason: "subscription pricing unavailable".to_owned(),
            },
        ),
        accounting_entry(
            "session-a",
            5,
            4,
            "2026-01-02T00:00:50.000Z",
            Cost::Monetary {
                amount_micros: 999,
                currency: "EUR".to_owned(),
            },
        ),
        accounting_entry(
            "session-b",
            2,
            1,
            "2026-01-02T00:02:00.000Z",
            Cost::Monetary {
                amount_micros: 5_000,
                currency: "USD".to_owned(),
            },
        ),
    ];
    ledger
        .reconcile(&entries)
        .unwrap_or_else(|error| panic!("entries must reconcile: {error}"));
    let totals = ledger
        .totals(
            "session-a",
            &utc_day("2026-01-02"),
            &utc_timestamp("2026-01-01T23:59:45.000Z"),
            &utc_timestamp("2026-01-02T00:00:59.999Z"),
        )
        .unwrap_or_else(|error| panic!("totals must query: {error}"));

    assert_eq!(
        totals.utc_day_start_utc.as_str(),
        "2026-01-02T00:00:00.000Z"
    );
    assert_eq!(totals.session_micros_usd, 100);
    assert_eq!(totals.day_micros_usd, 300);
    assert_eq!(totals.trailing_session_micros_usd, 0);
    assert_eq!(totals.trailing_all_sessions_micros_usd, 300);
    assert_eq!(totals.session_ai_credit_micros, 200);
    assert_eq!(totals.day_ai_credit_micros, 200);
    assert_eq!(totals.trailing_session_ai_credit_micros, 200);
    assert_eq!(totals.trailing_all_sessions_ai_credit_micros, 200);
    assert_eq!(totals.session_subscription_quota_turns, 1);
    assert_eq!(totals.day_subscription_quota_turns, 1);
    assert_eq!(totals.session_unavailable_turns, 1);
    assert_eq!(totals.day_unavailable_turns, 1);
    assert_eq!(totals.session_non_usd_monetary_turns, 1);
    assert_eq!(totals.day_non_usd_monetary_turns, 1);
}

#[test]
fn accounting_rebuild_is_idempotent_conflict_checked_and_rewind_independent() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let ledger = AccountingLedger::open(root.path())
        .unwrap_or_else(|error| panic!("ledger must open: {error}"));
    let paid = accounting_entry(
        "paid-session",
        1,
        4,
        "2026-02-03T04:05:06.007Z",
        Cost::Monetary {
            amount_micros: 41,
            currency: "USD".to_owned(),
        },
    );
    ledger
        .record(&paid)
        .and_then(|()| ledger.record(&paid))
        .unwrap_or_else(|error| panic!("duplicate projection must be idempotent: {error}"));
    assert_eq!(
        ledger
            .entries_for_session("paid-session")
            .unwrap_or_else(|error| panic!("entries must query: {error}")),
        vec![paid.clone()]
    );

    let mut conflicting = paid.clone();
    conflicting.cost = Cost::Monetary {
        amount_micros: 42,
        currency: "USD".to_owned(),
    };
    assert!(matches!(
        ledger.record(&conflicting),
        Err(SessionStoreError::AccountingConflict)
    ));

    let replacement = accounting_entry(
        "other-session",
        1,
        0,
        "2026-02-03T04:05:07.000Z",
        Cost::AiCredits {
            credits_micros: 9,
            nominal_amount_micros: None,
            currency: None,
        },
    );
    ledger
        .reconcile(&[])
        .unwrap_or_else(|error| panic!("empty rewind reconciliation must succeed: {error}"));
    assert_eq!(
        ledger
            .entries_for_session("paid-session")
            .unwrap_or_else(|error| panic!("paid history must query: {error}")),
        vec![paid.clone()]
    );

    ledger
        .reconcile(&[paid.clone(), replacement.clone()])
        .unwrap_or_else(|error| panic!("authoritative rebuild must replace rows: {error}"));
    assert_eq!(
        ledger
            .entries_for_session("paid-session")
            .unwrap_or_else(|error| panic!("rebuilt paid history must query: {error}")),
        vec![paid]
    );
    assert_eq!(
        ledger
            .entries_for_session("other-session")
            .unwrap_or_else(|error| panic!("replacement must query: {error}")),
        vec![replacement]
    );
}

#[test]
fn accounting_rejects_invalid_dates_future_window_rows_and_overflow() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let ledger = AccountingLedger::open(root.path())
        .unwrap_or_else(|error| panic!("ledger must open: {error}"));
    assert_eq!(
        UtcTimestamp::from_unix_millis(0)
            .unwrap_or_else(|error| panic!("epoch must convert: {error}"))
            .as_str(),
        "1970-01-01T00:00:00.000Z"
    );
    assert_eq!(
        UtcTimestamp::from_unix_millis(1_709_164_800_123)
            .unwrap_or_else(|error| panic!("leap day must convert: {error}"))
            .as_str(),
        "2024-02-29T00:00:00.123Z"
    );
    for timestamp in [
        "2025-02-29T00:00:00.000Z",
        "2024-02-30T00:00:00.000Z",
        "2024-13-01T00:00:00.000Z",
        "2024-01-01T24:00:00.000Z",
        "2024-01-01T00:60:00.000Z",
    ] {
        assert!(matches!(
            UtcTimestamp::parse(timestamp),
            Err(SessionStoreError::InvalidAccountingTimestamp)
        ));
    }

    let entries = [
        accounting_entry(
            "overflow",
            1,
            0,
            "2026-03-01T00:00:00.000Z",
            Cost::Monetary {
                amount_micros: u64::MAX,
                currency: "USD".to_owned(),
            },
        ),
        accounting_entry(
            "overflow",
            2,
            1,
            "2026-03-01T00:00:01.000Z",
            Cost::Monetary {
                amount_micros: 1,
                currency: "USD".to_owned(),
            },
        ),
    ];
    ledger
        .reconcile(&entries)
        .unwrap_or_else(|error| panic!("valid rows must reconcile: {error}"));
    assert!(matches!(
        ledger.totals(
            "overflow",
            &utc_day("2026-03-01"),
            &utc_timestamp("2026-03-01T00:00:00.000Z"),
            &utc_timestamp("2026-03-01T00:00:59.999Z"),
        ),
        Err(SessionStoreError::AccountingOverflow)
    ));

    let future_root =
        tempdir().unwrap_or_else(|error| panic!("future fixture tempdir must create: {error}"));
    let future_ledger = AccountingLedger::open(future_root.path())
        .unwrap_or_else(|error| panic!("future ledger must open: {error}"));
    future_ledger
        .record(&accounting_entry(
            "future",
            1,
            0,
            "2026-03-01T00:10:00.000Z",
            Cost::Monetary {
                amount_micros: 999,
                currency: "USD".to_owned(),
            },
        ))
        .unwrap_or_else(|error| panic!("future row must record: {error}"));
    let future_totals = future_ledger
        .totals(
            "future",
            &utc_day("2026-03-01"),
            &utc_timestamp("2026-03-01T00:00:00.000Z"),
            &utc_timestamp("2026-03-01T00:00:59.999Z"),
        )
        .unwrap_or_else(|error| panic!("future totals must query: {error}"));
    assert_eq!(future_totals.trailing_session_micros_usd, 0);
    assert_eq!(future_totals.trailing_all_sessions_micros_usd, 0);
    assert_eq!(future_totals.session_micros_usd, 0);
}

#[test]
fn accounting_serializes_concurrent_session_writers() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    drop(
        AccountingLedger::open(root.path())
            .unwrap_or_else(|error| panic!("ledger schema must initialize: {error}")),
    );
    let first_root = root.path().to_owned();
    let second_root = root.path().to_owned();
    let first = std::thread::spawn(move || {
        let ledger = AccountingLedger::open(&first_root)
            .unwrap_or_else(|error| panic!("first ledger must open: {error}"));
        for sequence in 0..32 {
            ledger
                .record(&accounting_entry(
                    "concurrent-a",
                    sequence + 1,
                    sequence,
                    "2026-04-01T00:00:00.000Z",
                    Cost::Monetary {
                        amount_micros: 1,
                        currency: "USD".to_owned(),
                    },
                ))
                .unwrap_or_else(|error| panic!("first writer must record: {error}"));
        }
    });
    let second = std::thread::spawn(move || {
        let ledger = AccountingLedger::open(&second_root)
            .unwrap_or_else(|error| panic!("second ledger must open: {error}"));
        for sequence in 0..32 {
            ledger
                .record(&accounting_entry(
                    "concurrent-b",
                    sequence + 1,
                    sequence,
                    "2026-04-01T00:00:01.000Z",
                    Cost::AiCredits {
                        credits_micros: 2,
                        nominal_amount_micros: None,
                        currency: None,
                    },
                ))
                .unwrap_or_else(|error| panic!("second writer must record: {error}"));
        }
    });
    first
        .join()
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload));
    second
        .join()
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload));

    let totals = AccountingLedger::open(root.path())
        .and_then(|ledger| {
            ledger.totals(
                "concurrent-a",
                &utc_day("2026-04-01"),
                &utc_timestamp("2026-04-01T00:00:00.000Z"),
                &utc_timestamp("2026-04-01T00:00:59.999Z"),
            )
        })
        .unwrap_or_else(|error| panic!("concurrent totals must query: {error}"));
    assert_eq!(totals.session_micros_usd, 32);
    assert_eq!(totals.day_micros_usd, 32);
    assert_eq!(totals.day_ai_credit_micros, 64);
    assert_eq!(totals.trailing_all_sessions_micros_usd, 32);
    assert_eq!(totals.trailing_all_sessions_ai_credit_micros, 64);
}
