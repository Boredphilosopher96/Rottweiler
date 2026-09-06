//! One published executor result owns preparation across caller cancellation.
use rw_tools::{CommandExecutor, ToolError};
use std::{future::Future, sync::Arc};
use tokio::sync::watch;

#[derive(Clone)]
pub(super) enum Failure {
    Rejected(String),
    Unsettled(String),
}
impl Failure {
    pub(super) fn tool_error(self) -> ToolError {
        match self {
            Self::Rejected(message) => ToolError::Command(message),
            Self::Unsettled(message) => ToolError::EffectsUnsettled(message),
        }
    }
}
type Outcome = std::result::Result<Arc<dyn CommandExecutor>, Failure>;
pub(super) struct Preparation(pub(super) watch::Receiver<Option<Outcome>>);
impl Preparation {
    pub(super) fn start(
        work: impl FnOnce() -> Result<Arc<dyn CommandExecutor>, String> + Send + 'static,
    ) -> Self {
        Self::from_future(Self::run(work))
    }
    pub(super) fn from_future(work: impl Future<Output = Outcome> + Send + 'static) -> Self {
        let (sender, receiver) = watch::channel(None);
        // The owner progresses independently of request waiters. Missing
        // publication is unsettled, including runtime loss before first poll.
        tokio::spawn(async move {
            sender.send_replace(Some(work.await));
        });
        Self(receiver)
    }
    pub(super) async fn run(
        work: impl FnOnce() -> Result<Arc<dyn CommandExecutor>, String> + Send + 'static,
    ) -> Outcome {
        match rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, work).await {
            Ok(result) => result.map_err(Failure::Rejected),
            Err(rw_resources::WorkError::Admission(error)) => {
                Err(Failure::Rejected(error.to_string()))
            }
            Err(rw_resources::WorkError::Worker(error)) => Err(Failure::Unsettled(format!(
                "command preparation lost physical proof: {error}"
            ))),
        }
    }
    pub(super) async fn wait(&self) -> Outcome {
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
    pub(super) async fn settle_effects(&self) -> Result<(), ToolError> {
        match self.wait().await {
            Ok(executor) => executor.settle_effects().await,
            Err(Failure::Rejected(_)) => Ok(()),
            Err(failure) => Err(failure.tool_error()),
        }
    }
}
