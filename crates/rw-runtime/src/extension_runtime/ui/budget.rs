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

/// Shared by the configured and temporary registries of one session.
pub(crate) struct UiSessionBudget {
    panels: Arc<Semaphore>,
    wire: Arc<Semaphore>,
}
impl Default for UiSessionBudget {
    fn default() -> Self {
        Self {
            panels: Arc::new(Semaphore::new(rw_types::extension_ui::MAX_UI_PANEL_SLOTS)),
            wire: Arc::new(Semaphore::new(rw_types::extension_ui::MAX_UI_PANELS_BYTES)),
        }
    }
}
pub(super) struct PanelCredit {
    _slot: OwnedSemaphorePermit,
    bytes: OwnedSemaphorePermit,
    pool: Arc<Semaphore>,
}
impl UiSessionBudget {
    pub(super) fn panel(&self, bytes: usize) -> Result<PanelCredit, PluginRpcError> {
        let slot = self
            .panels
            .clone()
            .try_acquire_owned()
            .map_err(|_| super::error("session panel count exhausted"))?;
        let charge = u32::try_from(bytes).map_err(|_| super::error("panel charge overflow"))?;
        let bytes = self
            .wire
            .clone()
            .try_acquire_many_owned(charge)
            .map_err(|_| super::error("session panel wire capacity exhausted"))?;
        Ok(PanelCredit {
            _slot: slot,
            bytes,
            pool: self.wire.clone(),
        })
    }
}
impl PanelCredit {
    /// Existing views remain admitted until replacement is ready. Additional
    /// credits are acquired before resizing; rejection leaves the old charge intact.
    pub(super) fn resize(&mut self, bytes: usize) -> Result<(), PluginRpcError> {
        let current = self.bytes.num_permits();
        if bytes > current {
            let extra = u32::try_from(bytes - current)
                .map_err(|_| super::error("panel charge overflow"))?;
            let extra = self
                .pool
                .clone()
                .try_acquire_many_owned(extra)
                .map_err(|_| super::error("session panel wire capacity exhausted"))?;
            self.bytes.merge(extra);
        } else {
            drop(self.bytes.split(current - bytes));
        }
        Ok(())
    }
}
