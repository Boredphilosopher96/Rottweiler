//! Durable subscription delivery: append wakes a cursor reader without retaining payloads.
mod encoding;
mod generations;
mod redaction;
mod worker;
pub(crate) use generations::PreparedPluginDelivery;

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

pub(crate) struct PluginFanoutEventSink {
    inner: Arc<DurableEventSink>,
    workers: std::sync::RwLock<generations::DeliveryGeneration>,
    redactor: FixtureRedactor,
}
impl PluginFanoutEventSink {
    pub(super) fn new(
        inner: Arc<DurableEventSink>,
        registrations: Vec<PluginEventRegistration>,
        redactor: &FixtureRedactor,
    ) -> Result<Self, AgentLoopError> {
        let workers = generations::workers(&inner, registrations, redactor)?;
        for worker in &workers {
            worker.activate();
        }
        Ok(Self {
            inner,
            redactor: redactor.clone(),
            workers: std::sync::RwLock::new(generations::DeliveryGeneration {
                workers: Arc::from(workers),
                paused: false,
                settled: false,
                closed: false,
                revision: 0,
                failure: None,
            }),
        })
    }

    fn publish(&self, event: &EngineEvent) {
        if let Some(kind) = rw_types::extension_events::ExtensionEventKind::from_event(event) {
            let generation = self
                .workers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !generation.paused {
                for worker in generation.workers.iter() {
                    worker.wake(kind);
                }
            }
        }
    }
}
fn error(error: impl std::fmt::Display) -> AgentLoopError {
    AgentLoopError::Persistence(error.to_string())
}
#[async_trait]
impl SessionEventSink for PluginFanoutEventSink {
    async fn completed_turn(
        &self,
        turn: u64,
    ) -> Result<Option<rw_core::CompletedTurn>, AgentLoopError> {
        self.inner.completed_turn(turn).await
    }

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
        self.workers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
        let mut failure = self.pause_and_settle().await.err();
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
