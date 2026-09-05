//! One owned delivery operation per subscription. Cancellation revokes admission;
//! completion is published only after the actual RPC owner proves settlement.
use super::{encoding, error};
use crate::extension_runtime::{PluginDeliveryBudget, PluginEventRegistration, PluginEventSources};
use async_trait::async_trait;
use futures_util::FutureExt;
use rw_core::{AgentLoopError, SessionEventSink, SessionReplayLimits};
use rw_ext::PluginRpcError;
use rw_providers::FixtureRedactor;
use rw_tools::CancellationToken;
use rw_types::{
    extension_contract::{
        ExtensionStateCommitOutcome, ExtensionStateSnapshot, ExtensionStateTransaction,
    },
    extension_events::{ExtensionEventKind, ExtensionEventNotice, ExtensionEventOutcome},
};
use std::{collections::BTreeSet, panic::AssertUnwindSafe, sync::Arc, time::Duration};
use tokio::sync::{Notify, OwnedSemaphorePermit, watch};

const SETTLEMENT_WAIT: Duration = Duration::from_secs(5);

#[async_trait]
pub(super) trait EventConsumer: Send + Sync {
    async fn snapshot(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<ExtensionStateSnapshot, AgentLoopError>;
    async fn commit(
        &self,
        transaction: ExtensionStateTransaction,
        cancellation: &CancellationToken,
    ) -> Result<ExtensionStateCommitOutcome, AgentLoopError>;
    async fn deliver(
        &self,
        notice: ExtensionEventNotice,
        cancellation: &CancellationToken,
    ) -> Result<ExtensionEventOutcome, PluginRpcError>;
    async fn settle(&self) -> Result<(), PluginRpcError>;
}
#[async_trait]
impl EventConsumer for PluginEventRegistration {
    async fn snapshot(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<ExtensionStateSnapshot, AgentLoopError> {
        self.handler
            .capability(cancellation)
            .await
            .map_err(error)?
            .read_state()
            .await
    }
    async fn commit(
        &self,
        transaction: ExtensionStateTransaction,
        cancellation: &CancellationToken,
    ) -> Result<ExtensionStateCommitOutcome, AgentLoopError> {
        self.handler
            .capability(cancellation)
            .await
            .map_err(error)?
            .commit_state(transaction)
            .await
    }
    async fn deliver(
        &self,
        notice: ExtensionEventNotice,
        cancellation: &CancellationToken,
    ) -> Result<ExtensionEventOutcome, PluginRpcError> {
        self.router.deliver(notice, cancellation).await
    }
    async fn settle(&self) -> Result<(), PluginRpcError> {
        self.router.settle_effects().await
    }
}

pub(super) struct PluginFanoutWorker {
    subscriptions: BTreeSet<ExtensionEventKind>,
    wake: Arc<Notify>,
    activation: Arc<Notify>,
    cancellation: CancellationToken,
    completed: watch::Receiver<Option<Result<(), String>>>,
}
impl PluginFanoutWorker {
    pub(super) fn new(
        source: Arc<dyn SessionEventSink>,
        registration: PluginEventRegistration,
        permit: OwnedSemaphorePermit,
        redactor: FixtureRedactor,
    ) -> Self {
        let subscriptions = registration.subscriptions.clone();
        let context = DeliveryContext {
            source,
            sources: registration.handler.event_sources.clone(),
            budget: registration.budget.clone(),
            consumer: Arc::new(registration.clone()),
            redactor,
        };
        Self::start(registration.plugin_id, subscriptions, context, permit)
    }
    fn start(
        plugin: String,
        subscriptions: BTreeSet<ExtensionEventKind>,
        context: DeliveryContext,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        let wake = Arc::new(Notify::new());
        let activation = Arc::new(Notify::new());
        let task_activation = activation.clone();
        let cancellation = CancellationToken::default();
        let (complete, completed) = watch::channel(None);
        let task_wake = wake.clone();
        let task_cancel = cancellation.clone();
        let task_subscriptions = subscriptions.clone();
        tokio::spawn(async move {
            let result =
                AssertUnwindSafe(async {
                    tokio::select! {
                        biased;
                        () = task_cancel.cancelled() => Ok(()),
                        () = task_activation.notified() => context.run(&task_subscriptions, &task_wake, &task_cancel).await,
                    }
                }).catch_unwind()
                    .await;
            let panicked = result.is_err();
            if let Ok(Err(_)) = &result {
                // Do not log plugin-controlled content or advance the cursor.
                tracing::warn!(plugin_id=%plugin,"plugin event delivery paused at its unacknowledged cursor");
            }
            let proof = AssertUnwindSafe(context.consumer.settle())
                .catch_unwind()
                .await;
            let settled = match proof {
                Ok(Ok(())) if !panicked => Ok(()),
                _ => Err("plugin event effect settlement remains unproven".to_owned()),
            };
            if settled.is_err() {
                // Bounded by the worker semaphore. Failed proof keeps the actual
                // source/endpoint and its charged capacity permanently quarantined.
                std::mem::forget((context, permit));
            }
            complete.send_replace(Some(settled));
        });
        Self {
            subscriptions,
            wake,
            activation,
            cancellation,
            completed,
        }
    }
    pub(super) fn activate(&self) {
        self.activation.notify_one();
    }
    pub(super) fn wake(&self, kind: ExtensionEventKind) {
        if self.subscriptions.contains(&kind) {
            self.wake.notify_one();
        }
    }
    pub(super) fn cancel(&self) {
        self.cancellation.cancel();
    }
    pub(super) async fn settle(&self) -> Result<(), AgentLoopError> {
        let mut completion = self.completed.clone();
        let wait = async {
            loop {
                if let Some(result) = completion.borrow_and_update().clone() {
                    return result.map_err(error);
                }
                completion
                    .changed()
                    .await
                    .map_err(|_| error("plugin event worker lost its settlement proof"))?;
            }
        };
        tokio::time::timeout(SETTLEMENT_WAIT, wait)
            .await
            .map_err(|_| {
                error("plugin event settlement deadline expired; owner remains retained")
            })?
    }
}
impl Drop for PluginFanoutWorker {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct DeliveryContext {
    source: Arc<dyn SessionEventSink>,
    sources: Arc<PluginEventSources>,
    budget: Arc<PluginDeliveryBudget>,
    consumer: Arc<dyn EventConsumer>,
    redactor: FixtureRedactor,
}
impl DeliveryContext {
    async fn run(
        &self,
        subscriptions: &BTreeSet<ExtensionEventKind>,
        wake: &Notify,
        cancellation: &CancellationToken,
    ) -> Result<(), AgentLoopError> {
        let snapshot = self.consumer.snapshot(cancellation).await?;
        let mut cursor = snapshot
            .acknowledged
            .or(snapshot.delivery_start)
            .map(|cursor| cursor.sequence);
        loop {
            if cancellation.is_cancelled() {
                return Ok(());
            }
            let preparation = self.budget.prepare(cancellation).await.map_err(error)?;
            let view = self.source.capture_read_view()?;
            if view
                .last_sequence()
                .is_none_or(|tail| cursor.is_some_and(|cursor| cursor >= tail))
            {
                drop(view);
                drop(preparation);
                tokio::select! {biased;()=cancellation.cancelled()=>return Ok(()),()=wake.notified()=>{}}
                continue;
            }
            // Never cancel-drop an admitted read: it can own blocking work.
            let mut events = view
                .read_page(
                    cursor,
                    SessionReplayLimits {
                        max_events: 1,
                        max_bytes: 16 * 1024 * 1024 + 1,
                    },
                )
                .await?;
            drop(view);
            if events.len() != 1 {
                return Err(error("plugin event reader did not make bounded progress"));
            }
            let event = events
                .pop()
                .ok_or_else(|| error("plugin event page missing"))?;
            let sequence = event
                .meta()
                .ok_or_else(|| error("transient event in plugin journal"))?
                .sequence_id;
            if cursor.is_some_and(|cursor| sequence <= cursor) {
                return Err(error("plugin event cursor regressed"));
            }
            let kind =
                ExtensionEventKind::from_event(&event).filter(|kind| subscriptions.contains(kind));
            let Some(kind) = kind else {
                cursor = Some(sequence);
                continue;
            };
            if cancellation.is_cancelled() {
                return Ok(());
            }
            let (source, content) = encoding::prepare(
                event,
                self.redactor.clone(),
                self.budget.clone(),
                cancellation,
            )
            .await?;
            drop(preparation);
            self.deliver(source, kind, content, cancellation).await?;
            cursor = Some(sequence);
            tokio::task::yield_now().await;
        }
    }
    async fn deliver(
        &self,
        source: Arc<crate::extension_runtime::PluginEventSource>,
        kind: ExtensionEventKind,
        content: rw_types::extension_events::ExtensionEventContent,
        cancellation: &CancellationToken,
    ) -> Result<(), AgentLoopError> {
        let snapshot = self.consumer.snapshot(cancellation).await?;
        let cursor = source.cursor.clone();
        let notice = ExtensionEventNotice {
            cursor: cursor.clone(),
            event: kind,
            state_revision: snapshot.revision,
            content,
        };
        let lease = self.sources.install(source).map_err(error)?;
        let outcome = self.consumer.deliver(notice, cancellation).await;
        // No further source requests can enter once the handler has settled.
        drop(lease);
        let outcome = outcome.map_err(error)?;
        if cancellation.is_cancelled() {
            return Ok(());
        }
        let transaction = ExtensionStateTransaction {
            expected_revision: snapshot.revision,
            mutations: outcome.mutations,
            acknowledged: Some(cursor),
        };
        // The accepted actor operation is awaited even if shutdown starts.
        match self.consumer.commit(transaction, cancellation).await? {
            ExtensionStateCommitOutcome::Committed { .. } => Ok(()),
            ExtensionStateCommitOutcome::Conflict { .. } => Err(error(
                "plugin event state revision changed before acknowledgement",
            )),
        }
    }
}

#[cfg(test)]
mod tests;
