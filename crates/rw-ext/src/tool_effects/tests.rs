#![allow(clippy::expect_used)]
use super::{ToolEffectsOwner, unsettled};
use async_trait::async_trait;
use rw_tools::{CapabilityManifest, ToolEffectGrant, ToolEffectHost, ToolError, ToolResult};
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

struct Host {
    started: Notify,
    release: Notify,
    finished: AtomicBool,
    panic: bool,
}
impl Host {
    fn new(panic: bool) -> Self {
        Self {
            started: Notify::new(),
            release: Notify::new(),
            finished: AtomicBool::new(false),
            panic,
        }
    }
}
#[async_trait]
impl ToolEffectHost for Host {
    async fn call(&self, _: &ToolEffectGrant, _: &str, _: Value) -> Result<ToolResult, ToolError> {
        Err(unsettled("fixture does not execute"))
    }
    async fn close_and_settle(&self) -> Result<(), ToolError> {
        self.started.notify_one();
        assert!(!self.panic, "failed proof fixture");
        self.release.notified().await;
        self.finished.store(true, Ordering::Release);
        Ok(())
    }
}
fn grant() -> ToolEffectGrant {
    ToolEffectGrant::new(CapabilityManifest::default(), &[]).expect("empty effect grant")
}

#[tokio::test]
async fn abandoned_invocation_retains_actual_host_until_proof() {
    let owner = Arc::new(ToolEffectsOwner::default());
    let host = Arc::new(Host::new(false));
    drop(
        owner
            .begin(host.clone(), grant())
            .expect("effect admission"),
    );
    let settling = tokio::spawn({
        let owner = owner.clone();
        async move { owner.settle().await }
    });
    host.started.notified().await;
    assert!(!settling.is_finished());
    assert_eq!(owner.entries.lock().expect("effect registry").len(), 1);
    host.release.notify_one();
    settling
        .await
        .expect("settlement task")
        .expect("effect proof");
    assert!(host.finished.load(Ordering::Acquire));
    assert!(owner.entries.lock().expect("effect registry").is_empty());
}

#[tokio::test]
async fn panicking_proof_retains_host_and_closes_admission() {
    let owner = Arc::new(ToolEffectsOwner::default());
    let host = Arc::new(Host::new(true));
    let weak = Arc::downgrade(&host);
    drop(
        owner
            .begin(host.clone(), grant())
            .expect("effect admission"),
    );
    assert!(matches!(
        owner.settle().await,
        Err(ToolError::EffectsUnsettled(_))
    ));
    assert!(owner.begin(host.clone(), grant()).is_err());
    drop(host);
    assert!(weak.upgrade().is_some());
    assert_eq!(owner.entries.lock().expect("effect registry").len(), 1);
    assert!(owner.settle().await.is_err());
}
