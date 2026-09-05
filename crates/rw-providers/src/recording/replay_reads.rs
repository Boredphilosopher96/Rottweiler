//! One owned fixture read, retained through completion or explicit failed proof.
use crate::{ProviderError, ProviderErrorKind};
use std::{
    io::Read,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

pub(super) const MAX_FIXTURE_BYTES: usize = 64 * 1024 * 1024;
const PROOF_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub(super) struct ReplayReads {
    current: Mutex<Option<Arc<Job>>>,
}
#[derive(Debug)]
struct Job {
    result: Mutex<Option<Result<Vec<u8>, ProviderError>>>,
    done: AtomicBool,
    abandoned: AtomicBool,
    failed: AtomicBool,
    changed: Notify,
}
pub(super) struct ReadLease {
    owner: Arc<ReplayReads>,
    job: Arc<Job>,
    consumed: bool,
    started: bool,
}
struct WorkerCompletion(Arc<Job>);
impl Drop for WorkerCompletion {
    fn drop(&mut self) {
        if !self.0.done.load(Ordering::Acquire) {
            self.0.failed.store(true, Ordering::Release);
            self.0.done.store(true, Ordering::Release);
            self.0.changed.notify_waiters();
        }
    }
}
impl Drop for ReadLease {
    fn drop(&mut self) {
        if !self.consumed {
            if self.started {
                self.job.abandoned.store(true, Ordering::Release);
            } else {
                let _ = self.owner.remove(&self.job);
            }
        }
    }
}
impl ReplayReads {
    #[cfg(test)]
    async fn run(
        self: &Arc<Self>,
        read: impl FnOnce() -> Result<Vec<u8>, ProviderError> + Send + 'static,
    ) -> Result<Vec<u8>, ProviderError> {
        self.begin()?.run(read).await
    }
    pub(super) fn begin(self: &Arc<Self>) -> Result<ReadLease, ProviderError> {
        let mut slot = self
            .current
            .lock()
            .map_err(|_| unsettled("replay read ownership poisoned"))?;
        if slot.is_some() {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "replay read admission exhausted",
            ));
        }
        let job = Arc::new(Job {
            result: Mutex::new(None),
            done: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            changed: Notify::new(),
        });
        *slot = Some(Arc::clone(&job));
        Ok(ReadLease {
            owner: Arc::clone(self),
            job,
            consumed: false,
            started: false,
        })
    }
    pub(super) async fn settle(&self) -> Result<(), ProviderError> {
        let job = self
            .current
            .lock()
            .map_err(|_| unsettled("replay read ownership poisoned"))?
            .clone();
        let Some(job) = job.filter(|job| job.abandoned.load(Ordering::Acquire)) else {
            return Ok(());
        };
        prove(&job).await?;
        self.remove(&job)
    }
    fn remove(&self, job: &Arc<Job>) -> Result<(), ProviderError> {
        let mut slot = self
            .current
            .lock()
            .map_err(|_| unsettled("replay read ownership poisoned"))?;
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, job))
        {
            *slot = None;
        }
        Ok(())
    }
}
impl ReadLease {
    pub(super) async fn read(self, path: PathBuf) -> Result<Vec<u8>, ProviderError> {
        self.run(move || read_fixture(path)).await
    }
    async fn run(
        mut self,
        read: impl FnOnce() -> Result<Vec<u8>, ProviderError> + Send + 'static,
    ) -> Result<Vec<u8>, ProviderError> {
        let completion = WorkerCompletion(Arc::clone(&self.job));
        self.started = true;
        // Construct before spawn so dropping a queued closure records failed proof.
        tokio::task::spawn_blocking(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(read))
                .unwrap_or_else(|_| {
                    Err(ProviderError::new(
                        ProviderErrorKind::Protocol,
                        "replay read worker panicked",
                    ))
                });
            *completion
                .0
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
            completion.0.done.store(true, Ordering::Release);
            completion.0.changed.notify_waiters();
        });
        self.finish().await
    }

    async fn finish(mut self) -> Result<Vec<u8>, ProviderError> {
        prove(&self.job).await?;
        let result = self
            .job
            .result
            .lock()
            .map_err(|_| unsettled("replay read result poisoned"))?
            .take()
            .ok_or_else(|| unsettled("replay read completed without result"))?;
        self.owner.remove(&self.job)?;
        self.consumed = true;
        result
    }
}
async fn prove(job: &Job) -> Result<(), ProviderError> {
    let wait = async {
        loop {
            let changed = job.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if job.failed.load(Ordering::Acquire) {
                return Err(unsettled("replay read settlement failed"));
            }
            if job.done.load(Ordering::Acquire) {
                return Ok(());
            }
            changed.await;
        }
    };
    if let Ok(result) = tokio::time::timeout(PROOF_TIMEOUT, wait).await {
        result
    } else {
        job.failed.store(true, Ordering::Release);
        Err(unsettled("replay read settlement timed out"))
    }
}
fn read_fixture(path: PathBuf) -> Result<Vec<u8>, ProviderError> {
    let mut file = std::fs::File::open(path).map_err(|_| {
        ProviderError::new(ProviderErrorKind::ReplayMiss, "replay fixture unavailable")
    })?;
    let len = usize::try_from(file.metadata().map_err(|error| io_error(&error))?.len())
        .map_err(|_| size_error())?;
    if len > MAX_FIXTURE_BYTES {
        return Err(size_error());
    }
    // A descriptor's admitted length plus one detects growth without an
    // unbounded read_to_end or geometric allocation beyond the byte limit.
    let mut bytes = vec![0; len + 1];
    let mut used = 0;
    while used < bytes.len() {
        match file.read(&mut bytes[used..]) {
            Ok(0) => break,
            Ok(count) => used += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io_error(&error)),
        }
    }
    if used > len {
        return Err(size_error());
    }
    bytes.truncate(used);
    Ok(bytes)
}
fn size_error() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Protocol,
        "replay fixture exceeds encoded byte admission or changed during read",
    )
}
fn io_error(error: &std::io::Error) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Protocol,
        format!("could not read replay fixture: {error}"),
    )
}
fn unsettled(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::EffectsUnsettled, message)
}

#[cfg(test)]
mod tests;
