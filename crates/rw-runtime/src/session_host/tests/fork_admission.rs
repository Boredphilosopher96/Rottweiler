use super::*;
use std::future::{Future as _, poll_fn};
use std::task::Poll;

#[tokio::test]
async fn saturated_fork_waits_leave_unrelated_reads_and_physical_settlement_available() {
    let root = tempdir().expect("root");
    let workspace = private_test_directory(&root.path().join("workspace"));
    let factory = factory(root.path(), &workspace).await;
    drop(
        rw_store::session::journal::SegmentedJournal::open(
            &factory.options.storage_root,
            "independent",
        )
        .expect("independent journal"),
    );
    let (notify, ready) = tokio::sync::oneshot::channel();
    let (release, hold) = std::sync::mpsc::channel();
    let owner = factory.clone();
    let running = tokio::spawn(async move {
        owner
            .fork_journal_work(move |_| {
                notify.send(()).expect("physical owner started");
                hold.recv().expect("physical owner release");
                Ok(())
            })
            .await
    });
    ready.await.expect("fork journal held");
    let mut waiting: Vec<_> = (0..crate::journal_service::MAX_PROJECTION_WAITERS)
        .map(|_| Box::pin(fork_read(&factory)))
        .collect();
    poll_fn(|context| {
        for read in &mut waiting {
            assert!(
                read.as_mut().poll(context).is_pending(),
                "bounded async queue"
            );
        }
        Poll::Ready(())
    })
    .await;
    assert!(
        fork_read(&factory).await.is_err(),
        "overflow rejected before worker admission"
    );
    drop(waiting.pop());
    let mut replacement = Box::pin(fork_read(&factory));
    poll_fn(|context| {
        assert!(
            replacement.as_mut().poll(context).is_pending(),
            "cancelled queue credit returned"
        );
        Poll::Ready(())
    })
    .await;
    waiting.push(replacement);
    running.abort();
    assert!(running.await.expect_err("caller cancelled").is_cancelled());
    let unrelated = crate::todo_service::read_todos(
        Arc::clone(&factory.journal_service),
        SessionId("independent".into()),
        |_| Ok(()),
    )
    .await
    .expect("unrelated actual journal worker remains available");
    assert!(matches!(
        unrelated,
        rw_types::todo::TodoReadResult::Ready { .. }
    ));
    poll_fn(|context| {
        for read in &mut waiting {
            assert!(
                read.as_mut().poll(context).is_pending(),
                "cancelled worker retains physical ownership"
            );
        }
        Poll::Ready(())
    })
    .await;
    release.send(()).expect("release physical owner");
    for read in waiting {
        assert!(matches!(
            read.await.expect("fork read after settlement"),
            ForkOperationState::Missing
        ));
    }
}

async fn fork_read(factory: &RuntimeSessionFactory) -> Result<ForkOperationState, HostError> {
    factory
        .load_fork_operation(&ForkOperationKey {
            operation_id: "queued-fork".into(),
            client_id: rw_core::ClientId("queue-client".into()),
            request_id: rw_core::RequestId("queue-request".into()),
            payload_hash: "0".repeat(64),
        })
        .await
}
