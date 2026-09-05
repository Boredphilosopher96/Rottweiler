//! A child holds its captured provider generation until its actor drops it.
use super::{NativeModelGenerations, busy};
use async_trait::async_trait;
use rw_core::{AgentLoopError, ModelDriver, SessionResources};
use rw_providers::FixtureRedactor;
use std::sync::{Arc, Weak};

pub(in crate::session_runtime) struct ChildNativeModel {
    pub provider: Arc<dyn ModelDriver>,
    pub redactor: FixtureRedactor,
    pub resources: Arc<dyn SessionResources>,
}
struct ChildLease(Arc<NativeModelGenerations>);
impl Drop for ChildLease {
    fn drop(&mut self) {
        self.0.lock().children -= 1;
    }
}
#[async_trait]
impl SessionResources for ChildLease {
    fn bind_session(&self, _binding: rw_core::PluginSessionBinding) -> Result<(), AgentLoopError> {
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), AgentLoopError> {
        // Actor shutdown settles its model before releasing resources. The
        // admission remains retained until every actor configuration is dropped.
        Ok(())
    }
}
impl NativeModelGenerations {
    pub(in crate::session_runtime) fn capture_child(
        owner: &Weak<Self>,
        workspace: &std::path::Path,
        alias: &str,
    ) -> Result<ChildNativeModel, AgentLoopError> {
        let owner = owner.upgrade().ok_or(AgentLoopError::Closed)?;
        let mut state = owner.lock();
        if state.replacing {
            return Err(busy());
        }
        state.children = state.children.checked_add(1).ok_or_else(busy)?;
        let compose = Arc::clone(&state.current.children);
        let redactor = state.current.redactor.clone();
        drop(state);
        let resources = Arc::new(ChildLease(owner));
        let provider = compose(workspace, alias);
        Ok(ChildNativeModel {
            provider,
            redactor,
            resources,
        })
    }
}
