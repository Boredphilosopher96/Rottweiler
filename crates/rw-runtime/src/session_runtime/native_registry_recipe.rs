//! First-party registries are rebuilt against the same native generation.
use super::{
    native_model_generations::NativeModelGenerations,
    workspace_roots::RuntimeWorkspaceRootController,
};
use crate::extension_runtime::{McpSessionRuntime, RuntimeSessionExtensionController};
use rw_core::{AgentLoopError, ApplyWorktreeDiffTool, SpawnAgentTool, SubagentOrchestrator};
use rw_ext::{ExtensionCatalog, compose_agent_registry};
use rw_tools::ToolRegistry;
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock, Weak},
};

/// Standalone runtimes have no session-owned native extension generation.
/// A session binding is installed exactly once after cyclic registries exist.
pub(crate) enum RootNativeBinding {
    Standalone,
    Session(OnceLock<Weak<RuntimeSessionExtensionController>>),
}
impl RootNativeBinding {
    pub(crate) fn session() -> Self {
        Self::Session(OnceLock::new())
    }
    pub(crate) fn bind(
        &self,
        controller: &Arc<RuntimeSessionExtensionController>,
    ) -> Result<(), AgentLoopError> {
        let Self::Session(binding) = self else {
            return Err(error(
                "standalone root composition cannot bind native plugins",
            ));
        };
        binding
            .set(Arc::downgrade(controller))
            .map_err(|_| error("native root composition is already bound"))
    }
}

pub(crate) struct NativeRegistryRecipe {
    pub(crate) roots: Weak<RuntimeWorkspaceRootController>,
    pub(crate) orchestrator: SubagentOrchestrator,
    pub(crate) models: Arc<NativeModelGenerations>,
    pub(crate) mcp: Option<Arc<McpSessionRuntime>>,
    pub(crate) storage_root: PathBuf,
}
impl NativeRegistryRecipe {
    pub(crate) fn root_owner(&self) -> Result<Arc<RuntimeWorkspaceRootController>, AgentLoopError> {
        self.roots.upgrade().ok_or(AgentLoopError::Closed)
    }
    pub(crate) fn add_tools(
        &self,
        tools: &mut ToolRegistry,
        catalog: &Arc<ExtensionCatalog>,
    ) -> Result<(), AgentLoopError> {
        if let Some(mcp) = &self.mcp {
            rw_core::register_mcp_tools(tools, mcp.manager.clone(), mcp.spool.clone())
                .map_err(error)?;
        }
        let mut agents = compose_agent_registry(catalog).map_err(error)?;
        let mut names = tools
            .descriptors()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        names.extend(["spawn_agent".to_owned(), "apply_worktree_diff".to_owned()]);
        if catalog.workflows().len() > 0 {
            names.push("workflow".into());
        }
        agents.resolve_tool_names(names).map_err(error)?;
        let agents = Arc::new(agents);
        tools
            .register(Arc::new(SpawnAgentTool::new(
                self.orchestrator.clone(),
                agents.clone(),
                self.models.source(),
            )))
            .map_err(error)?;
        tools
            .register(Arc::new(ApplyWorktreeDiffTool::new(
                self.orchestrator.diff_artifact_authority(),
            )))
            .map_err(error)?;
        if catalog.workflows().len() > 0 {
            tools
                .register(Arc::new(crate::workflow_runtime::WorkflowTool::new(
                    self.orchestrator.clone(),
                    agents,
                    catalog.clone(),
                    self.storage_root.clone(),
                )))
                .map_err(error)?;
        }
        Ok(())
    }
}
fn error(error: impl ToString) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(error.to_string())
}
