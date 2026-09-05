//! Bounded blocking reads whose settlement follows actual worker completion.
use rw_core::AgentLoopError;
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

const MAX_SESSION_READ_JOBS: usize = 8;

#[derive(Debug)]
pub(super) struct ReadOperations {
    jobs: Mutex<Jobs>,
    changed: Notify,
    admission: Arc<Semaphore>,
}
#[derive(Debug, Default)]
struct Jobs {
    active: usize,
    failed: bool,
}
struct Operation(Arc<ReadOperations>);
impl Drop for Operation {
    fn drop(&mut self) {
        let mut jobs = self
            .0
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        jobs.failed |= std::thread::panicking();
        jobs.active -= 1;
        self.0.changed.notify_waiters();
    }
}
fn persistence(message: impl std::fmt::Display) -> AgentLoopError {
    AgentLoopError::Persistence(message.to_string())
}
impl ReadOperations {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            jobs: Mutex::new(Jobs::default()),
            changed: Notify::new(),
            admission: Arc::new(Semaphore::new(MAX_SESSION_READ_JOBS)),
        })
    }
    fn admit(self: &Arc<Self>) -> Result<(Operation, OwnedSemaphorePermit), AgentLoopError> {
        let permit = Arc::clone(&self.admission)
            .try_acquire_owned()
            .map_err(|_| persistence("session read admission exhausted"))?;
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| persistence("session read owner poisoned"))?;
        if jobs.failed {
            return Err(persistence("session read worker proof failed"));
        }
        jobs.active += 1;
        Ok((Operation(Arc::clone(self)), permit))
    }
    pub(super) async fn run<T: Send + 'static, R: Send + 'static>(
        self: &Arc<Self>,
        mut retained: R,
        query: impl FnOnce(&mut R) -> Result<T, AgentLoopError> + Send + 'static,
    ) -> Result<T, AgentLoopError> {
        let (operation, permit) = self.admit()?;
        let (result, _retained, _permit) = tokio::task::spawn_blocking(move || {
            let result = query(&mut retained);
            drop(operation);
            // The completed unconsumed reply remains charged until delivery/drop.
            (result, retained, permit)
        })
        .await
        .map_err(|error| persistence(format!("session read worker failed: {error}")))?;
        result
    }
    pub(super) async fn settle(&self) -> Result<(), AgentLoopError> {
        let wait = async {
            loop {
                let changed = self.changed.notified();
                {
                    let jobs = self
                        .jobs
                        .lock()
                        .map_err(|_| persistence("session read owner poisoned"))?;
                    if jobs.failed {
                        return Err(persistence("session read worker proof failed"));
                    }
                    if jobs.active == 0 {
                        return Ok(());
                    }
                }
                changed.await;
            }
        };
        if let Ok(result) = tokio::time::timeout(std::time::Duration::from_secs(30), wait).await {
            return result;
        }
        self.jobs
            .lock()
            .map_err(|_| persistence("session read owner poisoned"))?
            .failed = true;
        Err(persistence("session read effects remain unsettled"))
    }
    #[cfg(test)]
    pub(super) fn active(&self) -> usize {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
    }
}

#[cfg(test)]
mod tests;
