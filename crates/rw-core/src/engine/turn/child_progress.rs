//! Coalesced child observations never hold up durable lifecycle settlement.
use rw_tools::{ChildProgressBudget, ChildProgressRetention, SubagentProgressEvent, ToolError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub(in crate::engine) struct ChildProgressSlot {
    pending: Mutex<Option<AdmittedProgress>>,
    budget: ChildProgressBudget,
    missed: AtomicBool,
}
pub(in crate::engine) struct AdmittedProgress {
    pub(in crate::engine) event: SubagentProgressEvent,
    _permit: ChildProgressRetention,
}
impl ChildProgressSlot {
    pub(in crate::engine) fn new(budget: ChildProgressBudget) -> Arc<Self> {
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
        if !self.budget.owns(&event.event) {
            return Err(ToolError::Output(
                "child preview belongs to another publisher".into(),
            ));
        }
        if event.event.value().is_null() && event.child_sequence.is_none() {
            return Err(ToolError::Output(
                "child progress invalidation requires a canonical sequence".into(),
            ));
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.missed.load(Ordering::Relaxed) {
            if event.child_sequence.is_none() {
                return Ok(());
            }
            event.event.invalidate();
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
            event.event.invalidate();
        }
        let charge = event
            .subagent_id
            .0
            .capacity()
            .checked_add(event.child_session_id.0.capacity())
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<AdmittedProgress>()));
        let permit = charge.and_then(|bytes| self.budget.reserve(bytes));
        let permit = if let Some(permit) = permit {
            permit
        } else {
            if event.child_sequence.is_none() {
                return Ok(());
            }
            event.event.invalidate();
            // A prior pending slot already owns enough for its scalar marker.
            if let Some(previous) = pending.as_mut() {
                previous.event.child_sequence = event.child_sequence;
                previous.event.event.invalidate();
                return Ok(());
            }
            if let Some(permit) = charge.and_then(|bytes| self.budget.reserve(bytes)) {
                permit
            } else {
                // No display allocation is required for durable progress to finish.
                self.missed.store(true, Ordering::Relaxed);
                return Ok(());
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
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::turn::TurnSignal;
    use rw_types::{SessionId, SubagentId};
    use tokio::sync::mpsc;
    fn event(budget: &ChildProgressBudget, sequence: u64, text: String) -> SubagentProgressEvent {
        SubagentProgressEvent {
            subagent_id: SubagentId("child".into()),
            child_session_id: SessionId("session".into()),
            child_sequence: Some(sequence),
            event: budget
                .encode_value(Some(sequence), &serde_json::Value::String(text))
                .expect("encode")
                .expect("preview"),
        }
    }
    #[test]
    fn a_preview_cannot_spend_another_publishers_credit() {
        let source = ChildProgressBudget::default();
        let destination = ChildProgressBudget::default();
        let slot = ChildProgressSlot::new(destination.clone());
        let preview = event(&source, 1, "owned".into());
        assert!(
            slot.publish(preview, |_| panic!("foreign preview must not queue"))
                .is_err()
        );
        assert!(slot.take().is_none());
        assert_eq!(
            source.available_bytes(),
            rw_tools::CHILD_PROGRESS_MEMORY_BYTES
        );
        assert_eq!(
            destination.available_bytes(),
            rw_tools::CHILD_PROGRESS_MEMORY_BYTES
        );
    }
    #[test]
    fn flooded_child_has_one_source_invalidation_and_one_queued_signal() {
        let budget = ChildProgressBudget::default();
        let slot = ChildProgressSlot::new(budget.clone());
        let (send, mut receive) = mpsc::unbounded_channel();
        for sequence in 0..10_000 {
            slot.publish(event(&budget, sequence, "delta".into()), |slot| {
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
        assert!(value.event.event.value().is_null());
        drop(value);
        assert_eq!(
            budget.available_bytes(),
            rw_tools::CHILD_PROGRESS_MEMORY_BYTES
        );
        slot.publish(event(&budget, 10_000, "next".into()), |slot| {
            send.send(TurnSignal::SubagentProgress(slot)).is_ok()
        })
        .expect("next");
        assert_eq!(receive.len(), 1);
    }
    #[test]
    fn shared_memory_pressure_yields_source_markers_without_blocking() {
        let budget = ChildProgressBudget::default();
        let first = ChildProgressSlot::new(budget.clone());
        let second = ChildProgressSlot::new(budget.clone());
        let (send, receive) = mpsc::unbounded_channel();
        first
            .publish(event(&budget, 1, "x".repeat(2500)), |slot| {
                send.send(TurnSignal::SubagentProgress(slot)).is_ok()
            })
            .expect("first");
        let occupied = budget
            .reserve(budget.available_bytes() - 4096)
            .expect("pressure");
        second
            .publish(event(&budget, 2, "x".repeat(2500)), |slot| {
                send.send(TurnSignal::SubagentProgress(slot)).is_ok()
            })
            .expect("second");
        assert_eq!(receive.len(), 2);
        let marker = second.take().expect("marker");
        assert!(marker.event.event.value().is_null());
        assert!(budget.available_bytes() < 4096);
        drop(marker);
        drop(first.take());
        drop(occupied);
        assert_eq!(
            budget.available_bytes(),
            rw_tools::CHILD_PROGRESS_MEMORY_BYTES
        );
    }
    #[test]
    fn dropped_marker_requires_invalidation_when_memory_returns() {
        let budget = ChildProgressBudget::default();
        let occupied = budget
            .reserve(rw_tools::CHILD_PROGRESS_MEMORY_BYTES)
            .expect("occupied budget");
        let slot = ChildProgressSlot::new(budget.clone());
        slot.publish(event(&budget, 1, "lost".into()), |_| {
            panic!("no marker fits")
        })
        .expect("nonblocking loss");
        drop(occupied);
        let mut queued = false;
        slot.publish(event(&budget, 2, "new delta".into()), |_| {
            queued = true;
            true
        })
        .expect("recover source");
        assert!(queued);
        let marker = slot.take().expect("queued marker");
        assert_eq!(marker.event.child_sequence, Some(2));
        assert!(
            marker.event.event.value().is_null(),
            "a delta cannot repair the missing source"
        );
        drop(marker);
        assert_eq!(
            budget.available_bytes(),
            rw_tools::CHILD_PROGRESS_MEMORY_BYTES
        );
    }
}
