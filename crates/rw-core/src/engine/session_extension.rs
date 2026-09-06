//! Session-owned activation boundary for temporary plugin generations.

use std::{fmt, path::Path, sync::Arc};

use async_trait::async_trait;
use rw_ext::{CommandRegistry, HookDispatcher};
use rw_tools::ToolRegistry;

use super::{
    AgentLoopError,
    commands::{SessionCommandContext, SessionCommandOutput},
};

/// One immutable tool/hook/command boundary activated between turns.
#[derive(Clone)]
pub struct SessionExtensionSnapshot {
    pub publication: super::RuntimePublication,
    pub model: Arc<dyn super::ModelDriver>,
    pub model_alias: String,
    pub ui: Arc<dyn crate::ui::UiRegistry>,
    pub revision: u64,
    pub workspace_roots: Arc<[std::path::PathBuf]>,
    pub tools: Arc<ToolRegistry>,
    pub hooks: Arc<HookDispatcher>,
    pub commands: Arc<CommandRegistry<SessionCommandContext, SessionCommandOutput>>,
}

impl fmt::Debug for SessionExtensionSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionExtensionSnapshot")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

/// Session-owned boundary for temporary source-plugin activation and reloading.
#[async_trait]
pub trait SessionExtensionController: Send + Sync {
    async fn attach(
        &self,
        source: &Path,
        current: SessionExtensionSnapshot,
    ) -> Result<SessionExtensionSnapshot, AgentLoopError>;

    async fn detach(
        &self,
        current: SessionExtensionSnapshot,
    ) -> Result<SessionExtensionSnapshot, AgentLoopError>;

    async fn shutdown(&self) -> Result<(), AgentLoopError>;
}

#[derive(Debug, Default)]
pub struct NoopSessionExtensionController;

#[async_trait]
impl SessionExtensionController for NoopSessionExtensionController {
    async fn attach(
        &self,
        _source: &Path,
        _current: SessionExtensionSnapshot,
    ) -> Result<SessionExtensionSnapshot, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "live plugin development is unavailable for this session host".to_owned(),
        ))
    }

    async fn detach(
        &self,
        _current: SessionExtensionSnapshot,
    ) -> Result<SessionExtensionSnapshot, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "no development plugin is attached".to_owned(),
        ))
    }

    async fn shutdown(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }
}
