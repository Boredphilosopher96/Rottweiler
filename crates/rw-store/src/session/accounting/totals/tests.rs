#![allow(clippy::expect_used)]
use super::*;
use rw_types::{AccountingAttribution, SequenceId, TurnId, Usage};

fn entry(session: &str, sequence: u64, time: &str, cost: Cost) -> TurnAccountingEntry {
    let emitted_at_utc = UtcTimestamp::parse(time).expect("timestamp");
    TurnAccountingEntry {
        session_id: session.into(),
        turn_id: TurnId(sequence.to_string()),
        sequence_id: SequenceId(sequence),
        utc_day: emitted_at_utc.utc_day(),
        emitted_at_utc,
        attribution: AccountingAttribution::Main,
        usage: Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        },
        cost,
    }
}
fn dollars(amount: u64) -> Cost {
    Cost::Monetary {
        amount_micros: amount,
        currency: "USD".into(),
    }
}
fn query(
    ledger: &AccountingLedger,
    session: &str,
    day: &str,
    start: &str,
    end: &str,
) -> AccountingTotals {
    ledger
        .totals(
            session,
            &UtcDayKey::parse(day).expect("day"),
            &UtcTimestamp::parse(start).expect("start"),
            &UtcTimestamp::parse(end).expect("end"),
        )
        .expect("totals")
}
#[test]
fn time_keys_preserve_calendar_order_and_fit_the_tree() {
    let values = [
        "0001-01-01T00:00:00.000Z",
        "1969-12-31T23:59:59.999Z",
        "1970-01-01T00:00:00.000Z",
        "2024-02-29T23:59:59.999Z",
        "2024-03-01T00:00:00.000Z",
        "9999-12-31T23:59:59.999Z",
    ];
    let keys = values.map(|value| time_key(&UtcTimestamp::parse(value).expect("valid")));
    assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(keys.last().is_some_and(|key| *key < TIME_ROOT));
}
#[test]
fn range_subtraction_is_exact_after_large_global_history() {
    let root = tempfile::tempdir().expect("root");
    let ledger = AccountingLedger::open(root.path()).expect("ledger");
    ledger
        .reconcile(&[
            entry("old", 0, "2020-01-01T00:00:00.000Z", dollars(u64::MAX)),
            entry("old", 1, "2020-01-01T00:00:00.001Z", dollars(u64::MAX)),
            entry("chosen", 0, "2026-09-05T01:00:00.000Z", dollars(7)),
            entry("other", 0, "2026-09-05T01:00:00.001Z", dollars(11)),
            entry("chosen", 1, "2026-09-05T01:00:00.002Z", dollars(100)),
        ])
        .expect("facts");
    let totals = query(
        &ledger,
        "chosen",
        "2026-09-05",
        "2026-09-05T01:00:00.000Z",
        "2026-09-05T01:00:00.001Z",
    );
    assert_eq!(totals.session_micros_usd, 7);
    assert_eq!(totals.day_micros_usd, 18);
    assert_eq!(totals.trailing_session_micros_usd, 7);
    assert_eq!(totals.trailing_all_sessions_micros_usd, 18);
}
#[test]
fn duplicate_and_conflicting_reconciliation_cannot_change_totals() {
    let root = tempfile::tempdir().expect("root");
    let ledger = AccountingLedger::open(root.path()).expect("ledger");
    let fact = entry("chosen", 0, "2026-09-05T01:00:00.000Z", dollars(7));
    ledger.record(&fact).expect("record");
    ledger.record(&fact).expect("duplicate");
    let mut conflicting = fact.clone();
    conflicting.cost = dollars(99);
    assert!(
        ledger
            .reconcile(&[
                entry("chosen", 1, "2026-09-05T01:00:00.001Z", dollars(11)),
                conflicting
            ])
            .is_err()
    );
    assert_eq!(
        query(
            &ledger,
            "chosen",
            "2026-09-05",
            "2026-09-05T00:00:00.000Z",
            "2026-09-05T23:59:59.999Z"
        )
        .session_micros_usd,
        7
    );
}
#[test]
fn derived_rebuild_is_paged_resumable_and_never_exposes_partial_totals() {
    let root = tempfile::tempdir().expect("root");
    let ledger = AccountingLedger::open(root.path()).expect("ledger");
    let facts = (0..300)
        .map(|index| entry("chosen", index, "2026-09-05T01:00:00.000Z", dollars(1)))
        .collect::<Vec<_>>();
    ledger.reconcile(&facts).expect("facts");
    let mut connection = ledger.connection().expect("connection");
    connection.execute_batch("DELETE FROM accounting_totals; UPDATE accounting_totals_progress SET projected_rowid=0;").expect("discard derived rows");
    assert!(catch_up_page(&mut connection).expect("first page"));
    assert_eq!(watermark(&connection).expect("watermark"), 128);
    assert!(matches!(
        ledger.totals(
            "chosen",
            &UtcDayKey::parse("2026-09-05").expect("day"),
            &UtcTimestamp::parse("2026-09-05T00:00:00.000Z").expect("start"),
            &UtcTimestamp::parse("2026-09-05T23:59:59.999Z").expect("end")
        ),
        Err(SessionStoreError::IncompleteAccountingTotals)
    ));
    drop(connection);
    let reopened = AccountingLedger::open(root.path()).expect("resume bounded rebuild");
    assert_eq!(
        query(
            &reopened,
            "chosen",
            "2026-09-05",
            "2026-09-05T00:00:00.000Z",
            "2026-09-05T23:59:59.999Z"
        )
        .session_micros_usd,
        300
    );
    assert_eq!(
        reopened
            .entries_bounded(Some("chosen"), 4096)
            .expect("authority")
            .len(),
        300
    );
}

#[test]
fn decoded_utc_boundaries_enforce_their_calendar_invariant() {
    for value in [
        "",
        "2026-02-29",
        "2026-09-05T00:00:00Z",
        "2026-09-05T25:00:00.000Z",
    ] {
        assert!(serde_json::from_value::<UtcTimestamp>(serde_json::json!(value)).is_err());
    }
    assert!(serde_json::from_str::<UtcDayKey>("\"2026-02-29\"").is_err());
    assert!(serde_json::from_str::<UtcTimestamp>("\"2024-02-29T00:00:00.000Z\"").is_ok());
}

#[test]
fn accounting_rejects_oversized_dispositions_before_persisting() {
    let root = tempfile::tempdir().expect("root");
    let ledger = AccountingLedger::open(root.path()).expect("ledger");
    let fact = entry(
        "chosen",
        0,
        "2026-09-05T01:00:00.000Z",
        Cost::Unavailable {
            reason: "\0".repeat(MAX_COST_BYTES / 6 + 1),
        },
    );
    assert!(matches!(
        ledger.record(&fact),
        Err(SessionStoreError::AccountingEntryTooLarge)
    ));
    assert!(
        ledger
            .entries_bounded(Some("chosen"), 4096)
            .expect("facts")
            .is_empty()
    );
}
