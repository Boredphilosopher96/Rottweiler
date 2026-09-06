//! Supervised JSON-RPC plugin runtime and public extension adapters.

mod push_reply;
pub use push_reply::{PushReply, PushReplyLimits, PushReplySlot};
mod boundary;
pub use boundary::*;
mod host;
pub use host::*;
mod adapters;
pub use adapters::*;
#[cfg(test)]
mod testing;
#[cfg(test)]
use testing::*;
mod incoming;
mod provider_http;
#[cfg(test)]
use incoming::stream_provider_http_body;
use incoming::{
    cancel_active_provider_http, drain_stderr, fail_pending, reader_loop, terminate_and_reap,
};

mod operation;
use operation::{PendingRequest, RequestPolicy};
mod writer;
use writer::{RpcReceiver, RpcWriter};

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use futures_util::{Stream, StreamExt as _};
#[cfg(test)]
use rw_plugin_protocol::encode_frame;
use rw_plugin_protocol::{
    CommandExecuteParams, DEFAULT_HANDLER_TIMEOUT_MS, ExtensionEventNotice, ExtensionEventOutcome,
    FrameDecoder, InitializeParams, MAX_FRAME_BYTES, MAX_HOOK_PAYLOAD_BYTES, MAX_NAME_BYTES,
    MAX_PLUGIN_MODEL_TOKENS, MAX_PLUGIN_PRICE_MICROS_USD, MAX_PROVIDER_STREAMS,
    MAX_RPC_MESSAGE_BYTES, METHOD_COMMAND_EXECUTE, METHOD_EVENT_PUBLISH, METHOD_EVENT_READ,
    METHOD_EXIT, METHOD_EXTENSION_STATE_COMMIT, METHOD_EXTENSION_STATE_READ, METHOD_INITIALIZE,
    METHOD_PROVIDER_COMPLETE, METHOD_PROVIDER_CREDIT, METHOD_PROVIDER_EVENT, METHOD_PROVIDER_HTTP,
    METHOD_PROVIDER_HTTP_CANCEL, METHOD_PROVIDER_HTTP_EVENT, METHOD_PROVIDER_MODELS,
    METHOD_SESSION_CONTEXT_READ, METHOD_SESSION_CONTROL, METHOD_SESSION_INJECT_MESSAGE,
    METHOD_SESSION_QUERY, METHOD_SESSION_SET_STATUS, METHOD_SHUTDOWN, METHOD_TOOL_CALL,
    METHOD_TOOL_PROGRESS, METHOD_UI_NOTIFY, PROVIDER_WINDOW_BYTES, PROVIDER_WINDOW_EVENTS,
    PluginCapabilities, PluginManifest, ProviderCacheBreakpoints, ProviderCompleteParams,
    ProviderEventParams, ProviderHttpCancelParams, ProviderHttpCapabilityParams,
    ProviderModelsParams, ProviderModelsResponse, RpcFailure, RpcFrame, RpcId, RpcNotification,
    RpcRequest, RpcSuccess, ToolCallParams, ToolProgressParams,
};
use rw_providers::{
    BoxEventStream, CacheBreakpointSupport, Capabilities, DiscoveredModel,
    DiscoveredProviderCatalog, ModelPricing, Provider, ProviderError, ProviderErrorKind,
    ProviderEvent, ProviderModelMetadata, ProviderRequest, UsageAccounting, WireMode,
};
use rw_tools::{
    CancellationToken, CapabilityManifest, MutationScope, Tool, ToolContext, ToolDescriptor,
    ToolError, ToolResult,
};
use rw_types::ToolCapability;
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
#[cfg(test)]
use tokio::io::BufReader;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, Semaphore, mpsc, oneshot, watch};

use crate::plugin::{
    ApprovalRequirement, ApprovalStore, CapabilityEnforcer, ExecutableIdentity,
    PluginApprovalError, PluginProcessConfig, PluginProcessError, PluginProviderEventStream,
    PluginRpcClient, PluginRpcError, SupervisedPluginProcess,
};
use crate::{CommandExecutionError, CommandHandler, CommandInvocation};

