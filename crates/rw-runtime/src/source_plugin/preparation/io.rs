//! Finite source IO shares the preparation owner and its retirement fence.
use super::{CancelOnDrop, Operation, Ownership, PrivateMcpScratch, SourcePreparations};
use miette::{Result, miette};
use std::{panic::AssertUnwindSafe, sync::Arc, sync::atomic::Ordering};

impl SourcePreparations {
    pub(in crate::source_plugin) async fn execute_io<T: Send + 'static>(
        self: &Arc<Self>,
        scratch: Arc<PrivateMcpScratch>,
        stage: &'static str,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let mut ownership = {
            let mut jobs = self
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.closed.load(Ordering::Acquire) {
                return Err(miette!("TypeScript preparation generation is closed"));
            }
            let admission = self
                .budget
                .admission
                .clone()
                .try_acquire_owned()
                .map_err(|_| miette!("TypeScript preparation admission is exhausted"))?;
            let operation = Arc::new(Operation::new());
            jobs.push(operation.clone());
            Ownership::new(self.clone(), operation, admission, scratch)
        };
        let operation = ownership.operation.clone();
        let mut guard = CancelOnDrop(Some(operation.clone()));
        let execution = self.budget.execution.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let cancellation = &operation.cancellation;
            let result = tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(miette!("TypeScript preparation was cancelled")),
                () = tokio::time::sleep(crate::source_plugin::HOST_DEADLINE) => Err(miette!("TypeScript source IO admission deadline expired")),
                permit = execution.acquire_owned() => permit.map_err(|_| miette!("TypeScript preparation admission closed")),
            };
            ownership.execution = match result {
                Ok(permit) => Some(permit),
                Err(error) => {
                    let _ = send.send(ownership.complete(Err(error)));
                    return;
                }
            };
            let credit = match rw_resources::acquire(
                rw_resources::ResourceClass::Blocking,
                cancellation.cancelled(),
            )
            .await
            {
                Ok(credit) => credit,
                Err(error) => {
                    let _ = send.send(ownership.complete(Err(miette!(error.to_string()))));
                    return;
                }
            };
            // Completion is published by the physical worker, not its join
            // waiter. Scratch and both admission credits survive caller drop.
            let _ = tokio::task::spawn_blocking(move || {
                let _credit = credit;
                let started = std::time::Instant::now();
                let result = if operation.cancellation.is_cancelled() {
                    Err(miette!("TypeScript preparation was cancelled"))
                } else {
                    std::panic::catch_unwind(AssertUnwindSafe(work))
                        .unwrap_or_else(|_| Err(miette!("TypeScript source IO worker panicked")))
                };
                tracing::debug!(target: "rw_performance", stage,
                    elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
                    succeeded = result.is_ok(), "plugin preparation stage finished");
                let _ = send.send(ownership.complete(result));
            })
            .await;
        });
        let completed = receive
            .await
            .map_err(|_| miette!("source IO ownership was interrupted"))?;
        guard.0 = None;
        completed.result
    }
}
