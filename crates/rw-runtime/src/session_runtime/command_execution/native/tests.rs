#![allow(clippy::expect_used)]
use super::*;
use rw_tools::NetworkPolicy;
use std::sync::atomic::{AtomicUsize, Ordering};

struct FixtureExecutor;
#[async_trait]
impl CommandExecutor for FixtureExecutor {
    async fn settle_effects(&self) -> std::result::Result<(), ToolError> {
        Ok(())
    }
    async fn run(
        &self,
        _: CommandRequest,
        _: CancellationToken,
        _: Arc<dyn ToolOutputSink>,
    ) -> std::result::Result<ToolCommandOutcome, ToolError> {
        Err(ToolError::Command(
            "fixture execution is not requested".into(),
        ))
    }
}

#[tokio::test]
async fn caller_loss_keeps_preparation_owned_until_its_last_filesystem_effect() {
    let root = Arc::new(tempfile::tempdir().expect("scratch"));
    let scratch = root.path().to_path_buf();
    let weak = Arc::downgrade(&root);
    let executions = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&executions);
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let preparation = Arc::new(Preparation::start(move || {
        count.fetch_add(1, Ordering::SeqCst);
        let _ = entered.send(());
        released.recv().expect("release physical preparation");
        std::fs::write(root.path().join("last-write"), b"prepared").expect("last physical effect");
        drop(root);
        Ok(Arc::new(FixtureExecutor))
    }));
    let waiter = {
        let preparation = Arc::clone(&preparation);
        tokio::spawn(async move { preparation.wait().await })
    };
    started.await.expect("worker entered");
    waiter.abort();
    assert!(waiter.await.is_err());
    assert!(weak.upgrade().is_some());
    assert!(scratch.is_dir());
    let settlement = {
        let preparation = Arc::clone(&preparation);
        tokio::spawn(async move { preparation.wait().await })
    };
    tokio::task::yield_now().await;
    assert!(
        !settlement.is_finished(),
        "caller loss cannot publish a false completion"
    );
    release.send(()).expect("finish owned work");
    let executor = settlement
        .await
        .expect("settlement task")
        .unwrap_or_else(|_| panic!("physical preparation must finish"));
    let shared = preparation
        .wait()
        .await
        .unwrap_or_else(|_| panic!("retained prepared result"));
    assert!(Arc::ptr_eq(&executor, &shared));
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert!(weak.upgrade().is_none());
    assert!(!scratch.exists());
}

#[tokio::test]
async fn missing_publication_and_panicked_workers_cannot_prove_settlement() {
    let (sender, receiver) = watch::channel(None);
    drop(sender);
    assert!(matches!(
        Preparation(receiver).wait().await,
        Err(Failure::Unsettled(_))
    ));
    let panicked = Preparation::start(|| panic!("physical verifier panic"));
    assert!(matches!(panicked.wait().await, Err(Failure::Unsettled(_))));
}

#[tokio::test]
async fn unused_native_executor_settlement_does_not_capture_a_helper() {
    let root = tempfile::tempdir().expect("workspace");
    let executor = NativeCommandExecutor::new(NativeRecipe {
        policy: Arc::new(SandboxPolicy::new([root.path()], NetworkPolicy::Deny).expect("policy")),
        execution_lease: Arc::new(
            ExecutionLease::acquire(root.path().join("execution.lock")).expect("execution lease"),
        ),
        safety: Arc::new(CommandSafetyClassifier::default()),
        policy_egress: false,
        upstream: None,
    });
    executor.settle_effects().await.expect("no effects");
    assert!(executor.initialization.get().is_none());
}
