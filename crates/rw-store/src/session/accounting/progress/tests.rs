#![cfg(test)]
#![allow(clippy::expect_used)]
use crate::session::{
    AccountingLedger, SessionStoreError, TurnAccountingEntry, UtcTimestamp,
    journal::SegmentedJournal,
};
use rw_types::{AccountingAttribution, Cost, SequenceId, TurnId, Usage};

fn entry(sequence: u64, amount: u64) -> TurnAccountingEntry {
    let time = UtcTimestamp::parse("2026-09-05T00:00:00.000Z").expect("time");
    TurnAccountingEntry {
        session_id: "accounted".into(),
        turn_id: TurnId(sequence.to_string()),
        sequence_id: SequenceId(sequence),
        utc_day: time.utc_day(),
        emitted_at_utc: time,
        attribution: AccountingAttribution::Main,
        usage: Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        },
        cost: Cost::Monetary {
            amount_micros: amount,
            currency: "USD".into(),
        },
    }
}

#[test]
fn accounting_progress_commits_with_facts_and_conflict_rolls_back_both() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "accounted").expect("journal");
    journal.append_batch(["first"]).expect("append");
    let ledger = AccountingLedger::open(root.path()).expect("ledger");
    let first = journal.read_view();
    ledger
        .reconcile_prefix("accounted", None, &first, &[entry(0, 1)])
        .expect("first page");
    assert_eq!(
        ledger.reconciled_prefix("accounted").expect("cursor"),
        Some(first.prefix_identity())
    );
    journal.append_batch(["second", "third"]).expect("append");
    ledger
        .record(&entry(2, 9))
        .expect("independently reconciled fact");
    assert!(matches!(
        ledger.reconcile_prefix(
            "accounted",
            Some(first.prefix_identity()),
            &journal.read_view(),
            &[entry(1, 2), entry(2, 3)]
        ),
        Err(SessionStoreError::AccountingConflict)
    ));
    assert_eq!(
        ledger
            .entries_for_session("accounted")
            .expect("facts")
            .len(),
        2
    );
    assert_eq!(
        ledger.reconciled_prefix("accounted").expect("cursor"),
        Some(first.prefix_identity())
    );
    ledger
        .reconcile_prefix(
            "accounted",
            Some(first.prefix_identity()),
            &journal.read_view(),
            &[entry(1, 2), entry(2, 9)],
        )
        .expect("repaired page");
    let reopened = AccountingLedger::open(root.path()).expect("reopen");
    assert_eq!(
        reopened.reconciled_prefix("accounted").expect("cursor"),
        Some(journal.read_view().prefix_identity())
    );
    assert!(matches!(
        reopened.reconcile_prefix(
            "accounted",
            Some(first.prefix_identity()),
            &journal.read_view(),
            &[entry(1, 2), entry(2, 9)]
        ),
        Err(SessionStoreError::AccountingConflict)
    ));
}

#[test]
fn accounting_progress_rejects_foreign_view_and_changed_prefix_digest() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "accounted").expect("journal");
    journal.append_batch(["same"]).expect("append");
    let mut foreign = SegmentedJournal::open(root.path(), "foreign").expect("foreign");
    foreign.append_batch(["same"]).expect("append");
    let ledger = AccountingLedger::open(root.path()).expect("ledger");
    assert!(
        ledger
            .reconcile_prefix("accounted", None, &foreign.read_view(), &[])
            .is_err()
    );
    ledger
        .reconcile_prefix("accounted", None, &journal.read_view(), &[entry(0, 1)])
        .expect("initial");
    let mut changed = journal.read_view().prefix_identity();
    changed.digest[0] ^= 1;
    assert!(
        ledger
            .reconcile_prefix("accounted", Some(changed), &journal.read_view(), &[])
            .is_err()
    );
    assert_eq!(
        ledger
            .entries_for_session("accounted")
            .expect("facts")
            .len(),
        1
    );
}