const RPC_REQUEST_CAPACITY: u16 = rw_plugin_protocol::MAX_IN_FLIGHT_REQUESTS;
const WRITER_QUEUE_CAPACITY: usize = RPC_REQUEST_CAPACITY as usize;
const PROVIDER_EVENT_QUEUE_CAPACITY: usize = PROVIDER_WINDOW_EVENTS;
const HOST_EFFECT_CAPACITY: u32 = RPC_REQUEST_CAPACITY as u32 * 2;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(DEFAULT_HANDLER_TIMEOUT_MS);
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

pub type PluginStdin = Pin<Box<dyn AsyncWrite + Send + Sync + Unpin + 'static>>;
pub type PluginStdout = Pin<Box<dyn AsyncBufRead + Send + Sync + Unpin + 'static>>;

type Pending = Arc<Mutex<BTreeMap<RpcId, PendingRequest>>>;

struct PendingProviderStream {
    alias: String,
    deadline: tokio::time::Instant,
    sender: mpsc::Sender<(Value, usize)>,
    terminal: watch::Sender<Option<Result<Value, PluginRpcError>>>,
    finished: Option<Value>,
    remaining_credit: (usize, usize),
    queued_bytes: Arc<AtomicUsize>,
    credit: Arc<ReturnedCredit>,
}

#[derive(Default)]
struct ReturnedCredit {
    available: StdMutex<(usize, usize)>,
    wake: tokio::sync::Notify,
    closed: CancellationToken,
}

async fn return_stream_credit(
    id: RpcId,
    credit: Arc<ReturnedCredit>,
    writer: RpcWriter,
    termination: Arc<RequestTermination>,
    streams: PendingProviderStreams,
    deadline: tokio::time::Instant,
) {
    loop {
        tokio::select! {
            biased;
            () = termination.cancellation.cancelled() => return,
            () = credit.closed.cancelled() => return,
            () = tokio::time::sleep_until(deadline) => {
                termination.begin();
                let failure = termination.wait().await.err().unwrap_or_else(|| rpc_error("timeout", "provider operation exceeded its total deadline"));
                if let Ok(mut streams) = streams.lock()
                    && let Some(stream) = streams.remove(&id) {
                        let _ = stream.terminal.send(Some(Err(failure)));
                }
                return;
            }
            () = credit.wake.notified() => {}
        }
        let returned = std::mem::take(
            &mut *credit
                .available
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if returned.0 == 0 {
            continue;
        }
        {
            let mut streams = streams
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(stream) = streams.get_mut(&id) else {
                return;
            };
            stream.remaining_credit.0 += returned.0;
            stream.remaining_credit.1 += returned.1;
        }
        let frame = RpcFrame::Notification(RpcNotification {
            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
            method: METHOD_PROVIDER_CREDIT.to_owned(),
            params: Some(json!({"request_id":id, "events":returned.0, "bytes":returned.1})),
        });
        tokio::select! {
            biased;
            () = termination.cancellation.cancelled() => return,
            () = credit.closed.cancelled() => return,
            () = tokio::time::sleep_until(deadline) => { termination.begin(); return; }
            result = writer.send(frame) => if result.is_err() { termination.begin(); return; }
        }
    }
}

type PendingProviderStreams = Arc<StdMutex<BTreeMap<RpcId, PendingProviderStream>>>;
type ActiveProviderHttp = Arc<StdMutex<BTreeMap<RpcId, provider_http::ActiveHttp>>>;

type SettlementResult = Option<Result<(), PluginProcessError>>;

struct RequestTermination {
    process: Arc<dyn SupervisedPluginProcess>,
    closed: Arc<AtomicBool>,
    in_flight: Arc<Semaphore>,
    active_provider_http: ActiveProviderHttp,
    cancellation: CancellationToken,
    host_effects: Arc<Semaphore>,
    host_failure: watch::Sender<bool>,
    completion: StdMutex<Option<watch::Receiver<SettlementResult>>>,
}

impl RequestTermination {
    fn fail_host_proof(&self) {
        self.host_failure.send_replace(true);
        self.begin();
    }

    fn begin(&self) {
        let mut completion = self
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if completion.is_some() {
            return;
        }
        self.cancellation.cancel();
        self.closed.store(true, Ordering::Release);
        self.in_flight.close();
        cancel_active_provider_http(&self.active_provider_http);
        if let Err(error) = self.process.kill_tree() {
            tracing::warn!(%error, "initial plugin kill failed; supervisor must prove effect settlement");
        }
        let process = Arc::clone(&self.process);
        let host_effects = Arc::clone(&self.host_effects);
        let mut host_failure = self.host_failure.subscribe();
        let (sender, receiver) = watch::channel(None);
        *completion = Some(receiver);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::error!("plugin was killed without an async runtime available for reaping");
            return;
        };
        runtime.spawn(async move {
            let result = process.settle_effects().await;
            let host_proof = async {
                loop {
                    if *host_failure.borrow_and_update() { return false; }
                    tokio::select! {
                        permit = host_effects.acquire_many(HOST_EFFECT_CAPACITY) => return permit.is_ok(),
                        changed = host_failure.changed() => if changed.is_err() { return false; },
                    }
                }
            }.await;
            let result = if host_proof { result } else {
                Err(PluginProcessError { message: "host effect settlement remains unproven".to_owned() })
            };
            if let Err(error) = &result {
                tracing::error!(%error, "plugin effects could not be settled; operation remains blocked");
            }
            let _ = sender.send(Some(result));
        });
    }

    async fn wait(&self) -> Result<(), PluginRpcError> {
        let completion = self
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(mut completion) = completion else {
            return Ok(());
        };
        loop {
            if let Some(result) = completion.borrow_and_update().clone() {
                return result.map_err(|error| PluginRpcError {
                    code: "effects_unsettled".to_owned(),
                    message: error.to_string(),
                });
            }
            if completion.changed().await.is_err() {
                return Err(PluginRpcError {
                    code: "effects_unsettled".to_owned(),
                    message: "plugin cleanup owner exited without effect settlement proof"
                        .to_owned(),
                });
            }
        }
    }
}

