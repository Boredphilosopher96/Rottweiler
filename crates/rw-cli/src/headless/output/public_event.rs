//! Admission precedes copying an immutable delivery into CLI-owned output.
use rw_types::{
    EngineEvent,
    allocation::{AllocationPlan, PrepareAllocation as _, PreparedAllocation},
};

pub(super) struct PublicEventPlan<'a> {
    event: &'a EngineEvent,
    bytes: usize,
}
impl<'a> PublicEventPlan<'a> {
    pub(super) fn new(event: &'a EngineEvent) -> Option<Self> {
        let mut bytes = event.prepared_bytes()?;
        if let EngineEvent::ThinkingDelta {
            signature: Some(signature),
            ..
        } = event
        {
            bytes = bytes.checked_sub(signature.capacity())?;
        }
        Some(Self { event, bytes })
    }
    pub(super) const fn bytes(&self) -> usize {
        self.bytes
    }
    pub(super) const fn value(&self) -> &EngineEvent {
        self.event
    }
    /// The caller reserves both original-copy and normalized storage before this
    /// operation. The immutable delivery's existing lease stays with its caller.
    pub(super) fn prepare(self) -> Option<PreparedAllocation<EngineEvent>> {
        let event = match self.event {
            EngineEvent::ThinkingDelta {
                meta,
                turn_id,
                text,
                ..
            } => EngineEvent::ThinkingDelta {
                meta: meta.clone(),
                turn_id: turn_id.clone(),
                text: text.clone(),
                signature: None,
            },
            event => event.clone(),
        };
        let plan = AllocationPlan::new(event).ok()?;
        if plan.bytes() > self.bytes {
            return None;
        }
        Some(plan.prepare())
    }
}
