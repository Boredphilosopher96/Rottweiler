mod batch;
pub use batch::{AdmittedEventBatch, EventBatchPlan, EventBatchReservation};

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

/// One namespace and its aggregate admission counters from the same committed prefix.
#[derive(Clone, Debug)]
pub struct ExtensionStateView {
    pub snapshot: rw_types::extension_contract::ExtensionStateSnapshot,
    pub session_bytes: usize,
    pub namespaces: usize,
}

/// Provider/UI-neutral durability boundary for the sequenced session log.
///
/// Implementations must not return until the event is durably appended. The
/// actor invokes this boundary before making the event visible to subscribers.
#[async_trait]
pub trait SessionEventSink: Send + Sync {
    /// Read the authoritative task snapshot at the acknowledged committed prefix.
    async fn todo_state(&self) -> Result<rw_types::todo::TodoSnapshot, AgentLoopError>;

    /// Resolve an effective committed user source against an exact durable prefix.
    async fn source_rewind_target(
        &self,
        expected_through: SequenceId,
        source: SequenceId,
        turn: u64,
        position: rw_types::RewindSourcePosition,
    ) -> Result<u64, AgentLoopError>;

    /// Reads one bounded extension namespace from the canonical committed state.
    /// Implementations must include session aggregate counters from that same prefix.
    async fn extension_state(&self, plugin_id: &str) -> Result<ExtensionStateView, AgentLoopError>;

    /// Reserves all resources required to prepare, queue, commit and return this batch.
    async fn reserve(&self, plan: &EventBatchPlan)
    -> Result<EventBatchReservation, AgentLoopError>;

    /// Returns the exact submitted allocation only after durability and its owned
    /// postcommit work finish. Accepted work remains owned when its waiter drops.
    async fn commit(
        self: Arc<Self>,
        batch: Arc<AdmittedEventBatch>,
    ) -> Result<Arc<AdmittedEventBatch>, AgentLoopError>;

    /// Waits for accepted commit work, including abandoned waiters, before session resources close.
    async fn settle_effects(&self) -> Result<(), AgentLoopError>;

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
    async fn todo_state(
        &self,
    ) -> std::result::Result<rw_types::todo::TodoSnapshot, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "sink has no authoritative task state".into(),
        ))
    }
    async fn source_rewind_target(
        &self,
        _expected_through: rw_types::SequenceId,
        _source: rw_types::SequenceId,
        _turn: u64,
        _position: rw_types::RewindSourcePosition,
    ) -> std::result::Result<u64, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "sink has no canonical source index".into(),
        ))
    }

    async fn extension_state(
        &self,
        _plugin_id: &str,
    ) -> Result<crate::engine::ExtensionStateView, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "this ephemeral event sink does not provide durable extension state".to_owned(),
        ))
    }
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }
    async fn reserve(
        &self,
        _plan: &EventBatchPlan,
    ) -> Result<EventBatchReservation, AgentLoopError> {
        Ok(EventBatchReservation::new(()))
    }

    async fn commit(
        self: Arc<Self>,
        batch: Arc<AdmittedEventBatch>,
    ) -> Result<Arc<AdmittedEventBatch>, AgentLoopError> {
        let events = batch.events();
        let count = u64::try_from(events.len())
            .map_err(|_| AgentLoopError::Persistence("event batch length overflow".to_owned()))?;
        let mut next = self
            .next_sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let advanced = next
            .checked_add(count)
            .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?;
        for (offset, event) in events.iter().enumerate() {
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
            .extend(events.iter().cloned());
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

/// Reserves before preparing and validates the exact acknowledged allocation.
///
/// # Errors
/// Rejects admission, persistence failure and substituted batch ownership.
pub async fn commit_session_events<S: SessionEventSink + ?Sized + 'static>(
    sink: Arc<S>,
    events: Vec<EngineEvent>,
) -> Result<Arc<AdmittedEventBatch>, AgentLoopError> {
    let plan = EventBatchPlan::new(events)?;
    let reservation = sink.reserve(&plan).await?;
    let requested = plan.prepare(reservation);
    let committed = sink.commit(Arc::clone(&requested)).await?;
    if !Arc::ptr_eq(&requested, &committed) {
        return Err(AgentLoopError::Persistence(
            "event sink substituted the submitted batch".to_owned(),
        ));
    }
    Ok(committed)
}
