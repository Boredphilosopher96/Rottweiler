use super::OrchestratedWorkflowExecutor;
use futures_util::FutureExt;
use rw_tools::{CancellationToken, ToolError, ToolResult};
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
};
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore, oneshot, watch};
type ExecutorOwner = Arc<OnceCell<Arc<OrchestratedWorkflowExecutor>>>;

/// The whole run, its journal and child cleanup outlive a cancelled tool caller.
pub(super) struct WorkflowExecutions {
    slots: Arc<Semaphore>,
    active: watch::Sender<usize>,
    unproven: Arc<Mutex<Vec<UnprovenWorkflow>>>,
}

struct UnprovenWorkflow {
    _permit: OwnedSemaphorePermit,
    _executor: ExecutorOwner,
}

struct CompletedWorkflow {
    result: Result<ToolResult, ToolError>,
    _admission: Option<OwnedSemaphorePermit>,
}

struct CancelCaller(Option<CancellationToken>);
impl Drop for CancelCaller {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.0 {
            cancellation.cancel();
        }
    }
}

impl WorkflowExecutions {
    pub(super) fn new() -> Self {
        Self {
            slots: Arc::new(Semaphore::new(4)),
            active: watch::channel(0).0,
            unproven: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(super) async fn run<F, C>(
        &self,
        cancellation: CancellationToken,
        executor: ExecutorOwner,
        future: impl Future<Output = Result<ToolResult, ToolError>> + Send + 'static,
        cleanup: F,
    ) -> Result<ToolResult, ToolError>
    where
        F: FnOnce() -> C + Send + 'static,
        C: Future<Output = Result<(), ToolError>> + Send + 'static,
    {
        let permit = Arc::clone(&self.slots)
            .try_acquire_owned()
            .map_err(|_| ToolError::Command("workflow capacity exhausted".to_owned()))?;
        self.active.send_modify(|count| *count += 1);
        let active = self.active.clone();
        let unproven = Arc::clone(&self.unproven);
        let mut cancel_caller = CancelCaller(Some(cancellation.clone()));
        let (reply, receive) = oneshot::channel();
        tokio::spawn(async move {
            let result = if let Ok(result) = AssertUnwindSafe(future).catch_unwind().await {
                result
            } else {
                cancellation.cancel();
                Err(ToolError::Command("workflow executor panicked".to_owned()))
            };
            let settled = AssertUnwindSafe(async move { cleanup().await })
                .catch_unwind()
                .await;
            match settled {
                Ok(Ok(())) => {
                    drop(executor);
                    drop(reply.send(CompletedWorkflow {
                        result,
                        _admission: Some(permit),
                    }));
                    active.send_modify(|count| *count -= 1);
                }
                outcome => {
                    let reason = match outcome {
                        Ok(Err(error)) => error.to_string(),
                        _ => "cleanup panicked".to_owned(),
                    };
                    unproven
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(UnprovenWorkflow {
                            _permit: permit,
                            _executor: executor,
                        });
                    active.send_modify(|_| {});
                    drop(reply.send(CompletedWorkflow {
                        result: Err(ToolError::EffectsUnsettled(format!(
                            "workflow effects remain unproven: {reason}"
                        ))),
                        _admission: None,
                    }));
                }
            }
        });
        let completed = receive.await.map_err(|_| {
            ToolError::EffectsUnsettled("workflow completion owner stopped".to_owned())
        })?;
        cancel_caller.0 = None;
        completed.result
    }

    pub(super) async fn settle(&self) -> Result<(), ToolError> {
        let mut active = self.active.subscribe();
        loop {
            let count = *active.borrow_and_update();
            if !self
                .unproven
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
            {
                return Err(ToolError::EffectsUnsettled(
                    "workflow effects remain unproven".to_owned(),
                ));
            }
            if count == 0 {
                return Ok(());
            }
            if active.changed().await.is_err() {
                return Err(ToolError::EffectsUnsettled(
                    "workflow completion owner stopped".to_owned(),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests;
