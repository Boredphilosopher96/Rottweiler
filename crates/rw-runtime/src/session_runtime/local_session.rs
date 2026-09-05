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
    prompt_dump: Option<rw_types::PromptDump>,
    lifetime: Arc<RuntimeSessionResources>,
}

impl LocalSession {
    pub(super) fn new(
        handle: rw_core::SessionHandle,
        session_id: String,
        storage_root: PathBuf,
        prompt_dump: Option<rw_types::PromptDump>,
        lifetime: Arc<RuntimeSessionResources>,
    ) -> Self {
        Self {
            handle,
            session_id,
            storage_root,
            prompt_dump,
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
        self.prompt_dump.as_ref()
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
