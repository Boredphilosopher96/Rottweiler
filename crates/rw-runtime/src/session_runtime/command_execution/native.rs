//! Native command preparation is single-owner work, started by actual execution.
use async_trait::async_trait;
use rw_tools::{
    CancellationToken, CommandExecutor, CommandOutcome as ToolCommandOutcome, CommandRequest,
    CommandSafetyClassifier, ExecutionLease, SandboxPolicy, SandboxSupport, TokioCommandExecutor,
    ToolError, ToolOutputSink, probe_policy_egress,
};
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::watch;

#[derive(Clone)]
pub(super) struct NativeRecipe {
    pub policy: Arc<SandboxPolicy>,
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
                .sandboxed(self.policy, helper)
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
        match preparation.wait().await {
            Ok(executor) => executor.settle_effects().await,
            Err(Failure::Rejected(_)) => Ok(()),
            Err(failure) => Err(failure.tool_error()),
        }
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
#[derive(Clone)]
enum Failure {
    Rejected(String),
    Unsettled(String),
}
impl Failure {
    fn tool_error(self) -> ToolError {
        match self {
            Self::Rejected(message) => ToolError::Command(message),
            Self::Unsettled(message) => ToolError::EffectsUnsettled(message),
        }
    }
}
type Outcome = std::result::Result<Arc<dyn CommandExecutor>, Failure>;
struct Preparation(watch::Receiver<Option<Outcome>>);
impl Preparation {
    fn start(
        work: impl FnOnce() -> std::result::Result<Arc<dyn CommandExecutor>, String> + Send + 'static,
    ) -> Self {
        let (sender, receiver) = watch::channel(None);
        // This task owns initialization independently of every request waiter.
        // The physical worker keeps its admission until verification/copy ends;
        // settlement waits for publication even when the initiating caller left.
        tokio::spawn(async move {
            let outcome =
                match rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, work).await
                {
                    Ok(result) => result.map_err(Failure::Rejected),
                    Err(rw_resources::WorkError::Admission(error)) => {
                        Err(Failure::Rejected(error.to_string()))
                    }
                    Err(rw_resources::WorkError::Worker(error)) => Err(Failure::Unsettled(
                        format!("command preparation lost physical proof: {error}"),
                    )),
                };
            sender.send_replace(Some(outcome));
        });
        Self(receiver)
    }
    async fn wait(&self) -> std::result::Result<Arc<dyn CommandExecutor>, Failure> {
        let mut receiver = self.0.clone();
        loop {
            if let Some(outcome) = receiver.borrow_and_update().clone() {
                return outcome;
            }
            receiver.changed().await.map_err(|_| {
                Failure::Unsettled("command preparation stopped without publication proof".into())
            })?;
        }
    }
}

#[cfg(test)]
mod tests;
