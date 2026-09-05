use super::*;
use tokio::sync::Notify;

struct GatedObserver {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    reject: bool,
}
#[async_trait]
impl SubagentObserver for GatedObserver {
    async fn spawned(
        &self,
        _handle: &SubagentHandle,
        _task: &str,
    ) -> Result<(), OrchestrationError> {
        self.entered.notify_one();
        self.release.notified().await;
        if self.reject {
            Err(OrchestrationError::Observer("injected failure".to_owned()))
        } else {
            Ok(())
        }
    }
    async fn finished(&self, _result: &SubagentResult) -> Result<(), OrchestrationError> {
        Ok(())
    }
    async fn progress(
        &self,
        _handle: &SubagentHandle,
        _sequence: Option<u64>,
        _event: Value,
    ) -> Result<(), OrchestrationError> {
        Ok(())
    }
}

#[tokio::test]
async fn aborted_startup_retains_child_until_close_and_keeps_ambiguous_receipt() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
    let metadata = Arc::new(RecordingMetadataStore::default());
    orchestrator.bind_metadata_store(metadata.clone());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let observer = Arc::new(GatedObserver {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
        reject: true,
    });
    let owner = orchestrator.clone();
    let caller = tokio::spawn(async move {
        owner
            .start(
                SessionId("parent".to_owned()),
                request("must-not-run"),
                observer,
                CancellationToken::default(),
            )
            .await
    });
    entered.notified().await;
    caller.abort();
    assert!(caller.await.expect_err("aborted").is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), orchestrator.settle_startups())
            .await
            .is_err()
    );
    assert!(factory.closed_artifacts.lock().expect("closed").is_empty());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), orchestrator.settle_startups())
        .await
        .expect("owner completes")
        .expect("cleanup proof");
    assert_eq!(factory.closed_artifacts.lock().expect("closed").len(), 1);
    assert_eq!(
        metadata
            .record
            .lock()
            .expect("receipt")
            .as_ref()
            .expect("retained")
            .phase,
        SubagentRecoveryPhase::Closed
    );
    assert_eq!(factory.active.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn unconsumed_start_reply_cancels_and_closes_the_launched_child() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
    let metadata = Arc::new(RecordingMetadataStore::default());
    orchestrator.bind_metadata_store(metadata.clone());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let observer = Arc::new(GatedObserver {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
        reject: false,
    });
    let mut caller = Box::pin(orchestrator.start(
        SessionId("parent".to_owned()),
        request("short"),
        observer,
        CancellationToken::default(),
    ));
    assert!(futures_util::poll!(&mut caller).is_pending());
    entered.notified().await;
    release.notify_one();
    // Never poll the reply: its acknowledgement must remain with the owner.
    drop(caller);
    tokio::time::timeout(Duration::from_secs(1), orchestrator.settle_startups())
        .await
        .expect("owner completes")
        .expect("cleanup proof");
    assert_eq!(factory.closed_artifacts.lock().expect("closed").len(), 1);
    assert!(
        orchestrator
            .list_for_parent(&SessionId("parent".to_owned()))
            .is_empty()
    );
    assert!(metadata.record.lock().expect("metadata").is_none());
}

#[tokio::test]
async fn failed_startup_cleanup_reports_error_without_releasing_its_capacity() {
    let factory = Arc::new(FakeFactory {
        fail_close: true,
        ..FakeFactory::default()
    });
    let orchestrator = orchestrator(
        SubagentLimits {
            max_concurrency: 1,
            ..SubagentLimits::default()
        },
        factory,
    );
    let observer = Arc::new(RecordingObserver {
        fail_spawned: true,
        ..RecordingObserver::default()
    });
    let error = orchestrator
        .start(
            SessionId("parent".to_owned()),
            request("must-not-run"),
            observer,
            CancellationToken::default(),
        )
        .await
        .expect_err("failed cleanup");
    assert!(error.to_string().contains("unproven"));
    assert!(
        orchestrator
            .settle_startups()
            .await
            .expect_err("unsettled")
            .to_string()
            .contains("unproven")
    );
    let next = orchestrator
        .start(
            SessionId("parent".to_owned()),
            request("no capacity"),
            Arc::new(RecordingObserver::default()),
            CancellationToken::default(),
        )
        .await;
    assert!(matches!(
        next,
        Err(OrchestrationError::ConcurrencyExceeded { .. })
    ));
}

#[tokio::test]
async fn rejected_startup_does_not_cancel_its_parent_turn() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), factory);
    let cancellation = CancellationToken::default();
    let result = orchestrator
        .start(
            SessionId("parent".to_owned()),
            request(""),
            Arc::new(RecordingObserver::default()),
            cancellation.clone(),
        )
        .await;
    assert!(matches!(result, Err(OrchestrationError::InvalidRequest(_))));
    orchestrator
        .settle_startups()
        .await
        .expect("no unsettled child");
    assert!(!cancellation.is_cancelled());
}
