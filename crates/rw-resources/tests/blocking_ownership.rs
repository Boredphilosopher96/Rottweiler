#![allow(clippy::expect_used)]
use rw_resources::{ResourceClass, run_blocking, try_acquire};
use std::time::Duration;

#[tokio::test]
async fn caller_abort_keeps_capacity_until_the_blocking_operation_exits() {
    let (started, begun) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let caller = tokio::spawn(run_blocking(ResourceClass::Blocking, move || {
        started.send(()).expect("worker started");
        released.recv().expect("release physical operation");
    }));
    begun.await.expect("real worker began");
    caller.abort();
    assert!(caller.await.expect_err("caller cancelled").is_cancelled());
    let mut competing = Vec::new();
    while let Ok(lease) = try_acquire(ResourceClass::Blocking) {
        competing.push(lease);
    }
    assert_eq!(competing.len(), 15, "live operation still owns one slot");
    release.send(()).expect("complete operation");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(lease) = try_acquire(ResourceClass::Blocking) {
                drop(lease);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("capacity returns only after physical completion");
}
