#![allow(clippy::expect_used)]
use super::ReadOperations;
use std::{sync::Arc, time::Duration};

#[tokio::test(flavor = "current_thread")]
async fn dropped_read_waiter_keeps_worker_and_retained_owner_until_completion() {
    let reads = ReadOperations::new();
    let retained = Arc::new(());
    let weak = Arc::downgrade(&retained);
    let (signal, started) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let query = tokio::spawn({
        let reads = Arc::clone(&reads);
        async move {
            reads
                .run(retained, move |_| {
                    signal.send(()).expect("worker started");
                    wait.recv_timeout(Duration::from_secs(5))
                        .expect("release worker");
                    Ok(())
                })
                .await
        }
    });
    started.await.expect("started");
    query.abort();
    assert!(query.await.expect_err("aborted").is_cancelled());
    assert!(weak.upgrade().is_some());
    assert_eq!(reads.active(), 1);
    // A current-thread runtime still polls timers while the owned worker blocks.
    assert!(
        tokio::time::timeout(Duration::from_millis(30), reads.settle())
            .await
            .is_err()
    );
    release.send(()).expect("release");
    tokio::time::timeout(Duration::from_secs(2), reads.settle())
        .await
        .expect("settlement completed")
        .expect("settled");
    tokio::time::timeout(Duration::from_secs(2), async {
        while weak.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker released retained owner");
}

#[tokio::test(flavor = "current_thread")]
async fn read_panic_fails_settlement_and_future_admission() {
    let reads = ReadOperations::new();
    assert!(
        reads
            .run((), |()| -> Result<(), rw_core::AgentLoopError> {
                panic!("controlled reader panic");
            })
            .await
            .is_err()
    );
    assert!(reads.settle().await.is_err());
    assert!(reads.run((), |()| Ok(())).await.is_err());
    assert_eq!(reads.active(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn completed_reply_keeps_its_capacity_until_the_waiter_is_polled() {
    use std::{future::Future, task::Poll};
    let reads = ReadOperations::new();
    let retained = Arc::new(());
    let weak = Arc::downgrade(&retained);
    let (finished, done) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let mut pending = Box::pin(reads.run(retained, move |_| {
        wait.recv_timeout(Duration::from_secs(5))
            .expect("release reply");
        finished.send(()).expect("finished");
        Ok(())
    }));
    std::future::poll_fn(|context| {
        assert!(pending.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    release.send(()).expect("release");
    done.await.expect("finished");
    reads.settle().await.expect("worker settled");
    assert!(weak.upgrade().is_some());
    assert_eq!(
        reads.admission.available_permits(),
        super::MAX_SESSION_READ_JOBS - 1
    );
    pending.await.expect("delivered");
    assert!(weak.upgrade().is_none());
    assert_eq!(
        reads.admission.available_permits(),
        super::MAX_SESSION_READ_JOBS
    );
}
