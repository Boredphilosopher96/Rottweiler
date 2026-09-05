//! Correlated request lifetime and replaceable progress state.
use super::{PluginRpcError, rpc_error};
use rw_plugin_protocol::{OperationLifetime, ToolProgress, ToolProgressParams};
use rw_tools::{CancellationToken, ToolProgressSink};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

pub(super) enum RequestPolicy {
    Ordinary {
        allow_closed: bool,
    },
    Tool {
        lifetime: OperationLifetime,
        progress: Arc<dyn ToolProgressSink>,
    },
}

impl RequestPolicy {
    pub(super) fn allows_closed(&self) -> bool {
        matches!(self, Self::Ordinary { allow_closed: true })
    }

    pub(super) fn begin(
        self,
        response: oneshot::Sender<Result<Value, PluginRpcError>>,
        timeout: Duration,
    ) -> (PendingRequest, RequestObserver) {
        match self {
            Self::Ordinary { .. } => (
                PendingRequest::Ordinary(response),
                RequestObserver::Ordinary(timeout),
            ),
            Self::Tool { lifetime, progress } => {
                let now = Instant::now();
                let total = now + Duration::from_millis(u64::from(lifetime.total_ms()));
                let idle = now + Duration::from_millis(u64::from(lifetime.idle_ms()));
                let (updates, receiver) = watch::channel(None);
                (
                    PendingRequest::Tool {
                        response,
                        operation: ToolOperation {
                            total,
                            idle,
                            idle_duration: Duration::from_millis(u64::from(lifetime.idle_ms())),
                            last_sequence: 0,
                            rate: ProgressRate::new(now),
                            updates,
                        },
                    },
                    RequestObserver::Tool {
                        total,
                        idle,
                        progress,
                        updates: receiver,
                    },
                )
            }
        }
    }
}

pub(super) enum PendingRequest {
    Ordinary(oneshot::Sender<Result<Value, PluginRpcError>>),
    Tool {
        response: oneshot::Sender<Result<Value, PluginRpcError>>,
        operation: ToolOperation,
    },
}

impl PendingRequest {
    pub(super) fn respond(self, result: Result<Value, PluginRpcError>) {
        match self {
            Self::Ordinary(response) => {
                let _ = response.send(result);
            }
            Self::Tool {
                response,
                operation,
            } => {
                let now = Instant::now();
                let result = if now >= operation.total {
                    Err(rpc_error(
                        "timeout",
                        "plugin tool exceeded its total deadline",
                    ))
                } else if now >= operation.idle {
                    Err(rpc_error(
                        "timeout",
                        "plugin tool exceeded its idle deadline",
                    ))
                } else {
                    result
                };
                let _ = response.send(result);
                drop(operation);
            }
        }
    }

    pub(super) fn progress(&mut self, params: ToolProgressParams) -> bool {
        let Self::Tool { operation, .. } = self else {
            return false;
        };
        let now = Instant::now();
        if now >= operation.total
            || now >= operation.idle
            || params.sequence <= operation.last_sequence
            || !operation.rate.take(now)
        {
            return false;
        }
        operation.last_sequence = params.sequence;
        operation.idle = now + operation.idle_duration;
        operation
            .updates
            .send(Some(Observation {
                progress: params.progress,
                idle: operation.idle,
            }))
            .is_ok()
    }
}

pub(super) struct ToolOperation {
    total: Instant,
    idle: Instant,
    idle_duration: Duration,
    last_sequence: u32,
    rate: ProgressRate,
    updates: watch::Sender<Option<Observation>>,
}

#[derive(Clone)]
pub(super) struct Observation {
    progress: ToolProgress,
    idle: Instant,
}