struct OrdinaryRequestGuard<'a> {
    termination: &'a RequestTermination,
    armed: bool,
}

impl Drop for OrdinaryRequestGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.termination.begin();
        }
    }
}

struct ReaderState {
    writer: RpcWriter,
    pending: Pending,
    provider_streams: PendingProviderStreams,
    provider_http: Arc<dyn PluginProviderHttpHandler>,
    active_provider_http: ActiveProviderHttp,
    enforcer: Arc<CapabilityEnforcer>,
    push_handler: Arc<dyn PushHandler>,
    host_commands: Arc<StdMutex<BTreeSet<RpcId>>>,
    redactor: Arc<dyn PluginBoundaryRedactor>,
    process: Arc<dyn SupervisedPluginProcess>,
    termination: Arc<RequestTermination>,
}

/// Bounded, correlated, single-reader JSON-RPC client.
pub struct JsonRpcPluginClient {
    writer: RpcWriter,
    pending: Pending,
    provider_streams: PendingProviderStreams,
    in_flight: Arc<Semaphore>,
    provider_slots: Arc<Semaphore>,
    next_id: AtomicU64,
    timeout: Duration,
    process: Arc<dyn SupervisedPluginProcess>,
    closed: Arc<AtomicBool>,
    termination: Arc<RequestTermination>,
    shutdown_complete: AtomicBool,
    shutdown_lock: Mutex<()>,
    redactor: Arc<dyn PluginBoundaryRedactor>,
}

impl Drop for JsonRpcPluginClient {
    fn drop(&mut self) {
        if self.shutdown_complete.load(Ordering::Acquire) {
            return;
        }
        self.termination.begin();
    }
}

