use rw_core::AgentLoopError;
use rw_store::checkpoint::{CheckpointCancellation, CheckpointOperation};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{OwnedMutexGuard, oneshot, watch};

/// The lock follows disk work and its unacknowledged result, including unwinding.
#[derive(Debug)]
pub(super) struct MutationGuard {
    _lock: OwnedMutexGuard<()>,
    poisoned: Arc<AtomicBool>,
    settled: bool,
}

impl MutationGuard {
    pub(super) fn new(lock: OwnedMutexGuard<()>, poisoned: Arc<AtomicBool>) -> Self {
        Self {
            _lock: lock,
            poisoned,
            settled: false,
        }
    }

    pub(super) fn complete(&mut self) {
        self.settled = true;
    }
}

impl Drop for MutationGuard {
    fn drop(&mut self) {
        if !self.settled {
            self.poisoned.store(true, Ordering::Release);
        }
    }
}

const MAX_WORKERS: usize = 8;

pub(super) struct CheckpointWorkers {
    active: watch::Sender<usize>,
}

struct WorkerCredit(watch::Sender<usize>);

struct CallerCancellation(CheckpointCancellation);
impl Drop for CallerCancellation {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Drop for WorkerCredit {
    fn drop(&mut self) {
        self.0.send_modify(|count| *count -= 1);
    }
}

impl CheckpointWorkers {
    pub(super) fn new() -> Self {
        Self {
            active: watch::channel(0).0,
        }
    }

    pub(super) async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(CheckpointOperation) -> Result<T, AgentLoopError> + Send + 'static,
    ) -> Result<T, AgentLoopError> {
        let admitted = self.active.send_if_modified(|count| {
            if *count == MAX_WORKERS {
                return false;
            }
            *count += 1;
            true
        });
        if !admitted {
            return Err(AgentLoopError::Persistence(
                "checkpoint worker capacity exhausted".to_owned(),
            ));
        }
        let credit = WorkerCredit(self.active.clone());
        let operation = CheckpointOperation::default();
        let _caller_cancellation = CallerCancellation(operation.cancellation());
        let task = tokio::task::spawn_blocking(move || work(operation));
        let (reply, receiver) = oneshot::channel();
        // Dropping the caller cannot detach disk work from this completion owner.
        tokio::spawn(async move {
            let result = task.await.unwrap_or_else(|error| {
                Err(AgentLoopError::Persistence(format!(
                    "checkpoint worker failed: {error}"
                )))
            });
            // A cancelled caller drops any unacknowledged guard before completion.
            drop(reply.send(result));
            drop(credit);
        });
        receiver.await.map_err(|_| {
            AgentLoopError::Persistence("checkpoint completion owner stopped".to_owned())
        })?
    }

    pub(super) async fn settle(&self) {
        let mut active = self.active.subscribe();
        while *active.borrow_and_update() != 0 {
            if active.changed().await.is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_caller_retains_lock_until_worker_and_result_are_dropped() {
        let workers = Arc::new(CheckpointWorkers::new());
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let poisoned = Arc::new(AtomicBool::new(false));
        let guard = MutationGuard::new(Arc::clone(&lock).lock_owned().await, Arc::clone(&poisoned));
        let (started, begun) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let caller_workers = Arc::clone(&workers);
        let caller = tokio::spawn(async move {
            caller_workers
                .run(move |_| {
                    started.send(()).expect("start");
                    blocked.recv().expect("release");
                    Ok(guard)
                })
                .await
        });
        begun.await.expect("begun");
        caller.abort();
        assert!(caller.await.expect_err("cancelled").is_cancelled());
        assert!(lock.try_lock().is_err());
        assert!(!poisoned.load(Ordering::Acquire));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), workers.settle())
                .await
                .is_err()
        );
        release.send(()).expect("release");
        tokio::time::timeout(Duration::from_secs(2), workers.settle())
            .await
            .expect("settled");
        assert!(lock.try_lock().is_ok());
        assert!(poisoned.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn panic_poisons_before_unlock_and_does_not_leak_completion() {
        let workers = CheckpointWorkers::new();
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let poisoned = Arc::new(AtomicBool::new(false));
        let guard = MutationGuard::new(Arc::clone(&lock).lock_owned().await, Arc::clone(&poisoned));
        let error = workers
            .run(move |_| -> Result<(), AgentLoopError> {
                let _guard = guard;
                panic!("checkpoint failure");
            })
            .await
            .expect_err("panic converted");
        assert!(error.to_string().contains("checkpoint worker failed"));
        tokio::time::timeout(Duration::from_secs(2), workers.settle())
            .await
            .expect("settled");
        assert!(poisoned.load(Ordering::Acquire));
        assert!(lock.try_lock().is_ok());
    }

    #[tokio::test]
    async fn acknowledged_result_transfers_lock_without_poisoning() {
        let workers = CheckpointWorkers::new();
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let poisoned = Arc::new(AtomicBool::new(false));
        let guard = MutationGuard::new(Arc::clone(&lock).lock_owned().await, Arc::clone(&poisoned));
        let mut guard = workers.run(move |_| Ok(guard)).await.expect("prepared");
        workers.settle().await;
        assert!(lock.try_lock().is_err());
        guard.complete();
        drop(guard);
        assert!(lock.try_lock().is_ok());
        assert!(!poisoned.load(Ordering::Acquire));
    }
}
