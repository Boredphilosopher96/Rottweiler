use super::AgentLoopError;
use super::BudgetLedgerQuery;
use super::BudgetLedgerTotals;
use super::replay;
use super::replay::SessionEventReadView;
use async_trait::async_trait;
use rw_types::EngineEvent;
use rw_types::SequenceId;
use std::sync::Arc;
use std::sync::Mutex;

/// Provider/UI-neutral durability boundary for the sequenced session log.
///
/// Implementations must not return until the event is durably appended. The
/// actor invokes this boundary before making the event visible to subscribers.
#[async_trait]
pub trait SessionEventSink: Send + Sync {
    /// Durably append exactly the fully stamped protocol event supplied by the
    /// actor and return that same event after persistence completes.
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError>;

    /// Appends an ordered event batch.
    ///
    /// The extensible default appends sequentially and may leave a recoverable
    /// persisted prefix if a later append fails. Implementations with a native
    /// batch primitive should override this to share one durable sync.
    async fn append_batch(
        &self,
        batch: Vec<EngineEvent>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        let mut events = Vec::with_capacity(batch.len());
        for event in batch {
            events.push(self.append(event).await?);
        }
        Ok(events)
    }

    /// Captures an immutable acknowledged prefix for bounded replay.
    ///
    /// # Errors
    /// Rejects unavailable storage or exhausted read admission.
    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError>;

    /// Returns the current durable tail without relying on the finite live
    /// broadcast buffer.
    async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        Ok(self.capture_read_view()?.last_sequence())
    }

    /// Returns reconciled session, UTC-day, and trailing-minute spend totals.
    /// Ephemeral sinks have no cross-session ledger and therefore return zero.
    async fn budget_totals(
        &self,
        _query: BudgetLedgerQuery,
    ) -> Result<BudgetLedgerTotals, AgentLoopError> {
        Ok(BudgetLedgerTotals::default())
    }
}

/// Event sink for ephemeral sessions and deterministic unit tests.
#[derive(Debug, Default)]
pub struct NoopSessionEventSink {
    pub(super) next_sequence: Mutex<u64>,
    pub(super) events: Arc<Mutex<Vec<EngineEvent>>>,
}

impl NoopSessionEventSink {
    #[must_use]
    pub fn new(last_sequence: Option<SequenceId>) -> Self {
        Self {
            next_sequence: Mutex::new(
                last_sequence.map_or(0, |sequence| sequence.0.saturating_add(1)),
            ),
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl SessionEventSink for NoopSessionEventSink {
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        let mut next = self
            .next_sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let meta = event.meta().ok_or_else(|| {
            AgentLoopError::Persistence(
                "connection-scoped acknowledgement cannot enter a session log".to_owned(),
            )
        })?;
        if meta.sequence_id.0 != *next {
            return Err(AgentLoopError::Persistence(format!(
                "event sequence {} does not match expected {}",
                meta.sequence_id.0, *next
            )));
        }
        *next = next
            .checked_add(1)
            .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?;
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event.clone());
        Ok(event)
    }

    async fn append_batch(
        &self,
        batch: Vec<EngineEvent>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        let count = u64::try_from(batch.len())
            .map_err(|_| AgentLoopError::Persistence("event batch length overflow".to_owned()))?;
        let mut next = self
            .next_sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let advanced = next
            .checked_add(count)
            .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?;
        for (offset, event) in batch.iter().enumerate() {
            let offset = u64::try_from(offset)
                .map_err(|_| AgentLoopError::Persistence("event batch overflow".to_owned()))?;
            let sequence = next
                .checked_add(offset)
                .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?;
            let meta = event.meta().ok_or_else(|| {
                AgentLoopError::Persistence(
                    "connection-scoped acknowledgement cannot enter a session log".to_owned(),
                )
            })?;
            if meta.sequence_id.0 != sequence {
                return Err(AgentLoopError::Persistence(format!(
                    "event sequence {} does not match expected {sequence}",
                    meta.sequence_id.0
                )));
            }
        }
        *next = advanced;
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(batch.iter().cloned());
        Ok(batch)
    }

    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        Ok(Arc::new(replay::MemoryEventReadView::new(
            Arc::clone(&self.events),
            self.next_sequence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .checked_sub(1)
                .map(SequenceId),
        )))
    }

    async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        let next = *self
            .next_sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(next.checked_sub(1).map(SequenceId))
    }
}
