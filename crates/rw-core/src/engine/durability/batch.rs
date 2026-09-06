use super::AgentLoopError;
use rw_types::{
    EngineEvent,
    allocation::{AllocationPlan, PreparedAllocation},
};
use std::sync::Arc;

/// Unmodified producer-owned events with a checked preparation allowance.
pub struct EventBatchPlan {
    allocation: AllocationPlan<Vec<EngineEvent>>,
}
impl EventBatchPlan {
    /// # Errors
    /// Rejects unsupported allocation depth and arithmetic overflow.
    pub fn new(events: Vec<EngineEvent>) -> Result<Self, AgentLoopError> {
        let allocation = AllocationPlan::new(events).map_err(|_| {
            AgentLoopError::Persistence("event batch allocation cannot be admitted".to_owned())
        })?;
        Ok(Self { allocation })
    }
    #[must_use]
    pub fn events(&self) -> &[EngineEvent] {
        self.allocation.value()
    }
    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.allocation.bytes()
    }
    #[must_use]
    pub fn prepare(self, reservation: EventBatchReservation) -> Arc<AdmittedEventBatch> {
        Arc::new(AdmittedEventBatch {
            allocation: self.allocation.prepare(),
            reservation,
        })
    }
}

/// Holds the sink's queue and byte credits until the consuming owner releases them.
pub struct EventBatchReservation {
    _owner: Box<dyn Send + Sync>,
}
impl EventBatchReservation {
    #[must_use]
    pub fn new(owner: impl Send + Sync + 'static) -> Self {
        Self {
            _owner: Box::new(owner),
        }
    }
}

/// Immutable submitted events and their retained resource charge.
pub struct AdmittedEventBatch {
    allocation: PreparedAllocation<Vec<EngineEvent>>,
    reservation: EventBatchReservation,
}
impl AdmittedEventBatch {
    #[must_use]
    pub fn events(&self) -> &[EngineEvent] {
        self.allocation.value()
    }
    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.allocation.bytes()
    }
    #[must_use]
    pub(in crate::engine) fn into_prepared_parts(
        self,
    ) -> (PreparedAllocation<Vec<EngineEvent>>, EventBatchReservation) {
        (self.allocation, self.reservation)
    }
    #[must_use]
    pub fn into_parts(self) -> (Vec<EngineEvent>, EventBatchReservation) {
        (self.allocation.into_inner(), self.reservation)
    }
}
