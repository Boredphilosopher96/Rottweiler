//! Shared staged admission. One preparation allowance covers a 64MiB prepared
//! event, 32MiB encoded source and 32MiB redaction workspace. Completed sources
//! and their projected request allocations share a separate 64MiB retained pool.
use rw_ext::PluginRpcError;
use rw_tools::CancellationToken;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(crate) const MAX_DELIVERY_DECODED_BYTES: usize = 64 * 1024 * 1024;
const MAX_RETAINED_BYTES: u32 = 64 * 1024 * 1024;
const MAX_WORKERS: usize = 64;

pub(crate) struct PluginDeliveryBudget {
    workers: Arc<Semaphore>,
    preparation: Arc<Semaphore>,
    retained: Arc<Semaphore>,
}
impl Default for PluginDeliveryBudget {
    fn default() -> Self {
        Self {
            workers: Arc::new(Semaphore::new(MAX_WORKERS)),
            preparation: Arc::new(Semaphore::new(1)),
            retained: Arc::new(Semaphore::new(MAX_RETAINED_BYTES as usize)),
        }
    }
}
impl PluginDeliveryBudget {
    pub(crate) fn worker(&self) -> Result<OwnedSemaphorePermit, PluginRpcError> {
        self.workers
            .clone()
            .try_acquire_owned()
            .map_err(|_| error("plugin event worker capacity exhausted"))
    }
    pub(crate) async fn prepare(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, PluginRpcError> {
        acquire(self.preparation.clone(), 1, cancellation).await
    }
    pub(crate) async fn retain(
        &self,
        bytes: usize,
        cancellation: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, PluginRpcError> {
        let bytes = u32::try_from(bytes)
            .ok()
            .filter(|bytes| *bytes <= MAX_RETAINED_BYTES)
            .ok_or_else(|| error("plugin event exceeds retained capacity"))?;
        acquire(self.retained.clone(), bytes, cancellation).await
    }
    pub(crate) fn close(&self) -> Result<(), PluginRpcError> {
        self.workers.close();
        self.preparation.close();
        self.retained.close();
        if self.workers.available_permits() != MAX_WORKERS
            || self.preparation.available_permits() != 1
            || self.retained.available_permits() != MAX_RETAINED_BYTES as usize
        {
            return Err(PluginRpcError {
                code: "effects_unsettled".into(),
                message: "plugin event resources remain owned".into(),
            });
        }
        Ok(())
    }
}
async fn acquire(
    semaphore: Arc<Semaphore>,
    count: u32,
    cancellation: &CancellationToken,
) -> Result<OwnedSemaphorePermit, PluginRpcError> {
    tokio::select! {
        biased;
        ()=cancellation.cancelled()=>Err(error("plugin event delivery cancelled")),
        permit=semaphore.acquire_many_owned(count)=>permit.map_err(|_|error("plugin event delivery admission closed")),
    }
}
fn error(message: &str) -> PluginRpcError {
    PluginRpcError {
        code: "event_delivery_unavailable".into(),
        message: message.into(),
    }
}
