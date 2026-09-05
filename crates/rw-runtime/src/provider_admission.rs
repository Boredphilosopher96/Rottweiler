//! Application-owned, bounded storage admission for provider attempts.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use rw_core::provider_admission::{ActiveProviderCall, ProviderAdmission, ReservedProviderCall};
use rw_store::session::reservations::{
    BudgetLedger, BudgetReservationError as Error, BudgetReservationPlan, ProviderCallIdentity,
    ProviderCallReceipt,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};

/// Includes executing jobs and completed replies still held by their callers.
const MAX_STORAGE_JOBS: usize = 64;
type StorageAction = Box<dyn FnOnce(&mut BudgetLedger) + Send>;

enum Job {
    Apply(StorageAction),
    Shutdown,
}

struct Reply<T> {
    result: Result<T, Error>,
    _credit: OwnedSemaphorePermit,
}

struct Worker {
    sender: mpsc::Sender<Job>,
    credits: Arc<Semaphore>,
    closed: Arc<AtomicBool>,
    finished: watch::Receiver<Option<bool>>,
}

/// Reused by every session/provider route at one accounting root.
#[derive(Clone)]
pub struct DurableProviderAdmission {
    worker: Arc<Worker>,
}

impl DurableProviderAdmission {
    /// Opens the accounting connection on its owned storage worker.
    ///
    /// # Errors
    /// Returns schema, storage, worker-start or unresolved-history errors.
    pub async fn open(root: PathBuf) -> Result<Self, Error> {
        let (sender, mut receiver) = mpsc::channel(MAX_STORAGE_JOBS);
        let (ready, started) = oneshot::channel();
        let (finished, completion) = watch::channel(None);
        let closed = Arc::new(AtomicBool::new(false));
        let worker = Arc::new(Worker {
            sender,
            credits: Arc::new(Semaphore::new(MAX_STORAGE_JOBS)),
            closed: closed.clone(),
            finished: completion,
        });
        let task = tokio::task::spawn_blocking(move || {
            let mut ledger = match BudgetLedger::open(&root) {
                Ok(ledger) => ledger,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            let _ = ready.send(Ok(()));
            while let Some(Job::Apply(action)) = receiver.blocking_recv() {
                action(&mut ledger);
            }
        });
        // Completion follows JoinHandle settlement, including ledger/connection drops.
        tokio::spawn(async move {
            let result = task.await;
            closed.store(true, Ordering::Release);
            finished.send_replace(Some(result.is_ok()));
        });
        started.await.map_err(|_| stopped())??;
        Ok(Self { worker })
    }

    fn enqueue<T: Send + 'static>(
        &self,
        action: impl FnOnce(&mut BudgetLedger) -> Result<T, Error> + Send + 'static,
    ) -> Result<oneshot::Receiver<Reply<T>>, Error> {
        if self.worker.closed.load(Ordering::Acquire) {
            return Err(stopped());
        }
        let credit = self
            .worker
            .credits
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Capacity)?;
        let (sender, receiver) = oneshot::channel();
        let job = Job::Apply(Box::new(move |ledger| {
            let result = action(ledger);
            let _ = sender.send(Reply {
                result,
                _credit: credit,
            });
        }));
        self.worker
            .sender
            .try_send(job)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Error::Capacity,
                mpsc::error::TrySendError::Closed(_) => stopped(),
            })?;
        Ok(receiver)
    }

    async fn request<T: Send + 'static>(
        &self,
        action: impl FnOnce(&mut BudgetLedger) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        self.enqueue(action)?.await.map_err(|_| stopped())?.result
    }

    /// Reconciles one exact receipt read from a verified durable journal prefix.
    ///
    /// # Errors
    /// Returns bounded-admission, receipt or storage errors.
    pub async fn reconcile_accounted(&self, receipt: ProviderCallReceipt) -> Result<(), Error> {
        receipt.validate()?;
        self.request(move |ledger| ledger.reconcile_accounted(&receipt))
            .await
    }

    /// Closes admission, drains already-queued storage work and waits for worker settlement.
    /// Cancellation of this wait does not cancel the shutdown owner.
    ///
    /// # Errors
    /// Returns an error if the storage worker panicked or lost its completion channel.
    pub async fn shutdown(&self) -> Result<(), Error> {
        if !self.worker.closed.swap(true, Ordering::AcqRel) {
            let sender = self.worker.sender.clone();
            tokio::spawn(async move {
                let _ = sender.send(Job::Shutdown).await;
            });
        }
        let mut finished = self.worker.finished.clone();
        let result = *finished
            .wait_for(Option::is_some)
            .await
            .map_err(|_| stopped())?;
        if result == Some(true) {
            Ok(())
        } else {
            Err(stopped())
        }
    }
}

fn stopped() -> Error {
    Error::Worker("accounting worker has stopped".to_owned())
}

struct Reserved {
    service: DurableProviderAdmission,
    identity: ProviderCallIdentity,
}
struct Active {
    service: DurableProviderAdmission,
    identity: ProviderCallIdentity,
}

#[async_trait]
impl ProviderAdmission for DurableProviderAdmission {
    async fn reserve(
        &self,
        plan: BudgetReservationPlan,
    ) -> Result<Box<dyn ReservedProviderCall>, Error> {
        plan.validate()?;
        let identity = plan.identity.clone();
        self.request(move |ledger| ledger.reserve(&plan)).await?;
        Ok(Box::new(Reserved {
            service: self.clone(),
            identity,
        }))
    }
}

#[async_trait]
impl ReservedProviderCall for Reserved {
    async fn start(self: Box<Self>) -> Result<Box<dyn ActiveProviderCall>, Error> {
        let identity = self.identity.clone();
        self.service
            .request(move |ledger| ledger.start(&identity))
            .await?;
        Ok(Box::new(Active {
            service: self.service,
            identity: self.identity,
        }))
    }

    async fn cancel_unstarted(self: Box<Self>) -> Result<(), Error> {
        let identity = self.identity;
        self.service
            .request(move |ledger| ledger.cancel_unstarted(&identity))
            .await
    }
}

#[async_trait]
impl ActiveProviderCall for Active {
    async fn settle_accounted(&mut self, receipt: ProviderCallReceipt) -> Result<(), Error> {
        receipt.validate()?;
        if receipt.identity != self.identity {
            return Err(Error::IdentityConflict);
        }
        self.service
            .request(move |ledger| ledger.settle_accounted(&receipt))
            .await
    }
}

#[cfg(test)]
mod tests;
