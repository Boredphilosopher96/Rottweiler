//! Durable subscription delivery: append wakes a cursor reader without retaining payloads.
mod encoding;
mod redaction;
mod worker;

use super::durable_session::DurableEventSink;
use crate::extension_runtime::PluginEventRegistration;
use async_trait::async_trait;
use rw_core::{
    AgentLoopError, BudgetLedgerQuery, BudgetLedgerTotals, EngineEvent, SequenceId,
    SessionEventReadView, SessionEventSink,
};
use rw_providers::FixtureRedactor;
use std::sync::Arc;
use worker::PluginFanoutWorker;

pub(super) struct PluginFanoutEventSink {
    inner: Arc<DurableEventSink>,
    workers: Vec<PluginFanoutWorker>,
}
impl PluginFanoutEventSink {
    pub(super) fn new(
        inner: Arc<DurableEventSink>,
        registrations: Vec<PluginEventRegistration>,
        redactor: FixtureRedactor,
    ) -> Result<Self, AgentLoopError> {
        // Reserve every worker before starting any task, so constructor failure
        // cannot leave a partially registered set of event consumers.
        let permits = registrations
            .iter()
            .map(|registration| registration.budget.worker())
            .collect::<Result<Vec<_>, _>>()
            .map_err(error)?;
        let workers = registrations
            .into_iter()
            .zip(permits)
            .map(|(registration, permit)| {
                let source: Arc<dyn SessionEventSink> = inner.clone();
                PluginFanoutWorker::new(source, registration, permit, redactor.clone())
            })
            .collect();
        Ok(Self { inner, workers })
    }
    fn publish(&self, event: &EngineEvent) {
        if let Some(kind) = rw_types::extension_events::ExtensionEventKind::from_event(event) {
            for worker in &self.workers {
                worker.wake(kind);
            }
        }
    }
}
fn error(error: impl std::fmt::Display) -> AgentLoopError {
    AgentLoopError::Persistence(error.to_string())
}
#[async_trait]
impl SessionEventSink for PluginFanoutEventSink {
    async fn todo_state(
        &self,
    ) -> std::result::Result<rw_types::todo::TodoSnapshot, AgentLoopError> {
        self.inner.todo_state().await
    }
    async fn source_rewind_target(
        &self,
        expected_through: rw_types::SequenceId,
        source: rw_types::SequenceId,
        turn: u64,
        position: rw_types::RewindSourcePosition,
    ) -> std::result::Result<u64, AgentLoopError> {
        self.inner
            .source_rewind_target(expected_through, source, turn, position)
            .await
    }

    async fn extension_state(
        &self,
        plugin_id: &str,
    ) -> Result<rw_core::ExtensionStateView, AgentLoopError> {
        self.inner.extension_state(plugin_id).await
    }
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        // Cancellation is broadcast before waiting on any owner.
        for worker in &self.workers {
            worker.cancel();
        }
        let mut failure = None;
        for worker in &self.workers {
            if let Err(error) = worker.settle().await {
                failure.get_or_insert(error);
            }
        }
        if let Err(error) = self.inner.settle_effects().await {
            failure.get_or_insert(error);
        }
        failure.map_or(Ok(()), Err)
    }
    async fn reserve(
        &self,
        plan: &rw_core::EventBatchPlan,
    ) -> Result<rw_core::EventBatchReservation, AgentLoopError> {
        self.inner.reserve(plan).await
    }
    async fn commit(
        self: Arc<Self>,
        batch: Arc<rw_core::AdmittedEventBatch>,
    ) -> Result<Arc<rw_core::AdmittedEventBatch>, AgentLoopError> {
        let batch = self.inner.clone().commit(batch).await?;
        for event in batch.events() {
            self.publish(event);
        }
        Ok(batch)
    }
    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        self.inner.capture_read_view()
    }
    async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        self.inner.last_sequence().await
    }
    async fn budget_totals(
        &self,
        query: BudgetLedgerQuery,
    ) -> Result<BudgetLedgerTotals, AgentLoopError> {
        self.inner.budget_totals(query).await
    }
}
