//! Required response carriers keep producer and wire admission through consumers.
use crate::{McpError, payload_work::Allocation};
use rw_types::allocation::{AllocationPlan, PrepareAllocation};
use std::{ops::Deref, sync::Arc};

/// Maximum simultaneous working allocation declared by a trusted client adapter.
/// It covers response construction and normalization before the carrier is returned.
#[derive(Clone, Copy)]
pub struct McpResponseLimits {
    working_bytes: usize,
}
impl McpResponseLimits {
    /// Declare the adapter's response construction ceiling, including spare capacity.
    ///
    /// # Errors
    /// Rejects zero or a ceiling above the shared pool's per-operation limit.
    pub fn new(working_bytes: usize) -> Result<Self, McpError> {
        if !(4096..=128 * 1024 * 1024).contains(&working_bytes) {
            return Err(invalid());
        }
        Ok(Self { working_bytes })
    }
    pub(crate) const WIRE: Self = Self {
        working_bytes: 64 * 1024,
    };
}

/// Acquired before invoking a client adapter, never after it has allocated a reply.
/// Raw transports additionally admit their exact wire and typed decode profiles.
pub struct McpResponseSlot {
    retained: Allocation,
    limit: usize,
}
impl McpResponseSlot {
    /// Acquire the client's declared working allowance before invoking it.
    pub fn new(limits: McpResponseLimits) -> Result<Self, McpError> {
        Ok(Self {
            retained: Allocation::new(limits.working_bytes)?,
            limit: limits.working_bytes,
        })
    }
    /// Transfer this already-acquired allowance into a native result owner.
    /// The result must retain it until its body is admitted by its next consumer.
    #[must_use]
    pub fn retain_native(self) -> impl Send + Sync + 'static {
        self.retained
    }

    pub(crate) fn into_retention(self) -> Arc<Allocation> {
        Arc::new(self.retained)
    }
    /// Normalize an adapter's constructed reply under its already-held admission.
    /// The adapter must keep all original backing allocations inside its declared limit.
    ///
    /// # Errors
    /// Rejects unsupported allocation shapes or simultaneous old/new capacity overflow.
    pub async fn adopt<T: PrepareAllocation + Send + 'static>(
        self,
        value: T,
    ) -> Result<McpResponse<T>, McpError> {
        let work = ConstructedResponse { value, slot: self };
        rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || work.run())
            .await
            .map_err(|_| invalid())?
    }
}

struct ConstructedResponse<T> {
    value: T,
    slot: McpResponseSlot,
}
impl<T: PrepareAllocation> ConstructedResponse<T> {
    fn run(mut self) -> Result<McpResponse<T>, McpError> {
        let plan = AllocationPlan::new(self.value).map_err(|_| invalid())?;
        let bytes = plan
            .bytes()
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(4096))
            .ok_or_else(invalid)?;
        if bytes > self.slot.limit {
            return Err(invalid());
        }
        let value = plan.prepare().into_inner();
        self.slot.retained.resize(bytes)?;
        Ok(McpResponse {
            value,
            retained: vec![Arc::new(self.slot.retained)],
        })
    }
}

/// An admitted response or catalog. Its body always retires before its byte leases.
/// There is deliberately no public extraction to an unowned value.
pub struct McpResponse<T> {
    pub(crate) value: T,
    pub(crate) retained: Vec<Arc<Allocation>>,
}
impl<T> Deref for McpResponse<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}
impl<T> McpResponse<T> {
    pub(crate) fn wire(value: T, retained: Vec<Arc<Allocation>>) -> Self {
        Self { value, retained }
    }
}
fn invalid() -> McpError {
    McpError::Protocol("MCP response construction admission exceeded".into())
}

impl<T> McpResponse<Vec<T>> {
    pub(crate) fn empty() -> Self {
        Self {
            value: Vec::new(),
            retained: Vec::new(),
        }
    }
}
impl<'a, T> IntoIterator for &'a McpResponse<Vec<T>> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.value.iter()
    }
}
