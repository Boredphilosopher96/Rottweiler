//! A local client owns this session until explicit close or drop requests shutdown.
use super::runtime_options::display_agent_error;
use crate::session_resources::RuntimeSessionResources;
use miette::Result;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// Composed runtime with a command/event handle and an independent cleanup owner.
/// Clients own presentation and must await `close` before reporting completion.
/// Dropping this value requests the same cleanup without cancelling its work.
pub struct LocalSession {
    handle: rw_core::SessionHandle,
    session_id: String,
    storage_root: PathBuf,
    prompt_dump: Option<rw_core::recovery::HistoryRead<rw_types::PromptDump>>,
    children: Option<WakingChildren>,
    lifetime: Arc<RuntimeSessionResources>,
}

/// Direct children whose finished results wake this session's actor.
///
/// A one-shot client keeps the session open while any of them runs or waits for
/// a slot, so their reports reach the model before the session closes.
#[derive(Clone)]
pub struct WakingChildren {
    orchestrator: rw_core::SubagentOrchestrator,
    parent: rw_types::SessionId,
}

impl WakingChildren {
    /// Whether any direct child is running or queued.
    #[must_use]
    pub fn outstanding(&self) -> bool {
        self.orchestrator.has_outstanding_children(&self.parent)
    }

    /// Resolves once no direct child is running or queued. Every result those
    /// children produced is durable in the parent log by then.
    pub async fn settled(&self) {
        self.orchestrator.children_settled(&self.parent).await;
    }
}

impl LocalSession {
    pub(super) fn new(
        handle: rw_core::SessionHandle,
        session_id: String,
        storage_root: PathBuf,
        prompt_dump: Option<rw_core::recovery::HistoryRead<rw_types::PromptDump>>,
        orchestrator: Option<rw_core::SubagentOrchestrator>,
        lifetime: Arc<RuntimeSessionResources>,
    ) -> Self {
        let children = orchestrator
            .filter(|orchestrator| orchestrator.limits().wake_on_completion)
            .map(|orchestrator| WakingChildren {
                orchestrator,
                parent: rw_types::SessionId(session_id.clone()),
            });
        Self {
            handle,
            session_id,
            storage_root,
            prompt_dump,
            children,
            lifetime,
        }
    }

    #[must_use]
    pub fn handle(&self) -> &rw_core::SessionHandle {
        &self.handle
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn storage_root(&self) -> &Path {
        &self.storage_root
    }

    /// Exact validated request shape for a prompt-inspection session.
    #[must_use]
    pub fn prompt_dump(&self) -> Option<&rw_types::PromptDump> {
        self.prompt_dump.as_deref()
    }

    /// Children whose completion wakes this session, when waking is enabled.
    #[must_use]
    pub fn waking_children(&self) -> Option<&WakingChildren> {
        self.children.as_ref()
    }

    /// Waits for actor effects, session finalization, and service shutdown.
    ///
    /// # Errors
    /// Returns an error if cleanup cannot establish settlement.
    pub async fn close(&self) -> Result<()> {
        rw_core::SessionResources::shutdown(self.lifetime.as_ref())
            .await
            .map_err(display_agent_error)
    }
}
