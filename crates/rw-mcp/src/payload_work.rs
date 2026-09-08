//! Physical workers and result allocations outlive a dropped MCP payload caller.
use crate::McpError;
use rw_resources::{ResourceClass, ResourceLease};
use rw_tools::CancellationToken;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

/// One shared pool covers MCP encoding, redaction, payload I/O buffers and retained results.
const TOTAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_WORKING_BYTES: usize = 128 * 1024 * 1024;
const MAX_JOBS: usize = 64;
fn pool() -> &'static Arc<Semaphore> {
    static POOL: OnceLock<Arc<Semaphore>> = OnceLock::new();
    POOL.get_or_init(|| Arc::new(Semaphore::new(TOTAL_BYTES)))
}
#[derive(Debug)]
pub(crate) struct Allocation(OwnedSemaphorePermit);
impl Allocation {
    pub(crate) fn new(bytes: usize) -> Result<Self, McpError> {
        let mut allocation = Self(
            Arc::clone(pool())
                .try_acquire_many_owned(0)
                .map_err(|_| exhausted())?,
        );
        allocation.resize(bytes)?;
        Ok(allocation)
    }
    pub(crate) fn resize(&mut self, bytes: usize) -> Result<(), McpError> {
        if bytes > MAX_WORKING_BYTES {
            return Err(exhausted());
        }
        let previous = self.0.num_permits();
        if bytes > previous {
            let additional = u32::try_from(bytes - previous).map_err(|_| exhausted())?;
            self.0.merge(
                Arc::clone(pool())
                    .try_acquire_many_owned(additional)
                    .map_err(|_| exhausted())?,
            );
        } else {
            drop(self.0.split(previous - bytes));
        }
        Ok(())
    }
    pub(crate) fn ensure(&mut self, bytes: usize) -> std::io::Result<()> {
        if bytes > self.0.num_permits() {
            self.resize(bytes).map_err(std::io::Error::other)?;
        }
        Ok(())
    }
}
fn exhausted() -> McpError {
    McpError::Policy("MCP payload working allocation exhausted".into())
}

#[derive(Default)]
pub(crate) struct Jobs {
    pending: AtomicUsize,
    changed: Notify,
}
struct Job(Arc<Jobs>);
impl Drop for Job {
    fn drop(&mut self) {
        self.0.pending.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}
struct Caller(CancellationToken);
impl Drop for Caller {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
struct Work<F> {
    work: F,
    cancelled: CancellationToken,
    _job: Job,
    _resource: ResourceLease,
}
impl<F> Work<F> {
    fn run<T>(self) -> T
    where
        F: FnOnce(&CancellationToken) -> T,
    {
        let result = (self.work)(&self.cancelled);
        // These fields remain in this physical scope on success, error and unwinding.
        drop(self._resource);
        drop(self._job);
        result
    }
}
impl Jobs {
    pub(crate) async fn run<T: Send + 'static>(
        self: &Arc<Self>,
        class: ResourceClass,
        outer: CancellationToken,
        work: impl FnOnce(&CancellationToken) -> T + Send + 'static,
    ) -> Result<T, McpError> {
        self.pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                (pending < MAX_JOBS).then_some(pending + 1)
            })
            .map_err(|_| exhausted())?;
        let job = Job(Arc::clone(self));
        let cancelled = CancellationToken::default();
        let caller = Caller(cancelled.clone());
        let resource = rw_resources::acquire(class, async {
            tokio::select! { () = cancelled.cancelled() => {}, () = outer.cancelled() => {} }
        })
        .await
        .map_err(|error| McpError::Spool(error.to_string()))?;
        let owner = Work {
            work,
            cancelled,
            _job: job,
            _resource: resource,
        };
        let result = tokio::task::spawn_blocking(move || owner.run())
            .await
            .map_err(|_| McpError::Spool("payload worker panicked".into()))?;
        drop(caller);
        Ok(result)
    }
    pub(crate) async fn settle(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.pending.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    struct Marker(Arc<AtomicBool>);
    impl Drop for Marker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    #[tokio::test]
    async fn caller_drop_retains_worker_bytes_and_effect_settlement()
    -> Result<(), Box<dyn std::error::Error>> {
        let jobs = Arc::new(Jobs::default());
        let dropped = Arc::new(AtomicBool::new(false));
        let marker = Marker(Arc::clone(&dropped));
        let allocation = Allocation::new(512 * 1024)?;
        let (ready, started) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::sync_channel(1);
        let worker_jobs = Arc::clone(&jobs);
        let caller = tokio::spawn(async move {
            worker_jobs
                .run(
                    ResourceClass::Blocking,
                    CancellationToken::default(),
                    move |cancelled| {
                        let _owned = (marker, allocation);
                        let _ = ready.send(());
                        wait.recv()?;
                        assert!(cancelled.is_cancelled());
                        Ok::<_, std::sync::mpsc::RecvError>(())
                    },
                )
                .await
        });
        started.await?;
        caller.abort();
        assert!(caller.await.is_err());
        assert!(!dropped.load(Ordering::Acquire));
        assert_eq!(jobs.pending.load(Ordering::Acquire), 1);
        let settlement = jobs.settle();
        tokio::pin!(settlement);
        assert!(futures_util::poll!(settlement.as_mut()).is_pending());
        release.send(())?;
        settlement.await;
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(jobs.pending.load(Ordering::Acquire), 0);
        Ok(())
    }
}
