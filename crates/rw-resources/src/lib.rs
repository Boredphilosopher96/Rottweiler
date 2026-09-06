//! Shared admission belongs to the physical operation, through its actual settlement.
//!
//! Every session in this process competes for these pools. A process lease counts
//! one supervised process group, including helpers that exec into its worker; it
//! does not claim to count arbitrary descendants created inside that group.
//! Resource acquisition never authorizes an effect or replaces a caller's deadline.

use std::{
    future::Future,
    sync::{Arc, OnceLock},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Independently bounded physical workloads. Acquire only at their execution owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceClass {
    Process,
    Network,
    Blocking,
    Cpu,
}

/// A request either retains a bounded waiting slot or fails without starting work.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum AdmissionError {
    #[error("resource admission was cancelled")]
    Cancelled,
    #[error("resource admission queue is full")]
    QueueFull,
    #[error("resource execution capacity is full")]
    Busy,
    #[error("resource admission deadline exceeded")]
    Deadline,
    #[error("resource admission is closed")]
    Closed,
}

/// Move this lease into the operation's process, request, or worker owner.
/// Dropping a caller future is not a settlement boundary for detached work.
#[derive(Debug)]
pub struct ResourceLease {
    permit: OwnedSemaphorePermit,
}

impl ResourceLease {
    /// Permanently withdraw capacity when physical settlement cannot be proved.
    /// A process restart is required to restore this quarantined capacity.
    pub fn quarantine(self) {
        self.permit.forget();
    }
}

struct Pool {
    execution: Arc<Semaphore>,
    waiting: Arc<Semaphore>,
}
impl Pool {
    fn new(execution: usize, waiting: usize) -> Self {
        Self {
            execution: Arc::new(Semaphore::new(execution)),
            waiting: Arc::new(Semaphore::new(waiting)),
        }
    }
    fn try_acquire(&self) -> Result<ResourceLease, AdmissionError> {
        self.execution
            .clone()
            .try_acquire_owned()
            .map(|permit| ResourceLease { permit })
            .map_err(|error| {
                if matches!(error, tokio::sync::TryAcquireError::Closed) {
                    AdmissionError::Closed
                } else {
                    AdmissionError::Busy
                }
            })
    }
    async fn acquire(
        &self,
        cancelled: impl Future<Output = ()>,
    ) -> Result<ResourceLease, AdmissionError> {
        let waiting = self.waiting.clone().try_acquire_owned().map_err(|error| {
            if matches!(error, tokio::sync::TryAcquireError::Closed) {
                AdmissionError::Closed
            } else {
                AdmissionError::QueueFull
            }
        })?;
        tokio::select! {
            biased;
            () = cancelled => Err(AdmissionError::Cancelled),
            () = tokio::time::sleep(std::time::Duration::from_secs(30)) => Err(AdmissionError::Deadline),
            result = self.execution.clone().acquire_owned() => {
                drop(waiting);
                result.map(|permit| ResourceLease { permit }).map_err(|_| AdmissionError::Closed)
            }
        }
    }
}

fn pool(class: ResourceClass) -> &'static Pool {
    static PROCESS: OnceLock<Pool> = OnceLock::new();
    static NETWORK: OnceLock<Pool> = OnceLock::new();
    static BLOCKING: OnceLock<Pool> = OnceLock::new();
    static CPU: OnceLock<Pool> = OnceLock::new();
    match class {
        ResourceClass::Process => PROCESS.get_or_init(|| Pool::new(64, 64)),
        ResourceClass::Network => NETWORK.get_or_init(|| Pool::new(64, 64)),
        ResourceClass::Blocking => BLOCKING.get_or_init(|| Pool::new(16, 64)),
        ResourceClass::Cpu => CPU.get_or_init(|| {
            Pool::new(
                std::thread::available_parallelism()
                    .map_or(1, usize::from)
                    .min(4),
                64,
            )
        }),
    }
}

/// Wait in the process-wide class queue, cancelling without starting effects.
///
/// # Errors
/// Returns cancellation, queue exhaustion, or closure before granting a lease.
#[tracing::instrument(target = "rw_performance", level = "trace", name = "resource.admission_wait", skip(cancelled), fields(?class))]
pub async fn acquire(
    class: ResourceClass,
    cancelled: impl Future<Output = ()>,
) -> Result<ResourceLease, AdmissionError> {
    pool(class).acquire(cancelled).await
}

/// Admit synchronous launch work without adding an unbounded waiting caller.
///
/// # Errors
/// Returns capacity exhaustion or closure before granting a lease.
pub fn try_acquire(class: ResourceClass) -> Result<ResourceLease, AdmissionError> {
    pool(class).try_acquire()
}

/// Failure before execution or while joining the admitted physical worker.
#[derive(Debug, thiserror::Error)]
pub enum WorkError {
    #[error("{0}")]
    Admission(#[from] AdmissionError),
    #[error("physical worker failed: {0}")]
    Worker(#[from] tokio::task::JoinError),
}

/// Run a finite blocking operation with process-wide execution admission.
/// The real worker retains capacity if its async caller disappears. Cleanup
/// that allows existing operations to finish must not queue behind their pool.
///
/// # Errors
/// Rejects exhausted admission or a failed worker before returning its result.
pub async fn run_blocking<T: Send + 'static>(
    class: ResourceClass,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, WorkError> {
    let lease = acquire(class, std::future::pending()).await?;
    let span = tracing::Span::current();
    Ok(tokio::task::spawn_blocking(move || {
        let _lease = lease;
        span.in_scope(work)
    })
    .await?)
}

#[cfg(test)]
mod tests;
