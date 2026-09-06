#![allow(clippy::expect_used)]
use super::*;
use crate::ToolOutputChunk;
use std::time::Duration;
use tokio::sync::Notify;

struct HeldOutput {
    entered: Notify,
    release: Notify,
}
#[async_trait]
impl ToolOutputSink for HeldOutput {
    async fn emit(&self, _: ToolOutputChunk) -> Result<(), ToolError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

#[tokio::test]
async fn actual_command_retains_scratch_after_caller_and_executor_drop() {
    let workspace = tempfile::tempdir().expect("workspace");
    let scratch = CommandScratch::create("native-lifetime").expect("scratch");
    let path = scratch.path().to_path_buf();
    let retained = Arc::downgrade(&scratch);
    let policy = Arc::new(
        SandboxPolicy::new(
            [workspace.path(), scratch.path()],
            SandboxNetworkPolicy::Deny,
        )
        .expect("policy"),
    );
    let executor = Arc::new(TokioCommandExecutor::default().sandboxed(
        policy,
        crate::test_support::sandbox_helper(),
        scratch,
    ));
    let cleanup = Arc::clone(&executor.native_cleanup);
    let output = Arc::new(HeldOutput {
        entered: Notify::new(),
        release: Notify::new(),
    });
    let worker = Arc::clone(&executor);
    let sink = Arc::clone(&output);
    let cwd = workspace.path().to_path_buf();
    let caller = tokio::spawn(async move {
        worker
            .run(
                CommandRequest {
                    command: "printf held-output; /bin/sleep 60".into(),
                    cwd,
                    env: std::collections::BTreeMap::new(),
                    network_domains: Vec::new(),
                    // This exercises actual process/output ownership independent of
                    // platform sandbox enforcement, which has separate acceptance.
                    sandbox: BashSandboxMode::Unsandboxed,
                },
                CancellationToken::default(),
                sink,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), output.entered.notified())
        .await
        .expect("actual output worker entered sink");
    caller.abort();
    let _ = caller.await;
    drop(executor);
    assert!(
        retained.upgrade().is_some(),
        "physical output still owns scratch"
    );
    assert!(path.is_dir());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), cleanup.settle())
            .await
            .is_err()
    );
    output.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), cleanup.settle())
        .await
        .expect("settlement deadline")
        .expect("physical settlement");
    assert!(retained.upgrade().is_none());
    assert!(!path.exists());
}
