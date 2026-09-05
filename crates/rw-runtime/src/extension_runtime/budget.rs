//! Application-wide plugin preparation, activation, residency and event delivery accounting.
use std::{sync::Arc, time::Duration};

use rw_ext::PluginRpcError;
use rw_tools::CancellationToken;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Instant,
};

use crate::source_plugin::SourcePreparationBudget;

const MAX_WAITERS: usize = 32;
const MAX_STARTING: usize = 32;
const PARALLEL_STARTS: usize = 2;
const MAX_RESIDENT_PROCESSES: usize = 32;
pub(super) const ACTIVATION_DEADLINE: Duration = Duration::from_secs(30);

/// Shared by every configured plugin generation in one application host.
/// Construction starts no workers and allocates no filesystem resources.
pub(crate) struct PluginRuntimeBudget {
    pub(crate) delivery: Arc<super::PluginDeliveryBudget>,
    waiters: Arc<Semaphore>,
    starts: Arc<Semaphore>,
    execution: Arc<Semaphore>,
    residents: Arc<Semaphore>,
    pub(super) preparation: Arc<SourcePreparationBudget>,
}
impl Default for PluginRuntimeBudget {
    fn default() -> Self {
        Self {
            delivery: Arc::new(super::PluginDeliveryBudget::default()),
            waiters: Arc::new(Semaphore::new(MAX_WAITERS)),
            starts: Arc::new(Semaphore::new(MAX_STARTING)),
            execution: Arc::new(Semaphore::new(PARALLEL_STARTS)),
            residents: Arc::new(Semaphore::new(MAX_RESIDENT_PROCESSES)),
            preparation: Arc::new(SourcePreparationBudget::default()),
        }
    }
}
impl PluginRuntimeBudget {
    pub(crate) fn close(&self) -> Result<(), PluginRpcError> {
        let delivery = self.delivery.close();
        self.waiters.close();
        self.starts.close();
        self.execution.close();
        self.residents.close();
        if self.waiters.available_permits() != MAX_WAITERS
            || self.starts.available_permits() != MAX_STARTING
            || self.execution.available_permits() != PARALLEL_STARTS
            || self.residents.available_permits() != MAX_RESIDENT_PROCESSES
        {
            return Err(super::activation::unsettled(
                "plugin activation capacity remains owned at application shutdown",
            ));
        }
        delivery
    }

    pub(super) fn waiter(&self) -> Result<OwnedSemaphorePermit, PluginRpcError> {
        Arc::clone(&self.waiters)
            .try_acquire_owned()
            .map_err(|_| exhausted())
    }
    pub(super) fn admit(&self) -> Result<ActivationLease, PluginRpcError> {
        let admission = Arc::clone(&self.starts)
            .try_acquire_owned()
            .map_err(|_| exhausted())?;
        Ok(ActivationLease {
            admission: Some(admission),
            execution: None,
            resident: None,
            proof_required: false,
        })
    }
    pub(super) async fn reserve_process(
        &self,
        lease: &mut ActivationLease,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<(), PluginRpcError> {
        lease.resident = Some(acquire(Arc::clone(&self.residents), cancellation, deadline).await?);
        lease.execution = Some(acquire(Arc::clone(&self.execution), cancellation, deadline).await?);
        Ok(())
    }
}

async fn acquire(
    semaphore: Arc<Semaphore>,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<OwnedSemaphorePermit, PluginRpcError> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(PluginRpcError { code: "cancelled".to_owned(), message: "plugin activation was cancelled".to_owned() }),
        () = tokio::time::sleep_until(deadline) => Err(PluginRpcError { code: "timeout".to_owned(), message: "plugin activation deadline expired".to_owned() }),
        permit = semaphore.acquire_owned() => permit.map_err(|_| exhausted()),
    }
}

/// Charged before preparation. A failed proof cannot return its capacity.
pub(super) struct ActivationLease {
    admission: Option<OwnedSemaphorePermit>,
    execution: Option<OwnedSemaphorePermit>,
    resident: Option<OwnedSemaphorePermit>,
    proof_required: bool,
}
impl ActivationLease {
    pub fn begin_effects(&mut self) {
        self.proof_required = true;
    }
    pub fn published(&mut self) {
        self.admission.take();
        self.execution.take();
    }
    pub fn settled(&mut self) {
        self.proof_required = false;
        self.admission.take();
        self.execution.take();
        self.resident.take();
    }
}
impl Drop for ActivationLease {
    fn drop(&mut self) {
        if self.proof_required {
            if let Some(permit) = self.admission.take() {
                permit.forget();
            }
            if let Some(permit) = self.execution.take() {
                permit.forget();
            }
            if let Some(permit) = self.resident.take() {
                permit.forget();
            }
        }
    }
}
fn exhausted() -> PluginRpcError {
    PluginRpcError {
        code: "busy".to_owned(),
        message: "plugin activation capacity is exhausted".to_owned(),
    }
}
