#![allow(clippy::expect_used)]
use super::{DelegatedEffect, DelegatedTools, ToolEffectGrant, ToolEffectHost};
use crate::{
    CapabilityManifest, MutationScope, Tool, ToolContext, ToolDescriptor, ToolError, ToolRegistry,
    ToolResult,
};
use async_trait::async_trait;
use rw_types::ToolCapability::ReadFilesystem;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

struct BlockingAuthorization {
    entered: tokio::sync::Notify,
    release: (Mutex<bool>, Condvar),
}
#[async_trait]
impl Tool for BlockingAuthorization {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "blocked_read".into(),
            description: "owned path authorization".into(),
            input_schema: json!({"type":"object"}),
            capabilities: CapabilityManifest::new([ReadFilesystem]),
        }
    }
    fn delegated_effect(&self, _: &Value) -> Result<DelegatedEffect, ToolError> {
        self.entered.notify_one();
        let (lock, ready) = &self.release;
        let mut released = lock.lock().expect("authorization flag");
        while !*released {
            released = ready.wait(released).expect("authorization release");
        }
        Ok(DelegatedEffect::Filesystem)
    }
    fn workspace_paths(&self, _: &Value) -> Result<Vec<PathBuf>, ToolError> {
        Ok(vec!["file".into()])
    }
    async fn execute(&self, _: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        panic!("cancelled authorization cannot invoke the tool");
    }
    async fn settle_effects(&self) -> Result<(), ToolError> {
        Ok(())
    }
}

#[tokio::test]
async fn dropped_callback_retains_blocking_authorization_until_settlement() {
    let root = tempfile::tempdir().expect("workspace");
    std::fs::write(root.path().join("file"), "bytes").expect("input file");
    let context = ToolContext::new(root.path()).expect("context");
    let tool = Arc::new(BlockingAuthorization {
        entered: tokio::sync::Notify::new(),
        release: (Mutex::new(false), Condvar::new()),
    });
    let mut tools = ToolRegistry::default();
    tools.register(tool.clone()).expect("registered tool");
    let host = Arc::new(DelegatedTools::new(
        context,
        Arc::new(tools),
        CapabilityManifest::new([ReadFilesystem]),
        MutationScope::None,
    ));
    let grant = ToolEffectGrant::new(CapabilityManifest::new([ReadFilesystem]), &[])
        .expect("read authority");
    let call = tokio::spawn({
        let host = host.clone();
        let grant = grant.clone();
        async move { host.call(&grant, "blocked_read", json!({})).await }
    });
    tool.entered.notified().await;
    assert!(
        matches!(
            host.call(&grant, "blocked_read", json!({})).await,
            Err(ToolError::DelegationDenied(_))
        ),
        "no second queued effect"
    );
    call.abort();
    assert!(call.await.expect_err("caller dropped").is_cancelled());
    let settling = tokio::spawn({
        let host = host.clone();
        async move { host.close_and_settle().await }
    });
    tokio::task::yield_now().await;
    assert!(
        !settling.is_finished(),
        "dropped callback is not completed authorization"
    );
    let (lock, ready) = &tool.release;
    *lock.lock().expect("release lock") = true;
    ready.notify_one();
    settling
        .await
        .expect("proof task")
        .expect("actual authorization settled");
    assert!(
        matches!(
            host.call(&grant, "blocked_read", json!({})).await,
            Err(ToolError::DelegationDenied(_))
        ),
        "retired scope cannot admit another effect"
    );
}