impl JsonRpcPluginClient {
    pub fn start(
        launched: LaunchedPluginProcess,
        enforcer: Arc<CapabilityEnforcer>,
        push_handler: Arc<dyn PushHandler>,
        provider_http: Arc<dyn PluginProviderHttpHandler>,
        redactor: Arc<dyn PluginBoundaryRedactor>,
        timeout: Duration,
    ) -> Arc<Self> {
        let (writer, receiver) = RpcWriter::channel();
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let provider_streams = Arc::new(StdMutex::new(BTreeMap::new()));
        let active_provider_http = Arc::new(StdMutex::new(BTreeMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let in_flight = Arc::new(Semaphore::new(WRITER_QUEUE_CAPACITY));
        let termination = Arc::new(RequestTermination {
            process: Arc::clone(&launched.process),
            closed: Arc::clone(&closed),
            in_flight: Arc::clone(&in_flight),
            active_provider_http: Arc::clone(&active_provider_http),
            cancellation: CancellationToken::default(),
            host_effects: Arc::new(Semaphore::new(HOST_EFFECT_CAPACITY as usize)),
            host_failure: watch::channel(false).0,
            completion: StdMutex::new(None),
        });
        let client = Arc::new(Self {
            writer,
            pending: Arc::clone(&pending),
            provider_streams: Arc::clone(&provider_streams),
            in_flight,
            provider_slots: Arc::new(Semaphore::new(MAX_PROVIDER_STREAMS)),
            next_id: AtomicU64::new(1),
            timeout: if timeout.is_zero() {
                DEFAULT_REQUEST_TIMEOUT
            } else {
                timeout
            },
            process: Arc::clone(&launched.process),
            closed,
            termination: Arc::clone(&termination),
            shutdown_complete: AtomicBool::new(false),
            shutdown_lock: Mutex::new(()),
            redactor: Arc::clone(&redactor),
        });
        tokio::spawn(writer_loop(
            launched.stdin,
            receiver,
            Arc::clone(&pending),
            Arc::clone(&termination),
        ));
        tokio::spawn(reader_loop(
            launched.stdout,
            ReaderState {
                writer: client.writer.clone(),
                pending,
                provider_streams,
                provider_http,
                active_provider_http,
                enforcer,
                push_handler,
                host_commands: Arc::new(StdMutex::new(BTreeSet::new())),
                redactor,
                process: Arc::clone(&launched.process),
                termination,
            },
        ));
        tokio::spawn(drain_stderr(launched.stderr));
        client
    }

    /// Sends one correlated request with bounded queueing, response time, and cancellation.
    ///
    /// # Errors
    ///
    /// Returns a sanitized RPC error on cancellation, timeout, backpressure, protocol failure,
    /// process exit, or a plugin-reported failure. Cancellation and timeout wait
    /// for effect settlement; an unprovable cleanup remains pending.
    pub async fn request_cancellable(
        &self,
        method: &str,
        params: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, PluginRpcError> {
        if method == METHOD_TOOL_CALL {
            return Err(rpc_error(
                "invalid_method",
                "tool calls require typed operation admission",
            ));
        }
        self.request_cancellable_inner(
            method,
            params,
            cancellation,
            RequestPolicy::Ordinary {
                allow_closed: false,
            },
        )
        .await
    }

    async fn request_cancellable_inner(
        &self,
        method: &str,
        params: Value,
        cancellation: &CancellationToken,
        policy: RequestPolicy,
    ) -> Result<Value, PluginRpcError> {
        let allow_closed = policy.allows_closed();
        if self.closed.load(Ordering::Acquire) && !allow_closed {
            return Err(rpc_error("closed", "plugin RPC client is closed"));
        }
        let permit = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(rpc_error("cancelled", "plugin RPC request was cancelled")),
            permit = tokio::time::timeout(self.timeout, self.in_flight.acquire()) => {
                permit.map_err(|_| rpc_error("backpressure_timeout", "plugin RPC request limit remained saturated"))?
                    .map_err(|_| rpc_error("closed", "plugin RPC client is closed"))?
            }
        };
        if self.closed.load(Ordering::Acquire) && !allow_closed {
            return Err(rpc_error("closed", "plugin RPC client is closed"));
        }
        let numeric = self
            .next_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| {
                (id <= 9_007_199_254_740_991).then(|| id + 1)
            })
            .map_err(|_| rpc_error("id_exhausted", "plugin RPC request IDs exhausted"))?;
        let id = RpcId::Number(
            i64::try_from(numeric)
                .map_err(|_| rpc_error("id_exhausted", "plugin RPC request IDs exhausted"))?,
        );
        let (sender, receiver) = oneshot::channel();
        let (mut pending, observer) = policy.begin(sender, self.timeout);
        pending.bind_authority(method, &params)?;
        self.pending.lock().await.insert(id.clone(), pending);
        let mut guard = OrdinaryRequestGuard {
            termination: &self.termination,
            armed: true,
        };
        let frame = RpcFrame::Request(RpcRequest {
            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
            id: id.clone(),
            method: method.to_owned(),
            params: Some(self.redactor.redact(params)),
        });
        let sent = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                guard.armed = false;
                self.pending.lock().await.remove(&id);
                return Err(rpc_error("cancelled", "plugin RPC request was cancelled"));
            }
            sent = tokio::time::timeout(self.timeout, self.writer.send(frame)) => sent,
        };
        match sent {
            Ok(Ok(())) => {}
            Ok(Err(())) => {
                guard.armed = false;
                self.pending.lock().await.remove(&id);
                return Err(rpc_error(
                    "connection_closed",
                    "plugin RPC connection closed",
                ));
            }
            Err(_) => {
                guard.armed = false;
                self.pending.lock().await.remove(&id);
                return Err(rpc_error(
                    "backpressure_timeout",
                    "plugin RPC writer remained saturated",
                ));
            }
        }
        let result = observer.wait(receiver, cancellation).await;
        if result.as_ref().is_err_and(|error| {
            matches!(
                error.code.as_str(),
                "timeout" | "cancelled" | "-32004" | "-32800"
            )
        }) {
            self.termination.begin();
        }
        self.termination.wait().await?;
        self.pending.lock().await.remove(&id);
        guard.armed = false;
        drop(permit);
        result
    }

    /// Requests graceful shutdown, then kills and reaps after the deadline.
    ///
    /// # Errors
    ///
    /// Returns a process error if termination or reaping cannot be completed.
    pub async fn shutdown(&self, timeout: Duration) -> Result<(), PluginHostError> {
        let _shutdown = self.shutdown_lock.lock().await;
        if self.shutdown_complete.load(Ordering::Acquire) {
            return Ok(());
        }
        self.closed.store(true, Ordering::Release);
        let timeout = if timeout.is_zero() {
            DEFAULT_SHUTDOWN_TIMEOUT
        } else {
            timeout
        };
        let cancellation = CancellationToken::default();
        let graceful = tokio::time::timeout(timeout, async {
            self.request_internal(METHOD_SHUTDOWN, json!({}), &cancellation)
                .await?;
            self.send_notification(METHOD_EXIT, json!({})).await
        })
        .await;
        if graceful.is_ok_and(|result| result.is_ok()) {
            let _ = tokio::time::timeout(timeout, self.process.wait()).await;
        }
        self.termination.begin();
        let result = match tokio::time::timeout(timeout, self.termination.wait()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(PluginHostError::EffectsUnsettled {
                message: error.to_string(),
            }),
            Err(_) => Err(PluginHostError::EffectsUnsettled {
                message: "plugin effect settlement remains unproven after shutdown deadline"
                    .to_owned(),
            }),
        };
        if result.is_ok() {
            self.shutdown_complete.store(true, Ordering::Release);
        }
        result
    }

    async fn request_internal(
        &self,
        method: &str,
        params: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, PluginRpcError> {
        self.request_cancellable_inner(
            method,
            params,
            cancellation,
            RequestPolicy::Ordinary { allow_closed: true },
        )
        .await
    }

    async fn send_notification(&self, method: &str, params: Value) -> Result<(), PluginRpcError> {
        if self.termination.cancellation.is_cancelled() {
            return Err(rpc_error("closed", "plugin RPC client is closed"));
        }
        let frame = RpcFrame::Notification(RpcNotification {
            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
            method: method.to_owned(),
            params: Some(self.redactor.redact(params)),
        });
        tokio::time::timeout(self.timeout, self.writer.send(frame))
            .await
            .map_err(|_| {
                rpc_error(
                    "backpressure_timeout",
                    "plugin RPC writer remained saturated",
                )
            })?
            .map_err(|()| rpc_error("connection_closed", "plugin RPC connection closed"))
    }

    #[allow(clippy::too_many_lines)]
    async fn start_provider_stream(
        &self,
        params: Value,
    ) -> Result<PluginProviderEventStream, PluginRpcError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(rpc_error("closed", "plugin RPC client is closed"));
        }
        let permit = tokio::time::timeout(
            self.timeout,
            Arc::clone(&self.provider_slots).acquire_owned(),
        )
        .await
        .map_err(|_| {
            rpc_error(
                "backpressure_timeout",
                "plugin provider stream limit remained saturated",
            )
        })?
        .map_err(|_| rpc_error("closed", "plugin RPC client is closed"))?;
        if self.closed.load(Ordering::Acquire) {
            return Err(rpc_error("closed", "plugin RPC client is closed"));
        }
        let alias = params
            .get("alias")
            .and_then(Value::as_str)
            .filter(|alias| !alias.is_empty() && alias.len() <= MAX_NAME_BYTES)
            .ok_or_else(|| rpc_error("invalid_params", "invalid provider invocation alias"))?
            .to_owned();
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(rw_plugin_protocol::MAX_OPERATION_DURATION_MS);
        let numeric = self
            .next_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| {
                (id <= 9_007_199_254_740_991).then(|| id + 1)
            })
            .map_err(|_| rpc_error("id_exhausted", "plugin RPC request IDs exhausted"))?;
        let id = RpcId::Number(
            i64::try_from(numeric)
                .map_err(|_| rpc_error("id_exhausted", "plugin RPC request IDs exhausted"))?,
        );
        let (sender, receiver) = mpsc::channel(PROVIDER_EVENT_QUEUE_CAPACITY);
        let (terminal, terminal_receiver) = watch::channel(None);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let credit = Arc::new(ReturnedCredit::default());
        self.provider_streams
            .lock()
            .map_err(|_| {
                rpc_error(
                    "stream_state",
                    "plugin provider stream state is unavailable",
                )
            })?
            .insert(
                id.clone(),
                PendingProviderStream {
                    alias,
                    deadline,
                    sender,
                    terminal,
                    finished: None,
                    remaining_credit: (PROVIDER_WINDOW_EVENTS, PROVIDER_WINDOW_BYTES),
                    queued_bytes: Arc::clone(&queued_bytes),
                    credit: Arc::clone(&credit),
                },
            );
        let mut guard = OrdinaryRequestGuard {
            termination: &self.termination,
            armed: true,
        };
        let frame = RpcFrame::Request(RpcRequest {
            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
            id: id.clone(),
            method: METHOD_PROVIDER_COMPLETE.to_owned(),
            params: Some(self.redactor.redact(params)),
        });
        let sent = tokio::time::timeout(self.timeout, self.writer.send(frame)).await;
        if !matches!(sent, Ok(Ok(()))) {
            guard.armed = false;
            if let Ok(mut streams) = self.provider_streams.lock() {
                streams.remove(&id);
            }
            return Err(match sent {
                Err(_) => rpc_error(
                    "backpressure_timeout",
                    "plugin RPC writer remained saturated",
                ),
                Ok(Err(()) | Ok(())) => {
                    rpc_error("connection_closed", "plugin RPC connection closed")
                }
            });
        }
        self.send_notification(
            METHOD_PROVIDER_CREDIT,
            json!({
                "request_id": id, "events": PROVIDER_WINDOW_EVENTS, "bytes": PROVIDER_WINDOW_BYTES,
            }),
        )
        .await?;
        tokio::spawn(return_stream_credit(
            id.clone(),
            Arc::clone(&credit),
            self.writer.clone(),
            Arc::clone(&self.termination),
            Arc::clone(&self.provider_streams),
            deadline,
        ));
        guard.armed = false;
        Ok(Box::pin(JsonRpcProviderEventStream {
            receiver,
            id: Some(id),
            terminal: terminal_receiver,
            queued_bytes,
            credit,
            termination: Arc::clone(&self.termination),
            provider_streams: Arc::clone(&self.provider_streams),
            _permit: permit,
        }))
    }
}

