//! Cursor workers retire as a set before native generation publication.
use super::{PluginEventRegistration, PluginFanoutEventSink, PluginFanoutWorker, error};
use crate::session_runtime::durable_session::DurableEventSink;
use rw_core::{AgentLoopError, SessionEventSink};
use rw_providers::FixtureRedactor;
use std::sync::Arc;

pub(super) struct DeliveryGeneration {
    pub(super) workers: Arc<[PluginFanoutWorker]>,
    pub(super) paused: bool,
    pub(super) settled: bool,
    pub(super) closed: bool,
    pub(super) revision: u64,
    pub(super) failure: Option<String>,
}

pub(super) fn workers(
    inner: &Arc<DurableEventSink>,
    registrations: Vec<PluginEventRegistration>,
    redactor: &FixtureRedactor,
) -> Result<Vec<PluginFanoutWorker>, AgentLoopError> {
    let permits = registrations
        .iter()
        .map(|registration| registration.budget.worker())
        .collect::<Result<Vec<_>, _>>()
        .map_err(error)?;
    Ok(registrations
        .into_iter()
        .zip(permits)
        .map(|(registration, permit)| {
            let source: Arc<dyn SessionEventSink> = inner.clone();
            PluginFanoutWorker::new(source, registration, permit, redactor.clone())
        })
        .collect())
}

impl PluginFanoutEventSink {
    pub(crate) async fn pause_and_settle(&self) -> Result<(), AgentLoopError> {
        let (workers, revision) = {
            let mut generation = self
                .workers
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            generation.paused = true;
            // Cancel all owners before awaiting any proof. The retained set remains
            // reachable even when the caller waiting for proof is dropped.
            for worker in generation.workers.iter() {
                worker.cancel();
            }
            (generation.workers.clone(), generation.revision)
        };
        let mut failure = None;
        for result in
            futures_util::future::join_all(workers.iter().map(PluginFanoutWorker::settle)).await
        {
            if let Err(error) = result {
                failure.get_or_insert_with(|| error.to_string());
            }
        }
        let mut generation = self
            .workers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if generation.revision != revision {
            return failure.map_or(Ok(()), |failure| {
                Err(AgentLoopError::EffectsUnsettled(failure))
            });
        }
        if let Some(failure) = failure {
            generation.failure.get_or_insert(failure);
        }
        generation.settled = generation.failure.is_none();
        generation.failure.as_ref().map_or(Ok(()), |failure| {
            Err(AgentLoopError::EffectsUnsettled(failure.clone()))
        })
    }
    pub(crate) fn prepare(
        self: &Arc<Self>,
        registrations: Vec<PluginEventRegistration>,
    ) -> Result<PreparedPluginDelivery, AgentLoopError> {
        let generation = self
            .workers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !generation.paused
            || !generation.settled
            || generation.closed
            || generation.failure.is_some()
        {
            return Err(error("event delivery must retire before replacement"));
        }
        let revision = generation
            .revision
            .checked_add(1)
            .ok_or_else(|| error("event delivery revision exhausted"))?;
        let workers = workers(&self.inner, registrations, &self.redactor)?;
        Ok(PreparedPluginDelivery {
            owner: self.clone(),
            workers,
            revision,
        })
    }
}

/// Workers cannot consume a source event until their native generation is live.
pub(crate) struct PreparedPluginDelivery {
    owner: Arc<PluginFanoutEventSink>,
    workers: Vec<PluginFanoutWorker>,
    revision: u64,
}
impl PreparedPluginDelivery {
    pub(crate) fn publish(self) -> Result<(), AgentLoopError> {
        let mut generation = self
            .owner
            .workers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !generation.paused
            || !generation.settled
            || generation.closed
            || generation.failure.is_some()
            || generation.revision.checked_add(1) != Some(self.revision)
        {
            return Err(error("event delivery publication authority changed"));
        }
        generation.workers = Arc::from(self.workers);
        generation.revision = self.revision;
        generation.paused = false;
        generation.settled = false;
        for worker in generation.workers.iter() {
            worker.activate();
        }
        Ok(())
    }
}
