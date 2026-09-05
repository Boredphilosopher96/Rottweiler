//! Coalesced child observations never hold up durable lifecycle settlement.
use rw_tools::{SubagentProgressEvent, ToolError};
use rw_types::allocation::PrepareAllocation;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(in crate::engine) const PROGRESS_MEMORY_BYTES: usize = 8 * 1024 * 1024;
pub(in crate::engine) struct ChildProgressSlot {
    pending: Mutex<Option<AdmittedProgress>>,
    budget: Arc<Semaphore>,
    missed: AtomicBool,
}
pub(in crate::engine) struct AdmittedProgress {
    pub(in crate::engine) event: SubagentProgressEvent,
    _permit: OwnedSemaphorePermit,
}
impl ChildProgressSlot {
    pub(in crate::engine) fn new(budget: Arc<Semaphore>) -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(None),
            budget,
            missed: AtomicBool::new(false),
        })
    }
    pub(in crate::engine) fn publish(
        self: &Arc<Self>,
        mut event: SubagentProgressEvent,
        enqueue: impl FnOnce(Arc<Self>) -> bool,
    ) -> Result<(), ToolError> {
        event.event = crate::orchestration::progress::admit(event.child_sequence, event.event)
            .map_err(|error| ToolError::Output(error.to_string()))?;
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.missed.load(Ordering::Relaxed) {
            if event.child_sequence.is_none() {
                return Ok(());
            }
            event.event = serde_json::Value::Null;
        }
        let queued = pending.is_some();
        if let Some(previous) = pending.as_ref() {
            let Some(sequence) = event.child_sequence else {
                return Ok(());
            };
            if previous
                .event
                .child_sequence
                .is_some_and(|previous| previous >= sequence)
            {
                return Ok(());
            }
            // Replacing deltas loses information. Only canonical invalidation is
            // truthful; the client reads the complete source at this fence.
            event.event = serde_json::Value::Null;
        }
        let charge = event
            .event
            .prepared_bytes()
            .and_then(|bytes| {
                bytes
                    .checked_add(event.subagent_id.0.capacity())?
                    .checked_add(event.child_session_id.0.capacity())?
                    .checked_add(std::mem::size_of::<AdmittedProgress>())
            })
            .and_then(|bytes| u32::try_from(bytes).ok());
        let permit =
            charge.and_then(|bytes| self.budget.clone().try_acquire_many_owned(bytes).ok());
        let permit = if let Some(permit) = permit {
            permit
        } else {
            if event.child_sequence.is_none() {
                return Ok(());
            }
            event.event = serde_json::Value::Null;
            // A prior pending slot already owns enough for its scalar marker.
            if let Some(previous) = pending.as_mut() {
                previous.event.child_sequence = event.child_sequence;
                previous.event.event = serde_json::Value::Null;
                return Ok(());
            }
            let bytes = std::mem::size_of::<AdmittedProgress>()
                + event.subagent_id.0.capacity()
                + event.child_session_id.0.capacity();
            let bytes = u32::try_from(bytes).map_err(|_| {
                ToolError::Output("child progress identity exceeds admission".into())
            })?;
            match self.budget.clone().try_acquire_many_owned(bytes) {
                Ok(permit) => permit,
                // No display allocation is required for durable progress to finish.
                Err(_) => {
                    self.missed.store(true, Ordering::Relaxed);
                    return Ok(());
                }
            }
        };
        *pending = Some(AdmittedProgress {
            event,
            _permit: permit,
        });
        if !queued {
            if enqueue(self.clone()) {
                self.missed.store(false, Ordering::Relaxed);
            } else {
                pending.take();
                self.missed.store(true, Ordering::Relaxed);
            }
        }
        Ok(())
    }
    pub(in crate::engine) fn take(&self) -> Option<AdmittedProgress> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::turn::TurnSignal;
    use rw_types::{SessionId, SubagentId};
    use tokio::sync::mpsc;
    fn event(sequence: u64, text: String) -> SubagentProgressEvent {
        SubagentProgressEvent {
            subagent_id: SubagentId("child".into()),
            child_session_id: SessionId("session".into()),
            child_sequence: Some(sequence),
            event: serde_json::Value::String(text),
        }
    }
    #[test]
    fn flooded_child_has_one_source_invalidation_and_one_queued_signal() {
        let budget = Arc::new(Semaphore::new(PROGRESS_MEMORY_BYTES));
        let slot = ChildProgressSlot::new(budget.clone());
        let (send, mut receive) = mpsc::unbounded_channel();
        for sequence in 0..10_000 {
            slot.publish(event(sequence, "delta".into()), |slot| {
                send.send(TurnSignal::SubagentProgress(slot)).is_ok()
            })
            .expect("progress");
        }
        assert_eq!(receive.len(), 1);
        let TurnSignal::SubagentProgress(queued) = receive.try_recv().expect("signal") else {
            panic!("progress signal")
        };
        let value = queued.take().expect("value");
        assert_eq!(value.event.child_sequence, Some(9_999));
        assert!(value.event.event.is_null());
        drop(value);
        assert_eq!(budget.available_permits(), PROGRESS_MEMORY_BYTES);
        slot.publish(event(10_000, "next".into()), |slot| {
            send.send(TurnSignal::SubagentProgress(slot)).is_ok()
        })
        .expect("next");
        assert_eq!(receive.len(), 1);
    }
    #[test]
    fn shared_memory_pressure_yields_source_markers_without_blocking() {
        let budget = Arc::new(Semaphore::new(4096));
        let first = ChildProgressSlot::new(budget.clone());
        let second = ChildProgressSlot::new(budget.clone());
        let (send, receive) = mpsc::unbounded_channel();
        first
            .publish(event(1, "x".repeat(2500)), |slot| {
                send.send(TurnSignal::SubagentProgress(slot)).is_ok()
            })
            .expect("first");
        second
            .publish(event(2, "x".repeat(2500)), |slot| {
                send.send(TurnSignal::SubagentProgress(slot)).is_ok()
            })
            .expect("second");
        assert_eq!(receive.len(), 2);
        let marker = second.take().expect("marker");
        assert!(marker.event.event.is_null());
        assert!(budget.available_permits() < 4096);
        drop(marker);
        drop(first.take());
        assert_eq!(budget.available_permits(), 4096);
    }
    #[test]
    fn dropped_marker_requires_invalidation_when_memory_returns() {
        let budget = Arc::new(Semaphore::new(4096));
        let occupied = budget
            .clone()
            .try_acquire_many_owned(4096)
            .expect("occupied budget");
        let slot = ChildProgressSlot::new(budget.clone());
        slot.publish(event(1, "lost".into()), |_| panic!("no marker fits"))
            .expect("nonblocking loss");
        drop(occupied);
        let mut queued = false;
        slot.publish(event(2, "new delta".into()), |_| {
            queued = true;
            true
        })
        .expect("recover source");
        assert!(queued);
        let marker = slot.take().expect("queued marker");
        assert_eq!(marker.event.child_sequence, Some(2));
        assert!(
            marker.event.event.is_null(),
            "a delta cannot repair the missing source"
        );
        drop(marker);
        assert_eq!(budget.available_permits(), 4096);
    }
}
