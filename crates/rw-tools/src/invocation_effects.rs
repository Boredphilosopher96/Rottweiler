//! Retain each invoked backend until its local effects are proven settled.
use crate::{CancellationToken, ToolError};
use async_trait::async_trait;

#[async_trait]
pub(crate) trait InvocationEffect: Send + Sync {
    async fn settle_effects(&self) -> Result<(), ToolError>;
}
use futures_util::FutureExt as _;
use std::panic::AssertUnwindSafe;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_INVOCATION_OPERATIONS: usize = 16;
const SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct InvocationEffects {
    jobs: Mutex<BTreeMap<u64, Arc<Job>>>,
    next: AtomicU64,
    credits: Arc<Semaphore>,
}
impl Default for InvocationEffects {
    fn default() -> Self {
        Self {
            jobs: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(0),
            credits: Arc::new(Semaphore::new(MAX_INVOCATION_OPERATIONS)),
        }
    }
}
struct Job {
    backend: Arc<dyn InvocationEffect>,
    cancellation: CancellationToken,
    abandoned: AtomicBool,
    failed: AtomicBool,
    _credit: OwnedSemaphorePermit,
}
pub(crate) struct InvocationOperation {
    owner: Arc<InvocationEffects>,
    id: u64,
    job: Arc<Job>,
    finished: bool,
}
impl Drop for InvocationOperation {
    fn drop(&mut self) {
        if !self.finished {
            self.job.cancellation.cancel();
            self.job.abandoned.store(true, Ordering::Release);
        }
    }
}
impl InvocationEffects {
    pub(crate) fn begin(
        self: &Arc<Self>,
        backend: Arc<dyn InvocationEffect>,
        cancellation: CancellationToken,
    ) -> Result<InvocationOperation, ToolError> {
        let credit = Arc::clone(&self.credits).try_acquire_owned().map_err(|_| {
            ToolError::EffectsUnsettled("tool operation admission is exhausted".into())
        })?;
        let id = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| ToolError::EffectsUnsettled("tool operation identity exhausted".into()))?;
        let job = Arc::new(Job {
            backend,
            cancellation,
            abandoned: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            _credit: credit,
        });
        self.jobs
            .lock()
            .map_err(|_| ToolError::EffectsUnsettled("tool ownership poisoned".into()))?
            .insert(id, Arc::clone(&job));
        Ok(InvocationOperation {
            owner: Arc::clone(self),
            id,
            job,
            finished: false,
        })
    }
    pub(crate) async fn settle(&self) -> Result<(), ToolError> {
        let jobs: Vec<_> = self
            .jobs
            .lock()
            .map_err(|_| ToolError::EffectsUnsettled("tool ownership poisoned".into()))?
            .iter()
            .filter(|(_, job)| job.abandoned.load(Ordering::Acquire))
            .map(|(id, job)| (*id, Arc::clone(job)))
            .collect();
        let mut failure = None;
        for (id, job) in jobs {
            match prove(&job).await {
                Ok(()) => {
                    self.jobs
                        .lock()
                        .map_err(|_| ToolError::EffectsUnsettled("tool ownership poisoned".into()))?
                        .remove(&id);
                }
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}
impl InvocationOperation {
    pub(crate) async fn finish(mut self) -> Result<(), ToolError> {
        prove(&self.job).await?;
        self.owner
            .jobs
            .lock()
            .map_err(|_| ToolError::EffectsUnsettled("tool ownership poisoned".into()))?
            .remove(&self.id);
        self.finished = true;
        Ok(())
    }
}
async fn prove(job: &Job) -> Result<(), ToolError> {
    if job.failed.load(Ordering::Acquire) {
        return Err(ToolError::EffectsUnsettled(
            "tool invocation settlement failed".into(),
        ));
    }
    let result = tokio::time::timeout(
        SETTLEMENT_TIMEOUT,
        AssertUnwindSafe(job.backend.settle_effects()).catch_unwind(),
    )
    .await;
    match result {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => {
            job.failed.store(true, Ordering::Release);
            Err(ToolError::EffectsUnsettled(format!(
                "tool invocation settlement failed: {error}"
            )))
        }
        Ok(Err(_)) => {
            job.failed.store(true, Ordering::Release);
            Err(ToolError::EffectsUnsettled(
                "tool invocation settlement panicked".into(),
            ))
        }
        Err(_) => {
            job.failed.store(true, Ordering::Release);
            Err(ToolError::EffectsUnsettled(
                "tool invocation settlement timed out".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests;