struct ProgressRate {
    tokens: u32,
    replenished: Instant,
}
impl ProgressRate {
    fn new(now: Instant) -> Self {
        Self {
            tokens: rw_operation_contract::PROGRESS_BURST,
            replenished: now,
        }
    }
    fn take(&mut self, now: Instant) -> bool {
        let interval = u128::from(rw_operation_contract::PROGRESS_INTERVAL_MS);
        let elapsed = now.duration_since(self.replenished).as_millis() / interval;
        if elapsed > 0 {
            let replenished = u32::try_from(elapsed).unwrap_or(u32::MAX);
            self.tokens = self
                .tokens
                .saturating_add(replenished)
                .min(rw_operation_contract::PROGRESS_BURST);
            self.replenished += Duration::from_millis(
                u64::from(replenished) * u64::from(rw_operation_contract::PROGRESS_INTERVAL_MS),
            );
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

pub(super) enum RequestObserver {
    Ordinary(Duration),
    Tool {
        total: Instant,
        idle: Instant,
        progress: Arc<dyn ToolProgressSink>,
        updates: watch::Receiver<Option<Observation>>,
    },
}

impl RequestObserver {
    pub(super) async fn wait(
        self,
        mut response: oneshot::Receiver<Result<Value, PluginRpcError>>,
        cancellation: &CancellationToken,
    ) -> Result<Value, PluginRpcError> {
        match self {
            Self::Ordinary(timeout) => tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(rpc_error("cancelled", "plugin RPC request was cancelled")),
                result = tokio::time::timeout(timeout, response) => result
                    .map_err(|_| rpc_error("timeout", "plugin RPC request timed out"))?
                    .map_err(|_| rpc_error("connection_closed", "plugin RPC connection closed"))?,
            },
            Self::Tool {
                total,
                mut idle,
                progress,
                mut updates,
            } => loop {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(rpc_error("cancelled", "plugin tool was cancelled")),
                    () = tokio::time::sleep_until(total) => return Err(rpc_error("timeout", "plugin tool exceeded its total deadline")),
                    result = &mut response => return result.map_err(|_| rpc_error("connection_closed", "plugin RPC connection closed"))?,
                    changed = updates.changed() => {
                        if changed.is_err() { return Err(rpc_error("connection_closed", "plugin tool progress closed before its outcome")); }
                        if let Some(observation) = updates.borrow_and_update().clone() {
                            idle = observation.idle;
                            progress.report(observation.progress).map_err(|_| rpc_error("cancelled", "plugin tool progress admission closed"))?;
                        }
                    }
                    () = tokio::time::sleep_until(idle) => return Err(rpc_error("timeout", "plugin tool exceeded its idle deadline")),
                }
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rw_tools::ToolError;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Updates(Mutex<Vec<String>>);
    impl ToolProgressSink for Updates {
        fn report(&self, update: ToolProgress) -> Result<(), ToolError> {
            self.0.lock().unwrap().push(update.message().to_owned());
            Ok(())
        }
    }

    fn update(sequence: u32) -> ToolProgressParams {
        ToolProgressParams {
            request_id: rw_plugin_protocol::RpcId::Number(1),
            sequence,
            progress: ToolProgress::new(format!("work {sequence}"), None).unwrap(),
        }
    }

    #[tokio::test]
    async fn progress_renews_idle_but_never_the_total_deadline() {
        let (send, receive) = oneshot::channel();
        let sink = Arc::new(Updates::default());
        let (mut pending, observer) = RequestPolicy::Tool {
            lifetime: OperationLifetime::new(120, 75).unwrap(),
            progress: sink.clone(),
        }
        .begin(send, Duration::from_millis(1));
        let waiting =
            tokio::spawn(
                async move { observer.wait(receive, &CancellationToken::default()).await },
            );
        for sequence in 1..=2 {
            tokio::time::sleep(Duration::from_millis(45)).await;
            assert!(pending.progress(update(sequence)));
        }
        let result = tokio::time::timeout(Duration::from_millis(90), waiting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            result.unwrap_err().message,
            "plugin tool exceeded its total deadline"
        );
        assert_eq!(sink.0.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn outcome_is_published_before_progress_channel_closes() {
        for _ in 0..100 {
            let (send, receive) = oneshot::channel();
            let (pending, observer) = RequestPolicy::Tool {
                lifetime: OperationLifetime::default(),
                progress: Arc::new(Updates::default()),
            }
            .begin(send, Duration::from_millis(1));
            let waiting =
                tokio::spawn(
                    async move { observer.wait(receive, &CancellationToken::default()).await },
                );
            pending.respond(Ok(Value::Null));
            assert_eq!(waiting.await.unwrap(), Ok(Value::Null));
        }
    }

    #[tokio::test]
    async fn expired_idle_rejects_a_late_success_before_observer_polling() {
        let (send, receive) = oneshot::channel();
        let (mut pending, observer) = RequestPolicy::Tool {
            lifetime: OperationLifetime::default(),
            progress: Arc::new(Updates::default()),
        }
        .begin(send, Duration::from_secs(5));
        if let PendingRequest::Tool { operation, .. } = &mut pending {
            operation.idle = Instant::now();
        }
        pending.respond(Ok(Value::Null));
        let result = observer.wait(receive, &CancellationToken::default()).await;
        assert_eq!(
            result.unwrap_err().message,
            "plugin tool exceeded its idle deadline"
        );
    }

    #[tokio::test]
    async fn progress_rate_and_sequence_cannot_bypass_admission() {
        let (send, _receive) = oneshot::channel();
        let (mut pending, _observer) = RequestPolicy::Tool {
            lifetime: OperationLifetime::default(),
            progress: Arc::new(Updates::default()),
        }
        .begin(send, Duration::from_secs(5));
        assert!(!pending.progress(update(0)));
        assert!(pending.progress(update(1)));
        assert!(!pending.progress(update(1)));
        for sequence in 2..=4 {
            assert!(pending.progress(update(sequence)));
        }
        assert!(!pending.progress(update(5)));
        let (ordinary, _receive) = oneshot::channel();
        assert!(!PendingRequest::Ordinary(ordinary).progress(update(1)));
    }
}
