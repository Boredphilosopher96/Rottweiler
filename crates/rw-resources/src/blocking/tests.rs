#![allow(clippy::expect_used)]
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test]
async fn abandoned_admission_drops_work_without_starting_or_retaining_a_waiter() {
    let pool = Pool::new(1, 1);
    let active = pool.try_acquire().expect("active worker");
    let source = Arc::new(());
    let weak = Arc::downgrade(&source);
    let ran = Arc::new(AtomicBool::new(false));
    let observed = ran.clone();
    let mut waiting = Box::pin(run(&pool, ResourceClass::Cpu, move || {
        drop(source);
        ran.store(true, Ordering::SeqCst);
    }));
    tokio::select! {
        biased;
        result = &mut waiting => panic!("unexpected admission: {result:?}"),
        () = tokio::task::yield_now() => {}
    }
    assert_eq!(pool.waiting.available_permits(), 0);
    assert!(weak.upgrade().is_some());
    drop(waiting);
    assert!(weak.upgrade().is_none());
    assert!(!observed.load(Ordering::SeqCst));
    assert_eq!(pool.waiting.available_permits(), 1);
    assert_eq!(pool.execution.available_permits(), 0);
    drop(active);
}

struct RetainedResult {
    pool: Arc<Pool>,
    dropped: Option<tokio::sync::oneshot::Sender<usize>>,
}
impl Drop for RetainedResult {
    fn drop(&mut self) {
        if let Some(sender) = self.dropped.take() {
            let _ = sender.send(self.pool.execution.available_permits());
        }
    }
}

#[tokio::test]
async fn cancelled_waiter_keeps_work_and_discarded_result_inside_physical_lease() {
    let pool = Arc::new(Pool::new(1, 1));
    let worker_pool = pool.clone();
    let source = Arc::new(());
    let weak = Arc::downgrade(&source);
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (dropped, discarded) = tokio::sync::oneshot::channel();
    let waiter = tokio::spawn(async move {
        let result_pool = worker_pool.clone();
        run(&worker_pool, ResourceClass::Cpu, move || {
            let _source = source;
            let _ = entered.send(());
            released.recv().expect("physical release");
            RetainedResult {
                pool: result_pool,
                dropped: Some(dropped),
            }
        })
        .await
    });
    entry.await.expect("worker entered");
    waiter.abort();
    assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
    assert!(weak.upgrade().is_some());
    assert_eq!(pool.execution.available_permits(), 0);
    release.send(()).expect("release worker");
    assert_eq!(discarded.await.expect("actual result destruction"), 0);
    let settled = pool
        .acquire(std::future::pending())
        .await
        .expect("physical settlement");
    assert!(weak.upgrade().is_none());
    drop(settled);
}

#[tokio::test]
async fn panic_remains_a_tokio_join_error_and_refunds_physical_capacity() {
    let pool = Pool::new(1, 1);
    let error = run::<()>(&pool, ResourceClass::Blocking, || {
        panic!("worker panic oracle")
    })
    .await
    .expect_err("worker panic");
    let WorkError::Worker(error) = error else {
        panic!("panic was not preserved")
    };
    assert!(error.is_panic());
    assert_eq!(
        error.into_panic().downcast_ref::<&str>(),
        Some(&"worker panic oracle")
    );
    assert_eq!(pool.execution.available_permits(), 1);
    assert_eq!(pool.waiting.available_permits(), 1);
}

#[tokio::test]
async fn typed_output_survives_join_without_occupying_execution_capacity() {
    let pool = Arc::new(Pool::new(1, 1));
    let (dropped, discarded) = tokio::sync::oneshot::channel();
    let worker_pool = pool.clone();
    let output = run(&pool, ResourceClass::Cpu, move || RetainedResult {
        pool: worker_pool,
        dropped: Some(dropped),
    })
    .await
    .expect("typed output");
    assert_eq!(pool.execution.available_permits(), 1);
    assert_eq!(pool.waiting.available_permits(), 1);
    drop(output);
    assert_eq!(discarded.await.expect("caller destroyed result"), 1);
}
