//! Slow source reads must not occupy actor dispatch or lose settlement ownership.
use super::fixtures::{history, models::ScriptedModel, support::config};
use crate::engine::{AgentLoopError, SessionActor, SessionHandle, builtin_hook_dispatcher};
use crate::recovery::{SessionHistory, SessionHistoryView};
use async_trait::async_trait;
use rw_tools::ToolRegistry;
use rw_types::config::PermissionDecision;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Notify, Semaphore};
use tokio::time::{Duration, timeout};

struct GatedHistory {
    inner: Arc<dyn SessionHistory>,
    armed: AtomicBool,
    entered: Notify,
    release: Semaphore,
}
#[async_trait]
impl SessionHistory for GatedHistory {
    async fn capture_history(&self) -> Result<Arc<dyn SessionHistoryView>, AgentLoopError> {
        if self.armed.load(Ordering::Acquire) {
            self.entered.notify_one();
            self.release.acquire().await.expect("gate").forget();
        }
        self.inner.capture_history().await
    }
}
async fn fixture() -> (tempfile::TempDir, SessionHandle, Arc<GatedHistory>) {
    let root = tempfile::tempdir().expect("workspace");
    let config = config(
        root.path(),
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    let mut config = history::bind(config).await.expect("source");
    let gate = Arc::new(GatedHistory {
        inner: config.history,
        armed: AtomicBool::new(false),
        entered: Notify::new(),
        release: Semaphore::new(0),
    });
    config.history = gate.clone();
    let handle = SessionActor::spawn(config).expect("actor");
    handle.snapshot().await.expect("startup settled");
    gate.armed.store(true, Ordering::Release);
    (root, handle, gate)
}

#[tokio::test]
async fn abandoned_context_read_remains_owned_while_dispatch_and_shutdown_respond() {
    let (_root, handle, gate) = fixture().await;
    let reader = handle.clone();
    let read = tokio::spawn(async move { reader.context_snapshot().await });
    timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .expect("read entered source");
    timeout(Duration::from_millis(100), handle.snapshot())
        .await
        .expect("dispatch responsive")
        .expect("snapshot");
    read.abort();
    let _ = read.await;
    let denied = timeout(Duration::from_millis(100), handle.context_snapshot())
        .await
        .expect("bounded read admission");
    assert!(denied.is_err());
    let closer = handle.clone();
    let mut close = tokio::spawn(async move { closer.close().await });
    assert!(
        timeout(Duration::from_millis(20), &mut close)
            .await
            .is_err(),
        "shutdown must retain the blocked read"
    );
    gate.armed.store(false, Ordering::Release);
    gate.release.add_permits(1);
    timeout(Duration::from_secs(2), close)
        .await
        .expect("settled shutdown")
        .expect("task")
        .expect("closed");
}

#[tokio::test]
async fn context_read_rejects_publication_after_mode_changes() {
    let (_root, handle, gate) = fixture().await;
    let reader = handle.clone();
    let read = tokio::spawn(async move { reader.context_snapshot().await });
    timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .expect("read entered source");
    timeout(Duration::from_secs(2), handle.send_message("/mode plan"))
        .await
        .expect("mode dispatch responsive")
        .expect("mode changed");
    gate.armed.store(false, Ordering::Release);
    gate.release.add_permits(1);
    let result = timeout(Duration::from_secs(2), read)
        .await
        .expect("read completed")
        .expect("task");
    assert!(
        result.is_err(),
        "a result prepared under another mode cannot publish"
    );
    handle.close().await.expect("close");
}
