#![allow(clippy::expect_used)]
use super::*;

#[tokio::test]
async fn small_live_contexts_and_overlapping_queries_share_the_actual_budget() {
    let budget = HistoryRetentions::new();
    let mut resident = Vec::new();
    for _ in 0..4 {
        let mut owner = budget.working();
        owner.resize(21 * 1024 * 1024).expect("context plan");
        resident.push(owner);
    }
    let mut queries = Vec::new();
    for _ in 0..3 {
        queries.push(budget.query().await.expect("read"));
    }
    let waiting = budget.query();
    tokio::pin!(waiting);
    assert!(matches!(
        futures_util::poll!(&mut waiting),
        std::task::Poll::Pending
    ));
    queries[0].resize(1024).expect("delivered small result");
    let delivered = waiting.await.expect("read advances after delivery");
    assert_eq!(
        budget.0.usage.lock().expect("usage").resident,
        84 * 1024 * 1024 + UNIT_BYTES
    );
    drop((resident, queries, delivered));
    let usage = budget.0.usage.lock().expect("usage");
    assert_eq!(usage.resident + usage.query, 0);
}

#[tokio::test]
async fn resident_ceiling_preserves_query_progress_and_failed_transfer_keeps_its_owner() {
    let budget = HistoryRetentions::new();
    let mut resident = Vec::new();
    for _ in 0..3 {
        let mut owner = budget.working();
        owner
            .resize(MAX_HISTORY_RESULT_BYTES)
            .expect("resident capacity");
        resident.push(owner);
    }
    let mut query = budget.query().await.expect("protected query capacity");
    assert!(query.resize(1).is_err());
    assert_eq!(
        budget.0.usage.lock().expect("usage").query,
        MAX_HISTORY_RESULT_BYTES
    );
    drop(resident.pop());
    query.resize(1).expect("transfer after resident release");
    assert_eq!(budget.0.usage.lock().expect("usage").query, 0);
    assert!(query.resize(MAX_HISTORY_RESULT_BYTES + 1).is_err());
    assert_eq!(query.bytes, UNIT_BYTES);
}

#[tokio::test]
async fn cancelled_waiter_releases_queue_position_without_claiming_bytes() {
    let budget = HistoryRetentions::new();
    let mut active = Vec::new();
    for _ in 0..4 {
        active.push(budget.query().await.expect("query capacity"));
    }
    let mut queued = Vec::new();
    for _ in 0..MAX_WAITERS {
        let mut waiting = Box::pin(budget.query());
        assert!(matches!(
            futures_util::poll!(&mut waiting),
            std::task::Poll::Pending
        ));
        queued.push(waiting);
    }
    assert!(budget.query().await.is_err());
    drop(queued.remove(0));
    assert_eq!(budget.0.waiters.available_permits(), 1);
    drop(active.pop());
    let mut first = queued.remove(0);
    assert!(matches!(
        futures_util::poll!(&mut first),
        std::task::Poll::Ready(Ok(_))
    ));
    drop(queued);
    assert_eq!(budget.0.waiters.available_permits(), MAX_WAITERS);
}
