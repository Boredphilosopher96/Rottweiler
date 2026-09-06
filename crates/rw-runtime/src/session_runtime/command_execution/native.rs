//! Native command preparation is single-owner work, started by actual execution.
use super::preparation::{Failure, Preparation};
use async_trait::async_trait;
use rw_tools::{
    CancellationToken, CommandExecutor, CommandOutcome as ToolCommandOutcome, CommandRequest,
    CommandSafetyClassifier, ExecutionLease, SandboxPolicy, SandboxSupport, TokioCommandExecutor,
    ToolError, ToolOutputSink, probe_policy_egress,
};
use std::sync::Arc;
use std::sync::OnceLock;

#[derive(Clone)]
pub(super) struct NativeRecipe {
    pub policy: Arc<SandboxPolicy>,
    pub scratch: Arc<rw_tools::CommandScratch>,
    pub execution_lease: Arc<ExecutionLease>,
    pub safety: Arc<CommandSafetyClassifier>,
    pub policy_egress: bool,
    pub upstream: Option<rw_tools::UpstreamProxy>,
}
impl NativeRecipe {
    fn prepare(self) -> std::result::Result<Arc<dyn CommandExecutor>, String> {
        let helper = crate::plugin_process::helper_executable()
            .map_err(|error| format!("command sandbox helper could not resolve: {error}"))?;
        let policy_egress =
            self.policy_egress && probe_policy_egress().support == SandboxSupport::Enforced;
        Ok(Arc::new(
            TokioCommandExecutor::with_execution_lease(self.execution_lease)
                .sandboxed(self.policy, helper, self.scratch)
                .with_command_safety(self.safety)
                .with_policy_egress(policy_egress)
                .with_upstream_proxy(self.upstream),
        ))
    }
}
pub(super) struct NativeCommandExecutor {
    recipe: NativeRecipe,
    initialization: OnceLock<Preparation>,
}
impl NativeCommandExecutor {
    pub(super) fn new(recipe: NativeRecipe) -> Self {
        Self {
            recipe,
            initialization: OnceLock::new(),
        }
    }
    async fn inner(&self) -> std::result::Result<Arc<dyn CommandExecutor>, ToolError> {
        let preparation = self.initialization.get_or_init(|| {
            let recipe = self.recipe.clone();
            Preparation::start(move || recipe.prepare())
        });
        preparation.wait().await.map_err(Failure::tool_error)
    }
}
#[async_trait]
impl CommandExecutor for NativeCommandExecutor {
    async fn settle_effects(&self) -> std::result::Result<(), ToolError> {
        let Some(preparation) = self.initialization.get() else {
            return Ok(());
        };
        preparation.settle_effects().await
    }
    fn supports_background(&self) -> bool {
        true
    }
    async fn run(
        &self,
        request: CommandRequest,
        cancellation: CancellationToken,
        output: Arc<dyn ToolOutputSink>,
    ) -> std::result::Result<ToolCommandOutcome, ToolError> {
        self.inner().await?.run(request, cancellation, output).await
    }
}

#[cfg(test)]
mod tests;
