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

    /// Transform a borrowed response under a destination allowance acquired before
    /// construction. The physical CPU worker retains the source and allowance even
    /// when its caller disappears. The trusted projection must bound every temporary
    /// and its result by the declared slot; the result cannot borrow the source.
    pub async fn project<U: Send + 'static>(
        self,
        slot: McpResponseSlot,
        project: impl FnOnce(&T) -> Result<U, McpError> + Send + 'static,
    ) -> Result<McpResponse<U>, McpError>
    where
        T: Send + 'static,
    {
        let work = Projection {
            source: self,
            slot,
            project,
        };
        rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || work.run())
            .await
            .map_err(|_| invalid())?
    }
}

struct Projection<T, F> {
    source: McpResponse<T>,
    project: F,
    slot: McpResponseSlot,
}
impl<T, U, F: FnOnce(&T) -> Result<U, McpError>> Projection<T, F> {
    fn run(self) -> Result<McpResponse<U>, McpError> {
        let value = (self.project)(&self.source);
        value.map(|value| McpResponse::wire(value, vec![self.slot.into_retention()]))
    }
}

impl<T> AsRef<T> for McpResponse<T> {
    fn as_ref(&self) -> &T {
        &self.value
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    #[tokio::test]
    async fn abandoned_projection_retains_source_until_physical_worker_finishes() {
        let source = McpResponseSlot::new(McpResponseLimits::new(8192).expect("limit"))
            .expect("slot")
            .adopt(String::from("source"))
            .await
            .expect("source");
        let retained = Arc::downgrade(&source.retained[0]);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let slot =
            McpResponseSlot::new(McpResponseLimits::new(8192).expect("limit")).expect("slot");
        let caller = tokio::spawn(source.project(slot, move |source| {
            let _ = started_tx.send(());
            release_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("release");
            Ok(source.clone())
        }));
        started_rx.await.expect("started");
        caller.abort();
        assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
        assert!(
            retained.upgrade().is_some(),
            "physical source remains charged"
        );
        release_tx.send(()).expect("release");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while retained.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("physical completion");
    }
}
