//! Actual retained allocation charges shared across every session's UI registry.
use rw_ext::PluginRpcError;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const CAPACITY: usize = 8 * 1024 * 1024;
pub(super) const PREPARATION_BYTES: usize = 2 * 1024 * 1024;
const BASE_BYTES: usize = 64 * 1024;

pub(crate) struct UiBudget(Arc<Semaphore>);
impl Default for UiBudget {
    fn default() -> Self {
        Self(Arc::new(Semaphore::new(CAPACITY)))
    }
}
impl UiBudget {
    pub(super) fn prepare(&self) -> Result<OwnedSemaphorePermit, PluginRpcError> {
        self.reserve(PREPARATION_BYTES)
    }
    pub(super) fn base(&self) -> Result<OwnedSemaphorePermit, PluginRpcError> {
        self.reserve(BASE_BYTES)
    }
    fn reserve(&self, bytes: usize) -> Result<OwnedSemaphorePermit, PluginRpcError> {
        let bytes = u32::try_from(bytes).map_err(|_| super::error("UI allocation overflow"))?;
        self.0
            .clone()
            .try_acquire_many_owned(bytes)
            .map_err(|_| super::error("UI allocation capacity exhausted"))
    }
    pub(crate) fn close(&self) -> Result<(), PluginRpcError> {
        self.0.close();
        if self.0.available_permits() != CAPACITY {
            return Err(PluginRpcError {
                code: "effects_unsettled".into(),
                message: "UI registry allocations remain owned".into(),
            });
        }
        Ok(())
    }
}
pub(super) fn shrink(
    permit: &mut OwnedSemaphorePermit,
    bytes: usize,
) -> Result<(), PluginRpcError> {
    let spare = permit
        .num_permits()
        .checked_sub(bytes)
        .ok_or_else(|| super::error("UI prepared allocation limit"))?;
    drop(permit.split(spare));
    Ok(())
}
