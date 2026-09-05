use super::durable_session::DurableEventSink;
use async_trait::async_trait;
use rw_core::AgentLoopError;
use rw_core::BudgetLedgerQuery;
use rw_core::BudgetLedgerTotals;
use rw_core::EngineEvent;
use rw_core::SequenceId;
use rw_core::SessionEventReadView;
use rw_core::SessionEventSink;
use rw_providers::FixtureRedactor;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;

pub(super) struct PluginFanoutEventSink {
    pub(super) inner: Arc<DurableEventSink>,
    pub(super) workers: Vec<PluginFanoutWorker>,
    pub(super) redactor: FixtureRedactor,
}

pub(super) const PLUGIN_EVENT_QUEUE_CAPACITY: usize = 64;

pub(super) const PLUGIN_EVENT_SUSTAINED_OVERFLOW: usize = 64;

pub(super) const PLUGIN_EVENT_DELIVERY_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(1);

pub(super) struct PluginFanoutMessage {
    pub(super) event: String,
    pub(super) payload: serde_json::Value,
}

#[async_trait]
pub(super) trait PluginEventPublisher: Send + Sync {
    async fn publish(
        &self,
        event: &str,
        payload: serde_json::Value,
    ) -> std::result::Result<(), rw_ext::PluginRpcError>;
}

#[async_trait]
impl PluginEventPublisher for rw_ext::PluginEventRouter {
    async fn publish(
        &self,
        event: &str,
        payload: serde_json::Value,
    ) -> std::result::Result<(), rw_ext::PluginRpcError> {
        rw_ext::PluginEventRouter::publish(self, event, payload).await
    }
}

pub(super) struct PluginFanoutWorker {
    pub(super) subscriptions: BTreeSet<String>,
    pub(super) sender: mpsc::Sender<PluginFanoutMessage>,
    pub(super) overflow: Arc<AtomicUsize>,
    pub(super) disabled: Arc<AtomicBool>,
    pub(super) task: tokio::task::JoinHandle<()>,
}

impl PluginFanoutWorker {
    pub(super) fn new(
        subscriptions: BTreeSet<String>,
        publisher: Arc<dyn PluginEventPublisher>,
    ) -> Self {
        let (sender, mut receiver) =
            mpsc::channel::<PluginFanoutMessage>(PLUGIN_EVENT_QUEUE_CAPACITY);
        let overflow = Arc::new(AtomicUsize::new(0));
        let disabled = Arc::new(AtomicBool::new(false));
        let worker_overflow = Arc::clone(&overflow);
        let worker_disabled = Arc::clone(&disabled);
        let task = tokio::spawn(async move {
            while let Some(message) = receiver.recv().await {
                if worker_disabled.load(Ordering::Acquire) {
                    break;
                }
                let delivered = tokio::time::timeout(
                    PLUGIN_EVENT_DELIVERY_TIMEOUT,
                    publisher.publish(&message.event, message.payload),
                )
                .await
                .is_ok_and(|result| result.is_ok());
                if delivered {
                    worker_overflow.store(0, Ordering::Release);
                } else {
                    let failures = worker_overflow
                        .fetch_add(1, Ordering::AcqRel)
                        .saturating_add(1);
                    if failures >= PLUGIN_EVENT_SUSTAINED_OVERFLOW
                        && !worker_disabled.swap(true, Ordering::AcqRel)
                    {
                        tracing::warn!(
                            delivery_failures = failures,
                            "plugin event fanout disabled after sustained delivery failure"
                        );
                        break;
                    }
                }
            }
        });
        Self {
            subscriptions,
            sender,
            overflow,
            disabled,
            task,
        }
    }

    pub(super) fn publish(&self, kind: &str, pascal: &str, payload: serde_json::Value) {
        if self.disabled.load(Ordering::Acquire) {
            return;
        }
        let Some(subscription) = self
            .subscriptions
            .iter()
            .find(|subscription| subscription.as_str() == kind || subscription.as_str() == pascal)
        else {
            return;
        };
        match self.sender.try_send(PluginFanoutMessage {
            event: subscription.clone(),
            payload,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                let overflow = self
                    .overflow
                    .fetch_add(1, Ordering::AcqRel)
                    .saturating_add(1);
                if overflow >= PLUGIN_EVENT_SUSTAINED_OVERFLOW
                    && !self.disabled.swap(true, Ordering::AcqRel)
                {
                    tracing::warn!(
                        dropped_events = overflow,
                        "plugin event fanout disabled after sustained backpressure"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.disabled.store(true, Ordering::Release);
            }
        }
    }
}

impl Drop for PluginFanoutWorker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl PluginFanoutEventSink {
    pub(super) fn new(
        inner: Arc<DurableEventSink>,
        routers: Vec<(BTreeSet<String>, Arc<rw_ext::PluginEventRouter>)>,
        redactor: FixtureRedactor,
    ) -> Self {
        let workers = routers
            .into_iter()
            .map(|(subscriptions, router)| {
                let publisher: Arc<dyn PluginEventPublisher> = router;
                PluginFanoutWorker::new(subscriptions, publisher)
            })
            .collect();
        Self {
            inner,
            workers,
            redactor,
        }
    }

    pub(super) fn publish(&self, event: &EngineEvent) {
        let Some((kind, pascal, payload)) = plugin_event_payload(&self.redactor, event) else {
            return;
        };
        for worker in &self.workers {
            worker.publish(&kind, &pascal, payload.clone());
        }
    }
}

pub(super) fn plugin_event_payload(
    redactor: &FixtureRedactor,
    event: &EngineEvent,
) -> Option<(String, String, serde_json::Value)> {
    let mut payload = serde_json::to_value(event).ok()?;
    redact_json_value(redactor, &mut payload);
    if !matches!(serde_json::to_vec(&payload), Ok(bytes) if bytes.len() <= 256 * 1024) {
        return None;
    }
    let kind = payload.get("type")?.as_str()?.to_owned();
    let pascal = kind
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_ascii_uppercase().to_string() + chars.as_str()
            })
        })
        .collect::<String>();
    Some((kind, pascal, payload))
}

#[async_trait]
impl SessionEventSink for PluginFanoutEventSink {
    async fn append(&self, event: EngineEvent) -> std::result::Result<EngineEvent, AgentLoopError> {
        let event = self.inner.append(event).await?;
        self.publish(&event);
        Ok(event)
    }
    async fn append_batch(
        &self,
        batch: Vec<EngineEvent>,
    ) -> std::result::Result<Vec<EngineEvent>, AgentLoopError> {
        let events = self.inner.append_batch(batch).await?;
        for event in &events {
            self.publish(event);
        }
        Ok(events)
    }
    fn capture_read_view(
        &self,
    ) -> std::result::Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        self.inner.capture_read_view()
    }

    async fn last_sequence(&self) -> std::result::Result<Option<SequenceId>, AgentLoopError> {
        self.inner.last_sequence().await
    }
    async fn budget_totals(
        &self,
        query: BudgetLedgerQuery,
    ) -> std::result::Result<BudgetLedgerTotals, AgentLoopError> {
        self.inner.budget_totals(query).await
    }
}

pub(super) fn redact_json_value(redactor: &FixtureRedactor, value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = redactor.redact_text(text),
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_value(redactor, value);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                redact_json_value(redactor, value);
            }
        }
        _ => {}
    }
}
