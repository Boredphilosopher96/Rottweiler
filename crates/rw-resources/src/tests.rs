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

#[test]
fn unproven_settlement_cannot_return_execution_capacity() {
    let pool = Pool::new(1, 1);
    pool.try_acquire().expect("physical owner").quarantine();
    assert!(matches!(pool.try_acquire(), Err(AdmissionError::Busy)));
    assert_eq!(pool.execution.available_permits(), 0);
}

#[tokio::test]
async fn multi_group_operation_reserves_all_capacity_before_starting_effects() {
    let pool = Pool::new(2, 1);
    let first = pool.try_acquire().expect("another process");
    let mut command = Box::pin(pool.acquire_units(2, pending()));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut command)
            .await
            .is_err()
    );
    assert_eq!(
        pool.execution.available_permits(),
        0,
        "waiting pair owns its partial reservation"
    );
    drop(command);
    assert_eq!(
        pool.execution.available_permits(),
        1,
        "caller loss returns partial reservation"
    );
    drop(first);
    let pair = pool
        .acquire_units(2, pending())
        .await
        .expect("whole process demand");
    assert!(matches!(pool.try_acquire(), Err(AdmissionError::Busy)));
    drop(pair);
    assert_eq!(pool.execution.available_permits(), 2);
    assert!(matches!(
        pool.acquire_units(3, pending()).await,
        Err(AdmissionError::InvalidDemand)
    ));
}
