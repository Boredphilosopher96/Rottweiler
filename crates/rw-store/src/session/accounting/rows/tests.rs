#![allow(clippy::expect_used)]
use super::*;
use crate::session::AccountingLedger;
use rw_types::{AccountingAttribution, Cost, Usage};

fn fact(sequence: u64, reason: &str) -> TurnAccountingEntry {
    let emitted_at_utc = UtcTimestamp::parse("2026-09-05T01:00:00.000Z").expect("time");
    TurnAccountingEntry {
        session_id: "chosen".into(),
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
        cost: Cost::Unavailable {
            reason: reason.into(),
        },
    }
}

#[test]
fn aggregate_admission_applies_to_live_and_read_only_accounting() {
    let root = tempfile::tempdir().expect("root");
    let ledger = AccountingLedger::open(root.path()).expect("ledger");
    let reason = "x".repeat(512 * 1024);
    for sequence in 0..17 {
        ledger
            .record(&fact(sequence, &reason))
            .expect("admitted fact");
    }
    assert!(matches!(
        ledger.entries_bounded(Some("chosen"), 100),
        Err(SessionStoreError::AccountingReadTooLarge)
    ));
    assert!(matches!(
        AccountingLedger::entries_read_only_bounded(
            root.path(),
            &UtcTimestamp::parse("2026-09-05T00:00:00.000Z").expect("start"),
            &UtcTimestamp::parse("2026-09-05T23:59:59.999Z").expect("end"),
            100,
        ),
        Err(SessionStoreError::AccountingReadTooLarge)
    ));
}

#[test]
fn row_admission_precedes_json_decoding_and_rejects_noncanonical_facts() {
    for (column, value) in [
        ("turn_id", "x".repeat(129)),
        ("sequence_id", "00".into()),
        ("utc_day", "2026-09-04".into()),
        (
            "cost_json",
            "x".repeat(super::super::totals::MAX_COST_BYTES + 1),
        ),
    ] {
        let root = tempfile::tempdir().expect("root");
        let ledger = AccountingLedger::open(root.path()).expect("ledger");
        ledger.record(&fact(0, "fixture")).expect("fact");
        ledger
            .connection()
            .expect("connection")
            .execute(&format!("UPDATE turn_accounting SET {column}=?1"), [&value])
            .expect("corrupt selected fact");
        assert!(ledger.entries_bounded(None, 1).is_err(), "{column}");
    }
}

#[test]
fn inspection_requires_an_explicit_complete_result_allowance() {
    let root = tempfile::tempdir().expect("root");
    let ledger = AccountingLedger::open(root.path()).expect("ledger");
    ledger.record(&fact(0, "fixture")).expect("fact");
    assert!(matches!(
        ledger.entries_bounded(None, 0),
        Err(SessionStoreError::AccountingResultTooLarge { max_entries: 0 })
    ));
    assert!(
        ledger
            .entries_bounded(Some("unrelated"), 0)
            .expect("empty selection")
            .is_empty()
    );
    assert_eq!(
        ledger.entries_bounded(None, 1).expect("complete selection"),
        vec![fact(0, "fixture")]
    );
}