struct JsonRpcProviderEventStream {
    receiver: mpsc::Receiver<(Value, usize)>,
    credit: Arc<ReturnedCredit>,
    terminal: watch::Receiver<Option<Result<Value, PluginRpcError>>>,
    queued_bytes: Arc<AtomicUsize>,
    termination: Arc<RequestTermination>,
    id: Option<RpcId>,
    provider_streams: PendingProviderStreams,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Stream for JsonRpcProviderEventStream {
    type Item = Result<Value, PluginRpcError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.id.is_none() {
            return Poll::Ready(None);
        }
        let terminal = self.terminal.borrow().clone();
        if let Some(Err(error)) = terminal {
            self.id = None;
            return Poll::Ready(Some(Err(error)));
        }
        match self.receiver.poll_recv(context) {
            Poll::Ready(Some((event, bytes))) => {
                self.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
                {
                    let mut returned = self
                        .credit
                        .available
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    returned.0 += 1;
                    returned.1 += bytes;
                }
                self.credit.wake.notify_one();
                Poll::Ready(Some(Ok(event)))
            }
            Poll::Ready(None) => {
                if self.terminal.borrow().is_none() {
                    self.termination.begin();
                }
                self.id = None;
                // The finished event is released only after the correlated RPC
                // success. A consumer stopping at Finished cannot race the reply.
                Poll::Ready(Some(self.terminal.borrow().clone().unwrap_or_else(|| {
                    Err(rpc_error(
                        "connection_closed",
                        "provider stream lost its terminal outcome",
                    ))
                })))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for JsonRpcProviderEventStream {
    fn drop(&mut self) {
        self.credit.closed.cancel();
        let Some(id) = self.id.take() else {
            return;
        };
        let unfinished = self
            .provider_streams
            .lock()
            .map_or(true, |mut streams| streams.remove(&id).is_some());
        if unfinished {
            self.termination.begin();
        }
    }
}

#[async_trait]
impl PluginRpcClient for JsonRpcPluginClient {
    async fn call_command(
        &self,
        params: CommandExecuteParams,
        cancellation: &CancellationToken,
    ) -> Result<Value, PluginRpcError> {
        let lifetime = params.lifetime;
        let params = serde_json::to_value(params)
            .map_err(|_| rpc_error("invalid_params", "invalid command operation"))?;
        self.request_cancellable_inner(
            METHOD_COMMAND_EXECUTE,
            params,
            cancellation,
            RequestPolicy::Command { lifetime },
        )
        .await
    }

    async fn settle_effects(&self) -> std::result::Result<(), crate::PluginRpcError> {
        self.termination.wait().await
    }
    async fn request(&self, method: &str, params: Value) -> Result<Value, PluginRpcError> {
        self.request_cancellable(method, params, &CancellationToken::default())
            .await
    }

    async fn request_cancellable(
        &self,
        method: &str,
        params: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, PluginRpcError> {
        JsonRpcPluginClient::request_cancellable(self, method, params, cancellation).await
    }

    async fn call_tool(
        &self,
        params: ToolCallParams,
        cancellation: &CancellationToken,
        progress: Arc<dyn rw_tools::ToolProgressSink>,
        effects: Option<Arc<crate::PluginToolEffects>>,
    ) -> Result<Value, PluginRpcError> {
        let lifetime = params.lifetime;
        let params = serde_json::to_value(params)
            .map_err(|_| rpc_error("invalid_params", "invalid plugin tool call"))?;
        self.request_cancellable_inner(
            METHOD_TOOL_CALL,
            params,
            cancellation,
            RequestPolicy::Tool {
                lifetime,
                progress,
                effects,
            },
        )
        .await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), PluginRpcError> {
        self.send_notification(method, params).await
    }

    async fn provider_stream(
        &self,
        params: Value,
    ) -> Result<PluginProviderEventStream, PluginRpcError> {
        self.start_provider_stream(params).await
    }
}

async fn writer_loop(
    mut stdin: PluginStdin,
    mut receiver: RpcReceiver,
    pending: Pending,
    termination: Arc<RequestTermination>,
) {
    loop {
        let frame = tokio::select! {
            biased;
            () = termination.cancellation.cancelled() => return,
            frame = receiver.recv() => frame,
        };
        let Some(frame) = frame else {
            break;
        };
        let written = tokio::select! {
            biased;
            () = termination.cancellation.cancelled() => return,
            written = tokio::time::timeout(DEFAULT_REQUEST_TIMEOUT, async {
                stdin.write_all(&frame.bytes).await?;
                stdin.flush().await
            }) => written,
        };
        if !written.is_ok_and(|result| result.is_ok()) {
            termination.begin();
            let failure =
                termination.wait().await.err().unwrap_or_else(|| {
                    rpc_error("write_failed", "plugin RPC stdin failed or stalled")
                });
            fail_pending(&pending, failure).await;
            return;
        }
        frame.complete();
    }
    let _ = stdin.shutdown().await;
}

fn rpc_error(code: &str, message: &str) -> PluginRpcError {
    PluginRpcError {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests;
