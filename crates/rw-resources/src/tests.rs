#![allow(clippy::expect_used)]
use super::*;
use std::future::pending;

#[tokio::test]
async fn competing_owners_share_capacity_and_abandoned_waiters_release_their_slot() {
    let pool = Arc::new(Pool::new(1, 1));
    let first = pool.try_acquire().expect("first physical owner");
    let competing = pool.clone();
    let mut waiting = Box::pin(competing.acquire(pending()));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut waiting)
            .await
            .is_err()
    );
    assert!(matches!(
        pool.acquire(pending()).await,
        Err(AdmissionError::QueueFull)
    ));
    drop(waiting);
    assert_eq!(pool.waiting.available_permits(), 1);
    assert!(matches!(pool.try_acquire(), Err(AdmissionError::Busy)));
    drop(first);
    let second = pool
        .acquire(pending())
        .await
        .expect("settled owner returned capacity");
    assert_eq!(pool.execution.available_permits(), 0);
    drop(second);
    assert_eq!(pool.execution.available_permits(), 1);
}

#[tokio::test]
async fn cancellation_wins_before_grant_and_never_consumes_execution() {
    let pool = Pool::new(1, 1);
    assert!(matches!(
        pool.acquire(async {}).await,
        Err(AdmissionError::Cancelled)
    ));
    assert_eq!(pool.execution.available_permits(), 1);
    assert_eq!(pool.waiting.available_permits(), 1);
}

#[tokio::test]
async fn detached_physical_worker_retains_capacity_after_caller_loss() {
    let pool = Arc::new(Pool::new(1, 1));
    let lease = pool.try_acquire().expect("admission");
    let (settle, settled) = tokio::sync::oneshot::channel();
    let (finished, completion) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        let _ = settled.await;
        drop(lease);
        let _ = finished.send(());
    });
    drop(worker);
    assert!(matches!(pool.try_acquire(), Err(AdmissionError::Busy)));
    settle.send(()).expect("real completion");
    completion.await.expect("worker ended");
    assert!(pool.try_acquire().is_ok());
}

#[test]
fn process_pools_are_shared_by_class_and_independent_between_classes() {
    assert!(std::ptr::eq(
        pool(ResourceClass::Process),
        pool(ResourceClass::Process)
    ));
    assert!(!std::ptr::eq(
        pool(ResourceClass::Process),
        pool(ResourceClass::Network)
    ));
}
