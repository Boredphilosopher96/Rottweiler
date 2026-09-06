//! One replaceable, transient observation per admitted tool invocation.
use super::TurnSignal;
use crate::engine::SecretRedactor;
use rw_tools::{ToolError, ToolProgressSink};
use rw_types::{ToolInvocationId, ToolProgress};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const DELIVERY_INTERVAL: Duration = Duration::from_millis(250);

struct State {
    open: bool,
    queued: bool,
    latest: Option<ToolProgress>,
    last_enqueue: Option<Instant>,
}

pub(in crate::engine) struct ProgressSlot {
    pub(super) turn: u64,
    pub(super) id: String,
    pub(super) invocation_id: ToolInvocationId,
    state: Mutex<State>,
}

impl ProgressSlot {
    pub(super) fn take(&self) -> Option<ToolProgress> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.queued = false;
        if state.open {
            state.latest.take()
        } else {
            None
        }
    }
}

struct ProgressSink {
    slot: Arc<ProgressSlot>,
    signals: mpsc::UnboundedSender<TurnSignal>,
    redactor: Arc<dyn SecretRedactor>,
}

impl ToolProgressSink for ProgressSink {
    fn report(&self, update: ToolProgress) -> Result<(), ToolError> {
        let safe = ToolProgress::new(self.redactor.redact(update.message()), update.amount())
            .map_err(|error| ToolError::Output(error.to_string()))?;
        let mut state = self
            .slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.open {
            return Err(ToolError::Cancelled);
        }
        state.latest = Some(safe);
        if state.queued
            || state
                .last_enqueue
                .is_some_and(|last| last.elapsed() < DELIVERY_INTERVAL)
        {
            return Ok(());
        }
        state.queued = true;
        state.last_enqueue = Some(Instant::now());
        self.signals
            .send(TurnSignal::ToolProgress(Arc::clone(&self.slot)))
            .map_err(|_| ToolError::Cancelled)
    }
}

/// The execution future owns closure even when dropped or unwound.
pub(super) struct InvocationProgress {
    sink: Arc<ProgressSink>,
}

impl InvocationProgress {
    pub(super) fn new(
        turn: u64,
        id: String,
        invocation_id: ToolInvocationId,
        signals: mpsc::UnboundedSender<TurnSignal>,
        redactor: Arc<dyn SecretRedactor>,
    ) -> Self {
        Self {
            sink: Arc::new(ProgressSink {
                slot: Arc::new(ProgressSlot {
                    turn,
                    id,
                    invocation_id,
                    state: Mutex::new(State {
                        open: true,
                        queued: false,
                        latest: None,
                        last_enqueue: None,
                    }),
                }),
                signals,
                redactor,
            }),
        }
    }
    pub(super) fn sink(&self) -> Arc<dyn ToolProgressSink> {
        self.sink.clone()
    }
}

impl Drop for InvocationProgress {
    fn drop(&mut self) {
        let mut state = self
            .sink
            .slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.open = false;
        state.latest = None;
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::NoopSecretRedactor;

    #[test]
    fn producer_flood_retains_one_signal_and_drop_revokes_late_updates() {
        let (signals, mut receiver) = mpsc::unbounded_channel();
        let owner = InvocationProgress::new(
            1,
            "reused".to_owned(),
            ToolInvocationId("first".to_owned()),
            signals,
            Arc::new(NoopSecretRedactor),
        );
        let sink = owner.sink();
        for index in 0..100_000 {
            sink.report(ToolProgress::new(index.to_string(), None).expect("bounded progress"))
                .expect("admitted");
        }
        assert_eq!(receiver.len(), 1);
        let TurnSignal::ToolProgress(slot) = receiver.try_recv().expect("coalesced signal") else {
            panic!("progress signal")
        };
        assert_eq!(slot.take().expect("latest").message(), "99999");
        assert_eq!(slot.invocation_id.0, "first");
        drop(owner);
        assert!(slot.take().is_none());
        assert!(
            sink.report(ToolProgress::new("late".to_owned(), None).expect("progress"))
                .is_err()
        );
        assert_eq!(receiver.len(), 0);
    }
}
