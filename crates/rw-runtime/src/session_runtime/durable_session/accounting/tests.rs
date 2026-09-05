#![cfg(test)]
#![allow(clippy::expect_used)]
use super::super::DurableEventSink;
use crate::journal_service::JournalService;
use rw_core::{SessionEventSink, SessionUsage, commit_session_events};
use rw_store::session::{AccountingLedger, SessionEventLog};
use rw_types::{Cost, EngineEvent, EventMeta, SequenceId, SessionId, TurnId, TurnStatus};
use std::sync::{Arc, atomic::Ordering};

fn meta(sequence: u64) -> EventMeta {
    EventMeta {
        protocol_version: rw_core::SESSION_EVENT_VERSION,
        session_id: SessionId("accounting".into()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-09-05T00:00:00.000Z".into(),
        caused_by: None,
    }
}
fn finished(sequence: u64, turn: u64) -> EngineEvent {
    EngineEvent::TurnFinished {
        meta: meta(sequence),
        turn_id: TurnId(turn.to_string()),
        status: TurnStatus::Completed,
        usage: SessionUsage::default().into(),
        cost: Cost::Monetary {
            amount_micros: 7,
            currency: "USD".into(),
        },
    }
}
fn open(root: &std::path::Path) -> Arc<DurableEventSink> {
    let sink = DurableEventSink::new(
        SessionEventLog::open(root, "accounting").expect("journal"),
        root.to_owned(),
        "accounting".into(),
        JournalService::new(root).expect("service"),
    )
    .expect("sink");
    sink.configure_canonical(
        Arc::new(rw_ext::ModeRegistry::builtins().expect("modes")),
        None,
    )
    .expect("canonical");
    sink
}

#[tokio::test]
async fn indexed_accounting_reconciles_multiple_pages_and_reopens_at_exact_prefix() {
    let root = tempfile::tempdir().expect("root");
    let mut log = SessionEventLog::open(root.path(), "accounting").expect("journal");
    for page in 0..3 {
        log.append_batch((0..64).map(|offset| {
            let index = page * 64 + offset;
            finished(index, index + 1)
        }))
        .expect("source page");
    }
    let expected = log.read_view().prefix_identity();
    drop(log);
    let sink = open(root.path());
    sink.reconcile_indexed_accounting()
        .await
        .expect("reconcile");
    let ledger = AccountingLedger::open(root.path()).expect("ledger");
    assert_eq!(
        ledger
            .entries_for_session("accounting")
            .expect("facts")
            .len(),
        192
    );
    assert_eq!(
        ledger.reconciled_prefix("accounting").expect("cursor"),
        Some(expected)
    );
    sink.settle_effects().await.expect("settled");
    drop(sink);
    let reopened = open(root.path());
    reopened
        .reconcile_indexed_accounting()
        .await
        .expect("reopen reconciliation");
    assert_eq!(
        ledger
            .entries_for_session("accounting")
            .expect("facts")
            .len(),
        192
    );
    assert_eq!(
        ledger.reconciled_prefix("accounting").expect("cursor"),
        Some(expected)
    );
}

#[tokio::test]
async fn text_batches_do_not_checkpoint_accounting_until_the_billed_boundary() {
    let root = tempfile::tempdir().expect("root");
    let sink = open(root.path());
    let ledger = AccountingLedger::open(root.path()).expect("ledger");
    commit_session_events(
        Arc::clone(&sink),
        vec![EngineEvent::TurnStarted {
            meta: meta(0),
            turn_id: TurnId("1".into()),
        }],
    )
    .await
    .expect("start");
    for sequence in 1..=8 {
        commit_session_events(
            Arc::clone(&sink),
            vec![EngineEvent::TextDelta {
                meta: meta(sequence),
                turn_id: TurnId("1".into()),
                text: "token".into(),
            }],
        )
        .await
        .expect("text");
    }
    assert!(
        ledger
            .reconciled_prefix("accounting")
            .expect("no checkpoint per token")
            .is_none()
    );
    commit_session_events(Arc::clone(&sink), vec![finished(9, 1)])
        .await
        .expect("billed boundary");
    assert_eq!(
        ledger.reconciled_prefix("accounting").expect("cursor"),
        Some(sink.registration.publisher.capture().prefix_identity())
    );
    assert_eq!(
        ledger
            .entries_for_session("accounting")
            .expect("facts")
            .len(),
        1
    );
}

#[tokio::test]
async fn cancelled_reconciliation_waiter_keeps_dirty_accounting_pending() {
    let root = tempfile::tempdir().expect("root");
    let sink = open(root.path());
    sink.accounting_dirty.store(true, Ordering::Release);
    let guard = Arc::clone(&sink.accounting_progress).lock_owned().await;
    let query = tokio::spawn({
        let sink = Arc::clone(&sink);
        async move { sink.reconcile_indexed_accounting().await }
    });
    tokio::task::yield_now().await;
    query.abort();
    assert!(query.await.expect_err("cancelled").is_cancelled());
    assert!(sink.accounting_dirty.load(Ordering::Acquire));
    drop(guard);
    sink.reconcile_indexed_accounting().await.expect("repair");
    assert!(!sink.accounting_dirty.load(Ordering::Acquire));
}
