//! Supervised JSON-RPC plugin runtime and public extension adapters.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use futures_util::{Stream, StreamExt as _};
use rw_plugin_protocol::{
    CommandExecuteParams, DEFAULT_HANDLER_TIMEOUT_MS, EventPublishParams, FrameDecoder,
    InitializeParams, MAX_FRAME_BYTES, MAX_HOOK_PAYLOAD_BYTES, MAX_NAME_BYTES,
    MAX_PLUGIN_MODEL_TOKENS, MAX_PLUGIN_PRICE_MICROS_USD, MAX_RPC_MESSAGE_BYTES,
    METHOD_COMMAND_EXECUTE, METHOD_EVENT_PUBLISH, METHOD_EXIT, METHOD_INITIALIZE,
    METHOD_PROVIDER_CANCEL, METHOD_PROVIDER_COMPLETE, METHOD_PROVIDER_EVENT, METHOD_PROVIDER_HTTP,
    METHOD_PROVIDER_HTTP_CANCEL, METHOD_PROVIDER_HTTP_EVENT, METHOD_PROVIDER_MODELS,
    METHOD_SESSION_INJECT_MESSAGE, METHOD_SESSION_SET_STATUS, METHOD_SHUTDOWN, METHOD_TOOL_CALL,
    METHOD_UI_NOTIFY, PluginCapabilities, PluginManifest, ProviderCacheBreakpoints,
    ProviderCancelParams, ProviderCompleteParams, ProviderEventParams,
    ProviderHttpCapabilityParams, ProviderModelsParams, ProviderModelsResponse, RpcFailure,
    RpcFrame, RpcId, RpcNotification, RpcRequest, RpcSuccess, ToolCallParams, encode_frame,
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
use tokio::sync::{Mutex, Semaphore, mpsc, oneshot};

use crate::plugin::{
    ApprovalRequirement, ApprovalStore, CapabilityEnforcer, ExecutableIdentity,
    PluginApprovalError, PluginProcessConfig, PluginProcessError, PluginProviderEventStream,
    PluginRpcClient, PluginRpcError, SupervisedPluginProcess,
};
use crate::{CommandExecutionError, CommandHandler, CommandInvocation};

const WRITER_QUEUE_CAPACITY: usize = 64;
const PROVIDER_EVENT_QUEUE_CAPACITY: usize = 64;
const MAX_ABANDONED_REQUESTS: usize = 256;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(DEFAULT_HANDLER_TIMEOUT_MS);
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

pub type PluginStdin = Pin<Box<dyn AsyncWrite + Send + Sync + Unpin + 'static>>;
pub type PluginStdout = Pin<Box<dyn AsyncBufRead + Send + Sync + Unpin + 'static>>;

/// Immutable sandbox input. Launchers translate this into their platform profile.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginSandboxProfile {
    pub mode: PluginSandboxMode,
    pub capabilities: PluginCapabilities,
    pub approved_roots: Vec<PathBuf>,
    pub allowed_domains: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginSandboxMode {
    /// Initialization-only launch: deny network/writes and expose only runtime/entrypoint reads.
    #[cfg(test)]
    ManifestProbe,
    /// Release-owned source graph discovery and sealed-bundle preparation.
    Preparation,
    Approved,
}

impl PluginSandboxProfile {
    #[must_use]
    pub fn allows_workspace_reads(&self) -> bool {
        self.capabilities.tools.iter().any(|tool| {
            tool.caps
                .contains(&rw_plugin_protocol::PluginToolEffect::ReadsFilesystem)
        })
    }

    #[must_use]
    pub fn allows_workspace_writes(&self) -> bool {
        self.capabilities.tools.iter().any(|tool| {
            tool.caps
                .contains(&rw_plugin_protocol::PluginToolEffect::WritesFilesystem)
        })
    }

    #[must_use]
    pub fn requests_network(&self) -> bool {
        !self.capabilities.providers.is_empty()
            || self.capabilities.tools.iter().any(|tool| {
                tool.caps
                    .contains(&rw_plugin_protocol::PluginToolEffect::Network)
            })
    }

    #[must_use]
    pub fn allows_network(&self) -> bool {
        self.requests_network() && !self.allowed_domains.is_empty()
    }
}

/// A launched child with exclusive stdio ownership and a supervised process handle.
pub struct LaunchedPluginProcess {
    pub stdin: PluginStdin,
    pub stdout: PluginStdout,
    pub stderr: PluginStdout,
    pub process: Arc<dyn SupervisedPluginProcess>,
    /// Identity re-attested by the launcher at its final pre-spawn boundary.
    pub executable_identity: ExecutableIdentity,
}

/// Mandatory host boundary for removing known secrets before any value reaches a plugin.
pub trait PluginBoundaryRedactor: Send + Sync {
    fn redact(&self, value: Value) -> Value;

    /// Redacts known credential bytes before an HTTP response chunk is encoded
    /// onto the plugin wire.
    fn redact_bytes(&self, value: &[u8]) -> Vec<u8> {
        value.to_vec()
    }

    /// Redacts the safely-emittable prefix while returning the original tail
    /// needed to detect a credential completed by the next transport chunk.
    fn redact_streaming_prefix(&self, value: &[u8], retain: usize) -> (Vec<u8>, Vec<u8>) {
        if retain == 0 {
            (self.redact_bytes(value), Vec::new())
        } else {
            (Vec::new(), value.to_vec())
        }
    }

    /// Longest registered credential, used to retain an exact cross-chunk overlap.
    fn maximum_secret_bytes(&self) -> usize {
        0
    }
}

/// Test-only identity boundary. Production composition must inject the shared redactor.
#[cfg(test)]
pub(crate) struct NoopPluginBoundaryRedactor;

#[cfg(test)]
impl PluginBoundaryRedactor for NoopPluginBoundaryRedactor {
    fn redact(&self, value: Value) -> Value {
        value
    }

    fn redact_bytes(&self, value: &[u8]) -> Vec<u8> {
        value.to_vec()
    }

    fn maximum_secret_bytes(&self) -> usize {
        0
    }
}

pub type PluginHttpByteStream =
    Pin<Box<dyn Stream<Item = Result<Vec<u8>, PluginRpcError>> + Send + 'static>>;

/// Host-owned response to one authenticated plugin-provider HTTP request.
pub struct PluginHttpStreamResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: PluginHttpByteStream,
}

/// Trusted host boundary that resolves credentials and owns the provider socket.
#[async_trait]
pub trait PluginProviderHttpHandler: Send + Sync {
    async fn request(
        &self,
        params: Value,
        cancellation: &CancellationToken,
    ) -> Result<PluginHttpStreamResponse, PluginRpcError>;
}

pub struct DenyPluginProviderHttpHandler;

#[async_trait]
impl PluginProviderHttpHandler for DenyPluginProviderHttpHandler {
    async fn request(
        &self,
        _params: Value,
        _cancellation: &CancellationToken,
    ) -> Result<PluginHttpStreamResponse, PluginRpcError> {
        Err(rpc_error(
            "provider_http_unavailable",
            "host-mediated provider HTTP is unavailable on this host surface",
        ))
    }
}

/// Injected process launcher. Production launchers must sandbox before direct exec.
#[async_trait]
pub trait PluginLauncher: Send + Sync {
    /// Launches by direct exec. Implementations must revalidate and return the exact executable
    /// identity at the final spawn boundary, clear the environment, create a killable process
    /// group, and enforce every absent profile effect at syscall level. Manifest probes may read
    /// only their runtime/entrypoint; approved launches may read/write/network only when the
    /// corresponding helper above permits it. Network must traverse the policy proxy and exact
    /// public-domain allowlist.
    async fn launch(
        &self,
        config: &PluginProcessConfig,
        profile: &PluginSandboxProfile,
    ) -> Result<LaunchedPluginProcess, PluginProcessError>;
}

/// Host-owned handler for declared plugin-to-host push requests.
#[async_trait]
pub trait PushHandler: Send + Sync {
    async fn handle_push(&self, method: &str, params: Value) -> Result<Value, PluginRpcError>;
}

/// Rejects every push. Useful when a host surface has no interactive session attached.
pub struct DenyPushHandler;

#[async_trait]
impl PushHandler for DenyPushHandler {
    async fn handle_push(&self, _method: &str, _params: Value) -> Result<Value, PluginRpcError> {
        Err(PluginRpcError {
            code: "push_unavailable".to_owned(),
            message: "plugin push is unavailable on this host surface".to_owned(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PluginApprovalIdentity {
    pub plugin_name: String,
    pub manifest_fingerprint: String,
    pub executable: ExecutableIdentity,
    pub config_fingerprint: String,
    pub origin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::SourcePluginIdentity>,
}

impl PluginApprovalIdentity {
    fn fingerprint(&self) -> Result<String, PluginHostError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|error| PluginHostError::Protocol(error.to_string()))?;
        Ok(blake3::hash(&bytes).to_hex().to_string())
    }
}

fn approval_identity(
    manifest: &PluginManifest,
    config: &PluginProcessConfig,
    origin: &str,
) -> Result<PluginApprovalIdentity, PluginHostError> {
    if origin.is_empty() || origin.len() > 4096 || origin.chars().any(char::is_control) {
        return Err(PluginHostError::Approval(
            "plugin origin is invalid".to_owned(),
        ));
    }
    let mut config_value = json!({
        "argv": config.argv().iter().map(|value| os_fingerprint_bytes(value)).collect::<Vec<_>>(),
        "cwd": config.cwd(),
        "environment": config.environment_allowlist().iter().map(|value| os_fingerprint_bytes(value)).collect::<Vec<_>>(),
        "allowed_domains": config.allowed_domains(),
        "attested_files": config.attested_files(),
        "code_root": config.code_root(),
    });
    if let Some(source) = config.source_identity() {
        config_value["source"] = serde_json::to_value(source)
            .map_err(|error| PluginHostError::Protocol(error.to_string()))?;
    }
    let config_bytes = serde_json::to_vec(&config_value)
        .map_err(|error| PluginHostError::Protocol(error.to_string()))?;
    Ok(PluginApprovalIdentity {
        plugin_name: manifest.name.clone(),
        manifest_fingerprint: manifest.fingerprint().map_err(PluginApprovalError::from)?,
        executable: config.executable_identity().clone(),
        config_fingerprint: blake3::hash(&config_bytes).to_hex().to_string(),
        origin: origin.to_owned(),
        source: config.source_identity().cloned(),
    })
}

#[cfg(unix)]
fn os_fingerprint_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_fingerprint_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

/// Compares the exact executable/config/origin/manifest identity with durable approval.
///
/// # Errors
///
/// Returns an error if the identity cannot be validated, fingerprinted, or loaded.
pub fn plugin_launch_approval_requirement(
    store: &dyn ApprovalStore,
    manifest: &PluginManifest,
    config: &PluginProcessConfig,
    origin: &str,
) -> Result<ApprovalRequirement, PluginHostError> {
    let identity = approval_identity(manifest, config, origin)?;
    let current = identity.fingerprint()?;
    match store.approved_fingerprint(&manifest.name)? {
        None => Ok(ApprovalRequirement::FirstLoad {
            fingerprint: current,
        }),
        Some(previous) if previous == current => Ok(ApprovalRequirement::Approved),
        Some(previous) => Ok(ApprovalRequirement::ManifestChanged { previous, current }),
    }
}

/// Records explicit approval for an exact executable/config/origin/manifest identity.
///
/// # Errors
///
/// Returns an error if the identity cannot be validated, fingerprinted, or persisted.
pub fn approve_plugin_launch(
    store: &dyn ApprovalStore,
    manifest: &PluginManifest,
    config: &PluginProcessConfig,
    origin: &str,
) -> Result<String, PluginHostError> {
    let fingerprint = approval_identity(manifest, config, origin)?.fingerprint()?;
    store.record_approval(&manifest.name, &fingerprint)?;
    Ok(fingerprint)
}

#[derive(Debug, Error)]
pub enum PluginHostError {
    #[error(transparent)]
    ApprovalStore(#[from] crate::plugin::ApprovalStoreError),
    #[error(transparent)]
    ApprovalDetails(#[from] PluginApprovalError),
    #[error("plugin launch is not approved: {0}")]
    Approval(String),
    #[error(transparent)]
    Process(#[from] PluginProcessError),
    #[error("plugin protocol failed: {0}")]
    Protocol(String),
    #[error(transparent)]
    Rpc(#[from] PluginRpcError),
}

type Pending = Arc<Mutex<BTreeMap<RpcId, oneshot::Sender<Result<Value, PluginRpcError>>>>>;

enum ProviderStreamMessage {
    Event(Value),
    Failed(PluginRpcError),
    Complete,
}

struct PendingProviderStream {
    sender: mpsc::Sender<ProviderStreamMessage>,
    saw_finished: bool,
}

type PendingProviderStreams = Arc<StdMutex<BTreeMap<RpcId, PendingProviderStream>>>;
type ActiveProviderHttp = Arc<StdMutex<BTreeMap<RpcId, CancellationToken>>>;

struct ReaderState {
    writer: mpsc::Sender<RpcFrame>,
    pending: Pending,
    provider_streams: PendingProviderStreams,
    provider_http: Arc<dyn PluginProviderHttpHandler>,
    active_provider_http: ActiveProviderHttp,
    abandoned: Arc<Mutex<BTreeSet<RpcId>>>,
    enforcer: Arc<CapabilityEnforcer>,
    push_handler: Arc<dyn PushHandler>,
    redactor: Arc<dyn PluginBoundaryRedactor>,
    process: Arc<dyn SupervisedPluginProcess>,
}

/// Bounded, correlated, single-reader JSON-RPC client.
pub struct JsonRpcPluginClient {
    writer: mpsc::Sender<RpcFrame>,
    pending: Pending,
    provider_streams: PendingProviderStreams,
    abandoned: Arc<Mutex<BTreeSet<RpcId>>>,
    in_flight: Arc<Semaphore>,
    next_id: AtomicU64,
    timeout: Duration,
    process: Arc<dyn SupervisedPluginProcess>,
    closed: AtomicBool,
    shutdown_complete: AtomicBool,
    shutdown_lock: Mutex<()>,
    redactor: Arc<dyn PluginBoundaryRedactor>,
}

impl Drop for JsonRpcPluginClient {
    fn drop(&mut self) {
        if self.shutdown_complete.load(Ordering::Acquire) {
            return;
        }
        self.closed.store(true, Ordering::Release);
        let _ = self.process.kill_tree();
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
        let (writer, receiver) = mpsc::channel(WRITER_QUEUE_CAPACITY);
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let provider_streams = Arc::new(StdMutex::new(BTreeMap::new()));
        let abandoned = Arc::new(Mutex::new(BTreeSet::new()));
        let active_provider_http = Arc::new(StdMutex::new(BTreeMap::new()));
        let client = Arc::new(Self {
            writer,
            pending: Arc::clone(&pending),
            provider_streams: Arc::clone(&provider_streams),
            abandoned: Arc::clone(&abandoned),
            in_flight: Arc::new(Semaphore::new(WRITER_QUEUE_CAPACITY)),
            next_id: AtomicU64::new(1),
            timeout: if timeout.is_zero() {
                DEFAULT_REQUEST_TIMEOUT
            } else {
                timeout
            },
            process: Arc::clone(&launched.process),
            closed: AtomicBool::new(false),
            shutdown_complete: AtomicBool::new(false),
            shutdown_lock: Mutex::new(()),
            redactor: Arc::clone(&redactor),
        });
        tokio::spawn(writer_loop(
            launched.stdin,
            receiver,
            Arc::clone(&pending),
            Arc::clone(&launched.process),
        ));
        tokio::spawn(reader_loop(
            launched.stdout,
            ReaderState {
                writer: client.writer.clone(),
                pending,
                provider_streams,
                provider_http,
                active_provider_http,
                abandoned,
                enforcer,
                push_handler,
                redactor,
                process: Arc::clone(&launched.process),
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
    /// process exit, or a plugin-reported failure.
    pub async fn request_cancellable(
        &self,
        method: &str,
        params: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, PluginRpcError> {
        self.request_cancellable_inner(method, params, cancellation, false)
            .await
    }

    async fn request_cancellable_inner(
        &self,
        method: &str,
        params: Value,
        cancellation: &CancellationToken,
        allow_closed: bool,
    ) -> Result<Value, PluginRpcError> {
        if self.closed.load(Ordering::Acquire) && !allow_closed {
            return Err(rpc_error("closed", "plugin RPC client is closed"));
        }
        let permit = tokio::select! {
            () = cancellation.cancelled() => return Err(rpc_error("cancelled", "plugin RPC request was cancelled")),
            permit = tokio::time::timeout(self.timeout, self.in_flight.acquire()) => {
                permit.map_err(|_| rpc_error("backpressure_timeout", "plugin RPC request limit remained saturated"))?
                    .map_err(|_| rpc_error("closed", "plugin RPC client is closed"))?
            }
        };
        let numeric = self.next_id.fetch_add(1, Ordering::AcqRel);
        let id = RpcId::Number(
            i64::try_from(numeric)
                .map_err(|_| rpc_error("id_exhausted", "plugin RPC request IDs exhausted"))?,
        );
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), sender);
        let frame = RpcFrame::Request(RpcRequest {
            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
            id: id.clone(),
            method: method.to_owned(),
            params: Some(self.redactor.redact(params)),
        });
        match tokio::time::timeout(self.timeout, self.writer.send(frame)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                return Err(rpc_error(
                    "connection_closed",
                    "plugin RPC connection closed",
                ));
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(rpc_error(
                    "backpressure_timeout",
                    "plugin RPC writer remained saturated",
                ));
            }
        }
        let result = tokio::select! {
            () = cancellation.cancelled() => {
                self.abandon(id).await;
                Err(rpc_error("cancelled", "plugin RPC request was cancelled"))
            }
            response = tokio::time::timeout(self.timeout, receiver) => {
                match response {
                    Ok(Ok(result)) => result,
                    Ok(Err(_)) => Err(rpc_error("connection_closed", "plugin RPC connection closed")),
                    Err(_) => {
                        self.abandon(id).await;
                        Err(rpc_error("timeout", "plugin RPC request timed out"))
                    }
                }
            }
        };
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
        if graceful.is_ok_and(|result| result.is_ok())
            && tokio::time::timeout(timeout, self.process.wait())
                .await
                .is_ok_and(|result| result.is_ok())
        {
            self.shutdown_complete.store(true, Ordering::Release);
            return Ok(());
        }
        let kill_error = self.process.kill_tree().err();
        let reap = tokio::time::timeout(timeout, self.process.reap()).await;
        if let Some(error) = kill_error {
            return Err(PluginHostError::Process(error));
        }
        let result = match reap {
            Ok(result) => result.map_err(PluginHostError::Process),
            Err(_) => Err(PluginHostError::Process(PluginProcessError {
                message: "plugin did not reap before shutdown deadline".to_owned(),
            })),
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
        self.request_cancellable_inner(method, params, cancellation, true)
            .await
    }

    async fn abandon(&self, id: RpcId) {
        let mut pending = self.pending.lock().await;
        let mut abandoned = self.abandoned.lock().await;
        pending.remove(&id);
        if abandoned.len() >= MAX_ABANDONED_REQUESTS {
            drop(abandoned);
            self.closed.store(true, Ordering::Release);
            let _ = self.process.kill_tree();
        } else {
            abandoned.insert(id);
        }
    }

    async fn send_notification(&self, method: &str, params: Value) -> Result<(), PluginRpcError> {
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
            .map_err(|_| rpc_error("connection_closed", "plugin RPC connection closed"))
    }

    async fn start_provider_stream(
        &self,
        params: Value,
    ) -> Result<PluginProviderEventStream, PluginRpcError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(rpc_error("closed", "plugin RPC client is closed"));
        }
        let permit =
            tokio::time::timeout(self.timeout, Arc::clone(&self.in_flight).acquire_owned())
                .await
                .map_err(|_| {
                    rpc_error(
                        "backpressure_timeout",
                        "plugin provider stream limit remained saturated",
                    )
                })?
                .map_err(|_| rpc_error("closed", "plugin RPC client is closed"))?;
        let numeric = self.next_id.fetch_add(1, Ordering::AcqRel);
        let id = RpcId::Number(
            i64::try_from(numeric)
                .map_err(|_| rpc_error("id_exhausted", "plugin RPC request IDs exhausted"))?,
        );
        let (sender, receiver) = mpsc::channel(PROVIDER_EVENT_QUEUE_CAPACITY);
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
                    sender,
                    saw_finished: false,
                },
            );
        let frame = RpcFrame::Request(RpcRequest {
            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
            id: id.clone(),
            method: METHOD_PROVIDER_COMPLETE.to_owned(),
            params: Some(self.redactor.redact(params)),
        });
        let sent = tokio::time::timeout(self.timeout, self.writer.send(frame)).await;
        if !matches!(sent, Ok(Ok(()))) {
            if let Ok(mut streams) = self.provider_streams.lock() {
                streams.remove(&id);
            }
            return Err(match sent {
                Err(_) => rpc_error(
                    "backpressure_timeout",
                    "plugin RPC writer remained saturated",
                ),
                Ok(Err(_) | Ok(())) => {
                    rpc_error("connection_closed", "plugin RPC connection closed")
                }
            });
        }
        Ok(Box::pin(JsonRpcProviderEventStream {
            receiver,
            id: Some(id),
            writer: self.writer.clone(),
            provider_streams: Arc::clone(&self.provider_streams),
            abandoned: Arc::clone(&self.abandoned),
            process: Arc::clone(&self.process),
            _permit: permit,
        }))
    }
}

struct JsonRpcProviderEventStream {
    receiver: mpsc::Receiver<ProviderStreamMessage>,
    id: Option<RpcId>,
    writer: mpsc::Sender<RpcFrame>,
    provider_streams: PendingProviderStreams,
    abandoned: Arc<Mutex<BTreeSet<RpcId>>>,
    process: Arc<dyn SupervisedPluginProcess>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Stream for JsonRpcProviderEventStream {
    type Item = Result<Value, PluginRpcError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.receiver.poll_recv(context) {
            Poll::Ready(Some(ProviderStreamMessage::Event(event))) => Poll::Ready(Some(Ok(event))),
            Poll::Ready(Some(ProviderStreamMessage::Failed(error))) => {
                self.id = None;
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(Some(ProviderStreamMessage::Complete) | None) => {
                self.id = None;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for JsonRpcProviderEventStream {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        let removed = self
            .provider_streams
            .lock()
            .is_ok_and(|mut streams| streams.remove(&id).is_some());
        if !removed {
            return;
        }
        let Ok(mut abandoned) = self.abandoned.try_lock() else {
            let _ = self.process.kill_tree();
            return;
        };
        if abandoned.len() >= MAX_ABANDONED_REQUESTS {
            let _ = self.process.kill_tree();
            return;
        }
        abandoned.insert(id.clone());
        drop(abandoned);
        let cancel = RpcFrame::Notification(RpcNotification {
            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
            method: METHOD_PROVIDER_CANCEL.to_owned(),
            params: Some(json!({"request_id":id})),
        });
        if self.writer.try_send(cancel).is_err() {
            let _ = self.process.kill_tree();
        }
    }
}

#[async_trait]
impl PluginRpcClient for JsonRpcPluginClient {
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
    mut receiver: mpsc::Receiver<RpcFrame>,
    pending: Pending,
    process: Arc<dyn SupervisedPluginProcess>,
) {
    while let Some(frame) = receiver.recv().await {
        let encoded = match encode_frame(&frame, MAX_FRAME_BYTES) {
            Ok(encoded) => encoded,
            Err(error) => {
                fail_pending(&pending, rpc_error("invalid_frame", &error.to_string())).await;
                terminate_and_reap(process.as_ref()).await;
                return;
            }
        };
        if stdin.write_all(&encoded).await.is_err() || stdin.flush().await.is_err() {
            fail_pending(
                &pending,
                rpc_error("write_failed", "plugin RPC stdin failed"),
            )
            .await;
            terminate_and_reap(process.as_ref()).await;
            return;
        }
    }
    let _ = stdin.shutdown().await;
}

async fn reader_loop(mut stdout: PluginStdout, state: ReaderState) {
    let mut buffer = [0_u8; 8192];
    let mut decoder = FrameDecoder::default();
    loop {
        let count = match tokio::io::AsyncReadExt::read(&mut stdout, &mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        let Ok(frames) = decoder.push(&buffer[..count]) else {
            cancel_active_provider_http(&state.active_provider_http);
            terminate_and_reap(state.process.as_ref()).await;
            fail_pending(
                &state.pending,
                rpc_error(
                    "invalid_frame",
                    "plugin emitted an invalid or oversized frame",
                ),
            )
            .await;
            fail_provider_streams(
                &state.provider_streams,
                &rpc_error(
                    "invalid_frame",
                    "plugin emitted an invalid or oversized frame",
                ),
            );
            return;
        };
        for frame in frames {
            let continue_reading = process_incoming_frame(frame, &state).await;
            if !continue_reading {
                cancel_active_provider_http(&state.active_provider_http);
                terminate_and_reap(state.process.as_ref()).await;
                fail_pending(
                    &state.pending,
                    rpc_error(
                        "protocol_violation",
                        "plugin RPC stream violated correlation or capabilities",
                    ),
                )
                .await;
                fail_provider_streams(
                    &state.provider_streams,
                    &rpc_error(
                        "protocol_violation",
                        "plugin RPC stream violated correlation or capabilities",
                    ),
                );
                return;
            }
        }
    }
    cancel_active_provider_http(&state.active_provider_http);
    fail_pending(
        &state.pending,
        rpc_error("connection_closed", "plugin RPC connection closed"),
    )
    .await;
    fail_provider_streams(
        &state.provider_streams,
        &rpc_error("connection_closed", "plugin RPC connection closed"),
    );
    terminate_and_reap(state.process.as_ref()).await;
}

fn cancel_active_provider_http(active: &ActiveProviderHttp) {
    if let Ok(mut active) = active.lock() {
        for (_, cancellation) in std::mem::take(&mut *active) {
            cancellation.cancel();
        }
    }
}

async fn terminate_and_reap(process: &dyn SupervisedPluginProcess) {
    let _ = process.kill_tree();
    let _ = tokio::time::timeout(DEFAULT_SHUTDOWN_TIMEOUT, process.reap()).await;
}

#[allow(clippy::too_many_lines)]
async fn process_incoming_frame(frame: RpcFrame, state: &ReaderState) -> bool {
    match frame {
        RpcFrame::Success(success) => {
            let Some(id) = success.id else {
                let _ = state.process.kill_tree();
                return false;
            };
            let provider = state
                .provider_streams
                .lock()
                .ok()
                .and_then(|mut streams| streams.remove(&id));
            if let Some(provider) = provider {
                if !success.result.is_null() || !provider.saw_finished {
                    let _ = provider
                        .sender
                        .try_send(ProviderStreamMessage::Failed(rpc_error(
                            "invalid_provider_stream",
                            "plugin provider stream ended without one terminal finished event",
                        )));
                    let _ = state.process.kill_tree();
                    return false;
                }
                return provider
                    .sender
                    .try_send(ProviderStreamMessage::Complete)
                    .is_ok();
            }
            if let Some(sender) = state.pending.lock().await.remove(&id) {
                let _ = sender.send(Ok(success.result));
                true
            } else if state.abandoned.lock().await.remove(&id) {
                true
            } else {
                let _ = state.process.kill_tree();
                false
            }
        }
        RpcFrame::Failure(failure) => {
            let Some(id) = failure.id else {
                let _ = state.process.kill_tree();
                return false;
            };
            let safe_code = failure
                .error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let safe_code = if safe_code.is_empty() {
                failure.error.code.to_string()
            } else {
                safe_code
            };
            let provider = state
                .provider_streams
                .lock()
                .ok()
                .and_then(|mut streams| streams.remove(&id));
            if let Some(provider) = provider {
                let _ = provider
                    .sender
                    .try_send(ProviderStreamMessage::Failed(PluginRpcError {
                        code: safe_code.clone(),
                        message: failure.error.message,
                    }));
                return true;
            }
            if let Some(sender) = state.pending.lock().await.remove(&id) {
                let _ = sender.send(Err(PluginRpcError {
                    code: safe_code,
                    message: failure.error.message,
                }));
                true
            } else if state.abandoned.lock().await.remove(&id) {
                true
            } else {
                let _ = state.process.kill_tree();
                false
            }
        }
        RpcFrame::Request(request) => {
            if request.method == METHOD_PROVIDER_HTTP {
                return start_provider_http_request(request, state);
            }
            let response = handle_push_request(
                &state.enforcer,
                state.push_handler.as_ref(),
                &request.method,
                state.redactor.redact(request.params.unwrap_or(Value::Null)),
            )
            .await;
            let response_frame = match response {
                Ok(result) => RpcFrame::Success(RpcSuccess {
                    jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                    id: Some(request.id),
                    result,
                }),
                Err(error) => RpcFrame::Failure(RpcFailure {
                    jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                    id: Some(request.id),
                    error: rw_plugin_protocol::RpcErrorObject {
                        code: -32000,
                        message: error.message,
                        data: Some(json!({"code":error.code})),
                    },
                }),
            };
            tokio::time::timeout(DEFAULT_REQUEST_TIMEOUT, state.writer.send(response_frame))
                .await
                .is_ok_and(|result| result.is_ok())
                && !state.enforcer.violated()
        }
        RpcFrame::Notification(notification) => {
            if notification.method == METHOD_PROVIDER_HTTP_CANCEL {
                return cancel_provider_http_request(
                    &state.active_provider_http,
                    notification.params.unwrap_or(Value::Null),
                );
            }
            if notification.method == METHOD_PROVIDER_EVENT {
                return handle_provider_event(
                    &state.provider_streams,
                    &state.abandoned,
                    notification.params.unwrap_or(Value::Null),
                )
                .await;
            }
            let _ = handle_push_request(
                &state.enforcer,
                state.push_handler.as_ref(),
                &notification.method,
                state
                    .redactor
                    .redact(notification.params.unwrap_or(Value::Null)),
            )
            .await;
            !state.enforcer.violated()
        }
    }
}

fn start_provider_http_request(request: RpcRequest, state: &ReaderState) -> bool {
    let params = request.params.unwrap_or(Value::Null);
    let Ok(capability) = serde_json::from_value::<ProviderHttpCapabilityParams>(params.clone())
    else {
        return false;
    };
    if state
        .enforcer
        .check_provider_credential(&capability.alias, &capability.credential_reference)
        .is_err()
    {
        return false;
    }
    let _ = capability.request;
    let cancellation = CancellationToken::default();
    let inserted = state.active_provider_http.lock().is_ok_and(|mut active| {
        if active.len() >= WRITER_QUEUE_CAPACITY || active.contains_key(&request.id) {
            false
        } else {
            active.insert(request.id.clone(), cancellation.clone());
            true
        }
    });
    if !inserted {
        return false;
    }
    let handler = Arc::clone(&state.provider_http);
    let writer = state.writer.clone();
    let active = Arc::clone(&state.active_provider_http);
    let redactor = Arc::clone(&state.redactor);
    tokio::spawn(async move {
        stream_provider_http_response(
            request.id,
            params,
            cancellation,
            handler,
            writer,
            active,
            redactor,
        )
        .await;
    });
    true
}

fn cancel_provider_http_request(active: &ActiveProviderHttp, params: Value) -> bool {
    let Ok(cancel) = serde_json::from_value::<ProviderCancelParams>(params) else {
        return false;
    };
    let Ok(active) = active.lock() else {
        return false;
    };
    if let Some(token) = active.get(&cancel.request_id) {
        token.cancel();
    }
    true
}

#[allow(clippy::too_many_arguments)]
async fn stream_provider_http_response(
    id: RpcId,
    params: Value,
    cancellation: CancellationToken,
    handler: Arc<dyn PluginProviderHttpHandler>,
    writer: mpsc::Sender<RpcFrame>,
    active: ActiveProviderHttp,
    redactor: Arc<dyn PluginBoundaryRedactor>,
) {
    let result = handler.request(params, &cancellation).await;
    let result = match result {
        Ok(mut response) => {
            let head = json!({
                "request_id": id,
                "event": {
                    "type": "head",
                    "status": response.status,
                    "headers": response.headers,
                }
            });
            if send_provider_http_event(&writer, redactor.as_ref(), head)
                .await
                .is_err()
            {
                cancellation.cancel();
                Err(rpc_error(
                    "connection_closed",
                    "plugin RPC connection closed",
                ))
            } else {
                stream_provider_http_body(
                    &id,
                    &mut response.body,
                    &cancellation,
                    &writer,
                    redactor.as_ref(),
                )
                .await
            }
        }
        Err(error) => Err(error),
    };
    if let Ok(mut active) = active.lock() {
        active.remove(&id);
    }
    let frame = match result {
        Ok(()) => RpcFrame::Success(RpcSuccess {
            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
            id: Some(id),
            result: Value::Null,
        }),
        Err(error) => RpcFrame::Failure(RpcFailure {
            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
            id: Some(id),
            error: rw_plugin_protocol::RpcErrorObject {
                code: -32020,
                message: error.message,
                data: Some(json!({"code":error.code})),
            },
        }),
    };
    let _ = writer.send(frame).await;
}

async fn stream_provider_http_body(
    id: &RpcId,
    body: &mut PluginHttpByteStream,
    cancellation: &CancellationToken,
    writer: &mpsc::Sender<RpcFrame>,
    redactor: &dyn PluginBoundaryRedactor,
) -> Result<(), PluginRpcError> {
    let overlap = redactor.maximum_secret_bytes().saturating_sub(1);
    let mut pending = Vec::new();
    loop {
        let next = tokio::select! {
            () = cancellation.cancelled() => {
                return Err(rpc_error("cancelled", "host-mediated provider HTTP was cancelled"));
            }
            next = body.next() => next,
        };
        let Some(chunk) = next else {
            break;
        };
        pending.extend_from_slice(&chunk?);
        if pending.len() <= overlap {
            continue;
        }
        let (bytes, tail) = redactor.redact_streaming_prefix(&pending, overlap);
        pending = tail;
        if bytes.is_empty() {
            continue;
        }
        send_provider_http_event(
            writer,
            redactor,
            json!({
                "request_id": id,
                "event": {"type":"body","data_base64":BASE64_STANDARD.encode(bytes)},
            }),
        )
        .await?;
    }
    if !pending.is_empty() {
        let bytes = redactor.redact_bytes(&pending);
        send_provider_http_event(
            writer,
            redactor,
            json!({
                "request_id": id,
                "event": {"type":"body","data_base64":BASE64_STANDARD.encode(bytes)},
            }),
        )
        .await?;
    }
    send_provider_http_event(
        writer,
        redactor,
        json!({"request_id":id,"event":{"type":"finished"}}),
    )
    .await
}

async fn send_provider_http_event(
    writer: &mpsc::Sender<RpcFrame>,
    redactor: &dyn PluginBoundaryRedactor,
    params: Value,
) -> Result<(), PluginRpcError> {
    writer
        .send(RpcFrame::Notification(RpcNotification {
            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
            method: METHOD_PROVIDER_HTTP_EVENT.to_owned(),
            params: Some(redactor.redact(params)),
        }))
        .await
        .map_err(|_| rpc_error("connection_closed", "plugin RPC connection closed"))
}

async fn handle_provider_event(
    streams: &PendingProviderStreams,
    abandoned: &Mutex<BTreeSet<RpcId>>,
    params: Value,
) -> bool {
    let Ok(notification) = serde_json::from_value::<ProviderEventParams>(params) else {
        return false;
    };
    let Ok(event) = serde_json::from_value::<ProviderEvent>(notification.event.clone()) else {
        return false;
    };
    let finished = matches!(event, ProviderEvent::Finished { .. });
    let delivered = {
        let Ok(mut streams) = streams.lock() else {
            return false;
        };
        streams.get_mut(&notification.request_id).map(|stream| {
            if stream.saw_finished {
                return false;
            }
            if finished {
                stream.saw_finished = true;
            }
            stream
                .sender
                .try_send(ProviderStreamMessage::Event(notification.event))
                .is_ok()
        })
    };
    match delivered {
        Some(delivered) => delivered,
        None => abandoned.lock().await.contains(&notification.request_id),
    }
}

async fn handle_push_request(
    enforcer: &CapabilityEnforcer,
    handler: &dyn PushHandler,
    method: &str,
    params: Value,
) -> Result<Value, PluginRpcError> {
    enforcer
        .check_push_method(method)
        .map_err(|error| rpc_error("capability_violation", &error.to_string()))?;
    validate_push_params(method, &params)?;
    tokio::time::timeout(DEFAULT_REQUEST_TIMEOUT, handler.handle_push(method, params))
        .await
        .map_err(|_| rpc_error("push_timeout", "plugin push handler timed out"))?
}

fn validate_push_params(method: &str, params: &Value) -> Result<(), PluginRpcError> {
    let object = params
        .as_object()
        .ok_or_else(|| rpc_error("invalid_push", "plugin push parameters must be an object"))?;
    let (allowed, fields): (&[&str], &[(&str, usize)]) = match method {
        METHOD_SESSION_INJECT_MESSAGE => (
            &["session_id", "content"],
            &[
                ("session_id", MAX_NAME_BYTES),
                ("content", MAX_HOOK_PAYLOAD_BYTES),
            ],
        ),
        METHOD_SESSION_SET_STATUS => (
            &["session_id", "status"],
            &[
                ("session_id", MAX_NAME_BYTES),
                ("status", MAX_RPC_MESSAGE_BYTES),
            ],
        ),
        METHOD_UI_NOTIFY => (
            &["title", "message", "session_id"],
            &[
                ("title", MAX_NAME_BYTES),
                ("message", MAX_RPC_MESSAGE_BYTES),
            ],
        ),
        _ => return Err(rpc_error("invalid_push", "plugin push method is unknown")),
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(rpc_error(
            "invalid_push",
            "plugin push contains unknown fields",
        ));
    }
    for (field, limit) in fields {
        let value = object
            .get(*field)
            .and_then(Value::as_str)
            .ok_or_else(|| rpc_error("invalid_push", "plugin push contains an invalid field"))?;
        if value.is_empty() || value.len() > *limit || value.chars().any(char::is_control) {
            return Err(rpc_error(
                "invalid_push",
                "plugin push field exceeds its bounds",
            ));
        }
    }
    if let Some(session_id) = object.get("session_id") {
        let session_id = session_id
            .as_str()
            .ok_or_else(|| rpc_error("invalid_push", "plugin push session id is invalid"))?;
        if rw_types::SessionId::validate(session_id).is_err() {
            return Err(rpc_error(
                "invalid_push",
                "plugin push session id is invalid",
            ));
        }
    }
    Ok(())
}

async fn fail_pending(pending: &Pending, error: PluginRpcError) {
    for (_, sender) in std::mem::take(&mut *pending.lock().await) {
        let _ = sender.send(Err(error.clone()));
    }
}

fn fail_provider_streams(streams: &PendingProviderStreams, error: &PluginRpcError) {
    let Ok(mut streams) = streams.lock() else {
        return;
    };
    for (_, stream) in std::mem::take(&mut *streams) {
        let _ = stream
            .sender
            .try_send(ProviderStreamMessage::Failed(error.clone()));
    }
}

async fn drain_stderr(mut stderr: PluginStdout) {
    let mut buffer = [0u8; 4096];
    loop {
        match tokio::io::AsyncReadExt::read(&mut stderr, &mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

fn rpc_error(code: &str, message: &str) -> PluginRpcError {
    PluginRpcError {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

/// Running plugin with an immutable manifest/capability snapshot.
pub struct PluginHost {
    manifest: PluginManifest,
    client: Arc<JsonRpcPluginClient>,
    enforcer: Arc<CapabilityEnforcer>,
}

fn approved_plugin_profile(
    store: &dyn ApprovalStore,
    config: &PluginProcessConfig,
    origin: &str,
    approved_roots: &[PathBuf],
    expected_manifest: &PluginManifest,
) -> Result<PluginSandboxProfile, PluginHostError> {
    expected_manifest
        .validate()
        .map_err(PluginApprovalError::from)?;
    if plugin_launch_approval_requirement(store, expected_manifest, config, origin)?
        != ApprovalRequirement::Approved
    {
        return Err(PluginHostError::Approval(
            "executable, config, origin, or manifest requires explicit approval".to_owned(),
        ));
    }
    config.validate_executable_identity()?;
    let roots = canonical_roots(approved_roots)?;
    let cwd_authorized = if config.source_identity().is_some() {
        config
            .code_root()
            .is_some_and(|root| config.cwd().starts_with(&root.canonical_path))
    } else {
        roots.iter().any(|root| config.cwd().starts_with(root))
    };
    if !cwd_authorized {
        return Err(PluginHostError::Approval(
            "plugin cwd is outside its owned runtime root".to_owned(),
        ));
    }
    let reads_workspace = expected_manifest.capabilities.tools.iter().any(|tool| {
        tool.caps
            .contains(&rw_plugin_protocol::PluginToolEffect::ReadsFilesystem)
    });
    if !reads_workspace
        && config
            .code_root()
            .is_some_and(|code_root| roots.contains(&code_root.canonical_path))
    {
        return Err(PluginHostError::Approval(
            "plugin code root must be a strict workspace descendant unless reads-fs is declared"
                .to_owned(),
        ));
    }
    let requests_network = !expected_manifest.capabilities.providers.is_empty()
        || expected_manifest.capabilities.tools.iter().any(|tool| {
            tool.caps
                .contains(&rw_plugin_protocol::PluginToolEffect::Network)
        });
    if requests_network && config.allowed_domains().is_empty() {
        return Err(PluginHostError::Approval(
            "network-capable plugins require an explicit public-domain allowlist".to_owned(),
        ));
    }
    Ok(PluginSandboxProfile {
        mode: PluginSandboxMode::Approved,
        capabilities: expected_manifest.capabilities.clone(),
        approved_roots: roots,
        allowed_domains: config.allowed_domains().iter().cloned().collect(),
    })
}

impl PluginHost {
    /// Launches an approved plugin on a host surface that does not provide
    /// host-mediated provider HTTP.
    ///
    /// # Errors
    ///
    /// Returns the same approval, launch, handshake, or manifest error as the
    /// HTTP-capable launch boundary.
    #[allow(
        clippy::too_many_arguments,
        reason = "security-sensitive launch inputs remain explicit at the approval boundary"
    )]
    pub async fn launch_approved(
        launcher: &dyn PluginLauncher,
        store: &dyn ApprovalStore,
        config: &PluginProcessConfig,
        origin: &str,
        approved_roots: &[PathBuf],
        expected_manifest: PluginManifest,
        push_handler: Arc<dyn PushHandler>,
        redactor: Arc<dyn PluginBoundaryRedactor>,
    ) -> Result<Self, PluginHostError> {
        Self::launch_approved_with_http(
            launcher,
            store,
            config,
            origin,
            approved_roots,
            expected_manifest,
            push_handler,
            Arc::new(DenyPluginProviderHttpHandler),
            redactor,
        )
        .await
    }

    /// Launches only an exact approved executable/config/origin/manifest identity and completes
    /// the protocol handshake before exposing adapters.
    ///
    /// # Errors
    ///
    /// Returns an error for missing approval, identity drift, invalid roots, launch failure,
    /// handshake failure, or a manifest different from the approved snapshot.
    #[allow(
        clippy::too_many_arguments,
        reason = "security-sensitive launch inputs remain explicit at the approval boundary"
    )]
    pub async fn launch_approved_with_http(
        launcher: &dyn PluginLauncher,
        store: &dyn ApprovalStore,
        config: &PluginProcessConfig,
        origin: &str,
        approved_roots: &[PathBuf],
        expected_manifest: PluginManifest,
        push_handler: Arc<dyn PushHandler>,
        provider_http: Arc<dyn PluginProviderHttpHandler>,
        redactor: Arc<dyn PluginBoundaryRedactor>,
    ) -> Result<Self, PluginHostError> {
        let profile =
            approved_plugin_profile(store, config, origin, approved_roots, &expected_manifest)?;
        let child = launcher.launch(config, &profile).await?;
        if child.executable_identity != *config.executable_identity() {
            terminate_and_reap(child.process.as_ref()).await;
            return Err(PluginHostError::Approval(
                "launcher executable attestation differs from approved identity".to_owned(),
            ));
        }
        let process = Arc::clone(&child.process);
        let enforcer = Arc::new(CapabilityEnforcer::new(
            &expected_manifest,
            Arc::clone(&process),
        ));
        let client = JsonRpcPluginClient::start(
            child,
            Arc::clone(&enforcer),
            push_handler,
            provider_http,
            redactor,
            DEFAULT_REQUEST_TIMEOUT,
        );
        let initialize = serde_json::to_value(InitializeParams {
            host: rw_plugin_protocol::PLUGIN_HOST_ID.to_owned(),
            protocol: expected_manifest.protocol,
            min_protocol: rw_plugin_protocol::MIN_PROTOCOL_VERSION,
            max_frame_bytes: MAX_FRAME_BYTES,
            capabilities: vec!["provider-models".to_owned(), "provider-http".to_owned()],
        })
        .map_err(|error| PluginHostError::Rpc(rpc_error("invalid_request", &error.to_string())))?;
        let result = client.request(METHOD_INITIALIZE, initialize).await;
        let initialized: PluginManifest = match result.and_then(|value| {
            serde_json::from_value(value)
                .map_err(|error| rpc_error("invalid_manifest", &error.to_string()))
        }) {
            Ok(manifest) => manifest,
            Err(error) => {
                terminate_and_reap(process.as_ref()).await;
                return Err(PluginHostError::Rpc(error));
            }
        };
        if let Err(error) = initialized.validate() {
            terminate_and_reap(process.as_ref()).await;
            return Err(PluginHostError::ApprovalDetails(error.into()));
        }
        if initialized
            .fingerprint()
            .map_err(PluginApprovalError::from)?
            != expected_manifest
                .fingerprint()
                .map_err(PluginApprovalError::from)?
        {
            terminate_and_reap(process.as_ref()).await;
            return Err(PluginHostError::Approval(
                "initialized manifest differs from approved manifest".to_owned(),
            ));
        }
        Ok(Self {
            manifest: initialized,
            client,
            enforcer,
        })
    }

    #[must_use]
    pub const fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    #[must_use]
    pub fn client(&self) -> Arc<dyn PluginRpcClient> {
        self.client.clone()
    }
    #[must_use]
    pub fn enforcer(&self) -> Arc<CapabilityEnforcer> {
        Arc::clone(&self.enforcer)
    }
    /// Gracefully shuts down and reaps the plugin process.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be terminated or reaped.
    pub async fn shutdown(&self) -> Result<(), PluginHostError> {
        self.client.shutdown(DEFAULT_SHUTDOWN_TIMEOUT).await
    }
}

/// Starts an initialization-only, zero-capability process to discover its manifest.
/// The child is always terminated and reaped; callers must separately approve and relaunch it.
///
/// # Errors
///
/// Returns an error for invalid roots or identity, launch/transport failure, malformed manifests,
/// or bounded shutdown failure.
#[cfg(test)]
pub(crate) async fn probe_plugin_manifest(
    launcher: &dyn PluginLauncher,
    config: &PluginProcessConfig,
    approved_roots: &[PathBuf],
    redactor: Arc<dyn PluginBoundaryRedactor>,
) -> Result<PluginManifest, PluginHostError> {
    config.validate_executable_identity()?;
    let roots = canonical_roots(approved_roots)?;
    if !roots.iter().any(|root| config.cwd().starts_with(root)) {
        return Err(PluginHostError::Approval(
            "plugin cwd is outside approved roots".to_owned(),
        ));
    }
    let child = launcher
        .launch(
            config,
            &PluginSandboxProfile {
                mode: PluginSandboxMode::ManifestProbe,
                capabilities: PluginCapabilities::default(),
                approved_roots: roots,
                allowed_domains: Vec::new(),
            },
        )
        .await?;
    if child.executable_identity != *config.executable_identity() {
        terminate_and_reap(child.process.as_ref()).await;
        return Err(PluginHostError::Approval(
            "launcher executable attestation differs from configured identity".to_owned(),
        ));
    }
    let process = Arc::clone(&child.process);
    let empty_manifest = PluginManifest {
        name: "manifest-probe".to_owned(),
        version: "0".to_owned(),
        protocol: rw_plugin_protocol::PROTOCOL_VERSION,
        capabilities: PluginCapabilities::default(),
    };
    let enforcer = Arc::new(CapabilityEnforcer::new(
        &empty_manifest,
        Arc::clone(&process),
    ));
    let client = JsonRpcPluginClient::start(
        child,
        enforcer,
        Arc::new(DenyPushHandler),
        Arc::new(DenyPluginProviderHttpHandler),
        redactor,
        DEFAULT_REQUEST_TIMEOUT,
    );
    let value = client
        .request(
            METHOD_INITIALIZE,
            serde_json::to_value(InitializeParams {
                host: rw_plugin_protocol::PLUGIN_HOST_ID.to_owned(),
                protocol: rw_plugin_protocol::PROTOCOL_VERSION,
                min_protocol: rw_plugin_protocol::MIN_PROTOCOL_VERSION,
                max_frame_bytes: MAX_FRAME_BYTES,
                capabilities: vec!["provider-models".to_owned(), "provider-http".to_owned()],
            })
            .map_err(|error| rpc_error("invalid_request", &error.to_string()))?,
        )
        .await;
    let manifest: PluginManifest = match value.and_then(|value| {
        serde_json::from_value(value)
            .map_err(|_| rpc_error("invalid_manifest", "plugin returned an invalid manifest"))
    }) {
        Ok(manifest) => manifest,
        Err(error) => {
            terminate_and_reap(process.as_ref()).await;
            return Err(error.into());
        }
    };
    if let Err(error) = manifest.validate() {
        terminate_and_reap(process.as_ref()).await;
        return Err(PluginApprovalError::from(error).into());
    }
    client.shutdown(DEFAULT_SHUTDOWN_TIMEOUT).await?;
    Ok(manifest)
}

fn canonical_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>, PluginHostError> {
    if roots.is_empty() {
        return Err(PluginHostError::Approval(
            "at least one approved root is required".to_owned(),
        ));
    }
    roots
        .iter()
        .map(|root| {
            std::fs::canonicalize(root).map_err(|error| {
                PluginHostError::Approval(format!("invalid approved root: {error}"))
            })
        })
        .collect()
}

pub struct RpcToolAdapter {
    declaration: rw_plugin_protocol::PluginToolCapability,
    client: Arc<dyn PluginRpcClient>,
    enforcer: Arc<CapabilityEnforcer>,
}

impl RpcToolAdapter {
    /// Constructs an adapter only for the exact immutable approved declaration.
    ///
    /// # Errors
    ///
    /// Returns an approval error if any declaration field differs from the manifest snapshot.
    pub fn new(
        declaration: rw_plugin_protocol::PluginToolCapability,
        client: Arc<dyn PluginRpcClient>,
        enforcer: Arc<CapabilityEnforcer>,
    ) -> Result<Self, PluginHostError> {
        if !enforcer.tool_declaration_matches(&declaration) {
            return Err(PluginHostError::Approval(
                "tool adapter declaration differs from approved manifest".to_owned(),
            ));
        }
        Ok(Self {
            declaration,
            client,
            enforcer,
        })
    }
}

#[async_trait]
impl Tool for RpcToolAdapter {
    fn descriptor(&self) -> ToolDescriptor {
        let process_effects = self.enforcer.process_tool_effects();
        ToolDescriptor {
            name: self.declaration.name.clone(),
            description: self.declaration.description.clone(),
            input_schema: self.declaration.schema.clone(),
            capabilities: CapabilityManifest::new(process_effects.into_iter().map(tool_effect)),
        }
    }

    fn mutation_scope(&self, _input: &Value) -> MutationScope {
        if self
            .enforcer
            .process_tool_effects()
            .contains(&rw_plugin_protocol::PluginToolEffect::WritesFilesystem)
        {
            MutationScope::OpaqueWorkspace
        } else {
            MutationScope::None
        }
    }

    async fn execute(&self, _context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        self.enforcer
            .check_tool(&self.declaration.name)
            .map_err(|error| ToolError::Output(error.to_string()))?;
        let result = self
            .client
            .request_cancellable(
                METHOD_TOOL_CALL,
                serde_json::to_value(ToolCallParams {
                    name: self.declaration.name.clone(),
                    input,
                })
                .map_err(|error| ToolError::Output(error.to_string()))?,
                &_context.cancellation,
            )
            .await
            .map_err(|error| ToolError::Output(error.to_string()))?;
        serde_json::from_value(result).map_err(|error| {
            ToolError::Output(format!("plugin returned invalid tool result: {error}"))
        })
    }
}

fn tool_effect(effect: rw_plugin_protocol::PluginToolEffect) -> ToolCapability {
    match effect {
        rw_plugin_protocol::PluginToolEffect::ReadsFilesystem => ToolCapability::ReadFilesystem,
        rw_plugin_protocol::PluginToolEffect::WritesFilesystem => ToolCapability::WriteFilesystem,
        rw_plugin_protocol::PluginToolEffect::Network => ToolCapability::Network,
        rw_plugin_protocol::PluginToolEffect::Execute => ToolCapability::Execute,
    }
}

pub struct RpcCommandAdapter {
    name: String,
    client: Arc<dyn PluginRpcClient>,
    enforcer: Arc<CapabilityEnforcer>,
}

impl RpcCommandAdapter {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        client: Arc<dyn PluginRpcClient>,
        enforcer: Arc<CapabilityEnforcer>,
    ) -> Self {
        Self {
            name: name.into(),
            client,
            enforcer,
        }
    }
}

#[async_trait]
impl<Context> CommandHandler<Context, Value> for RpcCommandAdapter
where
    Context: Send,
{
    async fn execute(
        &self,
        _context: &mut Context,
        invocation: CommandInvocation,
    ) -> Result<Value, CommandExecutionError> {
        self.enforcer.check_command(&self.name).map_err(|error| {
            CommandExecutionError::new("capability_violation", error.to_string())
        })?;
        self.client
            .request(
                METHOD_COMMAND_EXECUTE,
                serde_json::to_value(CommandExecuteParams {
                    name: self.name.clone(),
                    arguments: invocation.arguments().to_owned(),
                })
                .map_err(|error| {
                    CommandExecutionError::new("invalid_request", error.to_string())
                })?,
            )
            .await
            .map_err(|error| CommandExecutionError::new(error.code, error.message))
    }
}

pub struct RpcProviderAdapter {
    name: String,
    alias_prefix: String,
    capabilities: Capabilities,
    client: Arc<dyn PluginRpcClient>,
    enforcer: Arc<CapabilityEnforcer>,
    model_catalog: bool,
    catalog_cache: StdRwLock<RpcProviderCatalogCache>,
}

#[derive(Clone, Debug, Default)]
struct RpcProviderCatalogCache {
    catalog: Option<DiscoveredProviderCatalog>,
    aggregate_capabilities: Option<Capabilities>,
    single_model_metadata: Option<ProviderModelMetadata>,
    metadata_by_model: BTreeMap<String, ProviderModelMetadata>,
}

impl RpcProviderAdapter {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        alias_prefix: impl Into<String>,
        capabilities: Capabilities,
        client: Arc<dyn PluginRpcClient>,
        enforcer: Arc<CapabilityEnforcer>,
    ) -> Self {
        Self {
            name: name.into(),
            alias_prefix: alias_prefix.into(),
            capabilities,
            client,
            enforcer,
            model_catalog: false,
            catalog_cache: StdRwLock::new(RpcProviderCatalogCache::default()),
        }
    }

    /// Enables protocol-2 model discovery for an approval-fingerprinted provider declaration.
    #[must_use]
    pub fn with_model_catalog(mut self) -> Self {
        self.model_catalog = true;
        self
    }

    fn cached_capabilities(&self) -> Option<Capabilities> {
        self.catalog_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .aggregate_capabilities
            .clone()
    }

    #[allow(
        clippy::too_many_lines,
        reason = "catalog validation keeps the complete untrusted wire boundary visible"
    )]
    fn parse_catalog(&self, value: Value) -> Result<RpcProviderCatalogCache, ProviderError> {
        let response: ProviderModelsResponse = serde_json::from_value(value).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                "plugin returned an invalid provider model catalog",
            )
        })?;
        if response.models.len() > rw_plugin_protocol::MAX_CAPABILITIES_PER_KIND {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "plugin provider model catalog exceeds the entry limit",
            ));
        }
        let mut ids = BTreeSet::new();
        let mut models = Vec::with_capacity(response.models.len());
        let mut metadata = Vec::with_capacity(response.models.len());
        let mut metadata_by_model = BTreeMap::new();
        for model in response.models {
            if model.id.is_empty()
                || model.id.len() > MAX_NAME_BYTES
                || model.id.chars().any(char::is_control)
                || !ids.insert(model.id.clone())
            {
                return Err(ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "plugin provider model id is invalid or duplicated",
                ));
            }
            if model.display_name.as_ref().is_some_and(|name| {
                name.is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control)
            }) {
                return Err(ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "plugin provider model display name is invalid",
                ));
            }
            let max_context_tokens = model
                .max_context_tokens
                .map(|limit| limit.clamp(1, MAX_PLUGIN_MODEL_TOKENS));
            let max_output_tokens = model
                .max_output_tokens
                .map(|limit| limit.clamp(1, MAX_PLUGIN_MODEL_TOKENS));
            let capabilities = Capabilities {
                tool_calling: model.capabilities.tool_calling,
                vision: model.capabilities.vision,
                thinking: model.capabilities.thinking,
                cache_breakpoints: match model.capabilities.cache_breakpoints {
                    ProviderCacheBreakpoints::None => CacheBreakpointSupport::None,
                    ProviderCacheBreakpoints::Explicit => CacheBreakpointSupport::Explicit,
                    ProviderCacheBreakpoints::Automatic => CacheBreakpointSupport::Automatic,
                },
                max_context_tokens,
                max_output_tokens,
                wire_mode: WireMode::NormalizedReplay,
            };
            let pricing = model.pricing.map(|pricing| ModelPricing {
                display_name: model
                    .display_name
                    .clone()
                    .unwrap_or_else(|| model.id.clone()),
                max_context_tokens,
                max_output_tokens,
                supports_tools: capabilities.tool_calling,
                supports_thinking: capabilities.thinking,
                supports_vision: capabilities.vision,
                reasoning_efforts: Vec::new(),
                input_per_million_micros_usd: pricing
                    .input_per_million_micros_usd
                    .min(MAX_PLUGIN_PRICE_MICROS_USD),
                output_per_million_micros_usd: pricing
                    .output_per_million_micros_usd
                    .min(MAX_PLUGIN_PRICE_MICROS_USD),
                cache_read_per_million_micros_usd: pricing
                    .cache_read_per_million_micros_usd
                    .map(|price| price.min(MAX_PLUGIN_PRICE_MICROS_USD)),
                cache_write_per_million_micros_usd: pricing
                    .cache_write_per_million_micros_usd
                    .map(|price| price.min(MAX_PLUGIN_PRICE_MICROS_USD)),
                reasoning_per_million_micros_usd: pricing
                    .reasoning_per_million_micros_usd
                    .map(|price| price.min(MAX_PLUGIN_PRICE_MICROS_USD)),
            });
            let model_metadata = ProviderModelMetadata {
                capabilities: capabilities.clone(),
                accounting: if pricing.is_some() {
                    UsageAccounting::ApiDollars
                } else {
                    UsageAccounting::UnpricedApi
                },
                pricing: pricing.clone(),
            };
            metadata_by_model.insert(model.id.clone(), model_metadata.clone());
            metadata.push(model_metadata);
            models.push(DiscoveredModel {
                id: model.id,
                display_name: model.display_name,
                description: None,
                capabilities: Some(capabilities),
                pricing,
            });
        }
        let aggregate_capabilities = aggregate_plugin_capabilities(&metadata, &self.capabilities);
        Ok(RpcProviderCatalogCache {
            catalog: Some(DiscoveredProviderCatalog {
                provider: self.alias_prefix.trim_end_matches('/').to_owned(),
                models,
            }),
            aggregate_capabilities: Some(aggregate_capabilities),
            single_model_metadata: (metadata.len() == 1).then(|| metadata.remove(0)),
            metadata_by_model,
        })
    }
}

fn aggregate_plugin_capabilities(
    metadata: &[ProviderModelMetadata],
    fallback: &Capabilities,
) -> Capabilities {
    let Some(first) = metadata.first() else {
        return fallback.clone();
    };
    Capabilities {
        tool_calling: metadata.iter().all(|entry| entry.capabilities.tool_calling),
        vision: metadata.iter().all(|entry| entry.capabilities.vision),
        thinking: metadata.iter().all(|entry| entry.capabilities.thinking),
        cache_breakpoints: if metadata.iter().all(|entry| {
            entry.capabilities.cache_breakpoints == first.capabilities.cache_breakpoints
        }) {
            first.capabilities.cache_breakpoints
        } else {
            CacheBreakpointSupport::None
        },
        max_context_tokens: common_plugin_limit(metadata, |entry| {
            entry.capabilities.max_context_tokens
        }),
        max_output_tokens: common_plugin_limit(metadata, |entry| {
            entry.capabilities.max_output_tokens
        }),
        wire_mode: WireMode::NormalizedReplay,
    }
}

fn common_plugin_limit(
    metadata: &[ProviderModelMetadata],
    get: impl Fn(&ProviderModelMetadata) -> Option<u64>,
) -> Option<u64> {
    metadata.iter().try_fold(u64::MAX, |minimum, entry| {
        get(entry).map(|limit| minimum.min(limit))
    })
}

#[async_trait]
impl Provider for RpcProviderAdapter {
    fn name(&self) -> &str {
        &self.name
    }
    fn capabilities(&self) -> Capabilities {
        self.cached_capabilities()
            .unwrap_or_else(|| self.capabilities.clone())
    }
    async fn model_metadata(&self) -> Result<Option<ProviderModelMetadata>, ProviderError> {
        if let Some(metadata) = self.cached_model_metadata() {
            return Ok(Some(metadata));
        }
        let _ = self.discover_models().await?;
        Ok(self.cached_model_metadata())
    }
    fn cached_model_metadata(&self) -> Option<ProviderModelMetadata> {
        self.catalog_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .single_model_metadata
            .clone()
    }
    fn cached_model_metadata_for(&self, model: &str) -> Option<ProviderModelMetadata> {
        self.catalog_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .metadata_by_model
            .get(model)
            .cloned()
    }
    async fn discover_models(&self) -> Result<Option<DiscoveredProviderCatalog>, ProviderError> {
        if !self.model_catalog {
            return Ok(None);
        }
        self.enforcer
            .check_provider(&format!("{}catalog", self.alias_prefix))
            .map_err(|error| {
                ProviderError::new(ProviderErrorKind::Unsupported, error.to_string())
            })?;
        let value = self
            .client
            .request(
                METHOD_PROVIDER_MODELS,
                serde_json::to_value(ProviderModelsParams {
                    alias_prefix: self.alias_prefix.clone(),
                })
                .map_err(|error| {
                    ProviderError::new(ProviderErrorKind::Protocol, error.to_string())
                })?,
            )
            .await
            .map_err(|error| ProviderError::new(ProviderErrorKind::Protocol, error.to_string()))?;
        let cache = self.parse_catalog(value)?;
        let catalog = cache.catalog.clone();
        *self
            .catalog_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cache;
        Ok(catalog)
    }
    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        let alias = format!("{}{}", self.alias_prefix, request.model);
        self.enforcer.check_provider(&alias).map_err(|error| {
            ProviderError::new(ProviderErrorKind::Unsupported, error.to_string())
        })?;
        let events = self
            .client
            .provider_stream(
                serde_json::to_value(ProviderCompleteParams {
                    alias,
                    request: serde_json::to_value(request).map_err(|error| {
                        ProviderError::new(ProviderErrorKind::Protocol, error.to_string())
                    })?,
                })
                .map_err(|error| {
                    ProviderError::new(ProviderErrorKind::Protocol, error.to_string())
                })?,
            )
            .await
            .map_err(|error| provider_rpc_error(&error))?;
        Ok(Box::pin(events.map(|event| {
            let value = event.map_err(|error| provider_rpc_error(&error))?;
            serde_json::from_value(value).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "plugin returned an invalid provider event",
                )
            })
        })))
    }
}

fn provider_rpc_error(error: &PluginRpcError) -> ProviderError {
    let kind = match error.code.as_str() {
        "provider_http_authentication" | "authentication" => ProviderErrorKind::Authentication,
        "provider_http_rate_limited" => ProviderErrorKind::RateLimited,
        "provider_http_timeout" => ProviderErrorKind::Timeout,
        "provider_http_server" => ProviderErrorKind::Server,
        "provider_http_network" => ProviderErrorKind::Network,
        "provider_http_network_disabled" => ProviderErrorKind::NetworkDisabled,
        "provider_http_cancelled" | "cancelled" => ProviderErrorKind::Cancelled,
        "provider_http_invalid_request" | "invalid_request" | "domain_denied" => {
            ProviderErrorKind::InvalidRequest
        }
        _ => ProviderErrorKind::Protocol,
    };
    ProviderError::new(kind, error.to_string())
}

pub struct PluginEventRouter {
    client: Arc<dyn PluginRpcClient>,
    enforcer: Arc<CapabilityEnforcer>,
}

impl PluginEventRouter {
    #[must_use]
    pub fn new(client: Arc<dyn PluginRpcClient>, enforcer: Arc<CapabilityEnforcer>) -> Self {
        Self { client, enforcer }
    }
    /// Publishes an event only when it appears in the immutable subscription snapshot.
    ///
    /// # Errors
    ///
    /// Returns an RPC error for an undeclared event or failed notification delivery.
    pub async fn publish(&self, event: &str, payload: Value) -> Result<(), PluginRpcError> {
        self.enforcer
            .check_event(event)
            .map_err(|error| rpc_error("capability_violation", &error.to_string()))?;
        self.client
            .notify(
                METHOD_EVENT_PUBLISH,
                serde_json::to_value(EventPublishParams {
                    event: event.to_owned(),
                    payload,
                })
                .map_err(|error| rpc_error("invalid_request", &error.to_string()))?,
            )
            .await
    }
}

#[cfg(test)]
pub(crate) struct TestDirectLauncher;

#[cfg(test)]
struct TestChildProcess {
    child: StdMutex<tokio::process::Child>,
    pid: Option<u32>,
}

#[cfg(test)]
#[async_trait]
impl SupervisedPluginProcess for TestChildProcess {
    fn mark_capability_violation(&self, _violation: &crate::plugin::CapabilityViolation) {}
    fn kill_tree(&self) -> Result<(), PluginProcessError> {
        #[cfg(unix)]
        if let Some(pid) = self
            .pid
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        self.child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .start_kill()
            .map_err(|error| PluginProcessError {
                message: error.to_string(),
            })
    }
    async fn wait(&self) -> Result<Option<i32>, PluginProcessError> {
        loop {
            if let Some(status) = self
                .child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .try_wait()
                .map_err(|error| PluginProcessError {
                    message: error.to_string(),
                })?
            {
                return Ok(status.code());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[cfg(test)]
#[async_trait]
impl PluginLauncher for TestDirectLauncher {
    async fn launch(
        &self,
        config: &PluginProcessConfig,
        _profile: &PluginSandboxProfile,
    ) -> Result<LaunchedPluginProcess, PluginProcessError> {
        use std::os::unix::process::CommandExt;
        config.validate_executable_identity()?;
        let mut command = tokio::process::Command::new(config.executable());
        command
            .args(config.argv())
            .current_dir(config.cwd())
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for name in config.environment_allowlist() {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command.as_std_mut().process_group(0);
        let mut child = command.spawn().map_err(|error| PluginProcessError {
            message: error.to_string(),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| PluginProcessError {
            message: "missing stdin".to_owned(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| PluginProcessError {
            message: "missing stdout".to_owned(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| PluginProcessError {
            message: "missing stderr".to_owned(),
        })?;
        let pid = child.id();
        Ok(LaunchedPluginProcess {
            stdin: Box::pin(stdin),
            stdout: Box::pin(BufReader::new(stdout)),
            stderr: Box::pin(BufReader::new(stderr)),
            process: Arc::new(TestChildProcess {
                child: StdMutex::new(child),
                pid,
            }),
            executable_identity: config.executable_identity().clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicUsize;

    use futures_util::StreamExt;
    use rw_providers::{CacheBreakpointSupport, ToolChoice, WireMode};
    use rw_tools::ToolRegistry;
    use rw_types::config::ThinkingLevel;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;
    use crate::plugin::{ApprovalStoreError, CapabilityViolation};
    use rw_plugin_protocol::{
        PluginCommandCapability, PluginHookCapability, PluginHookFailurePolicy,
        PluginProviderCapability, PluginPush, PluginToolCapability, PluginToolEffect,
    };

    fn manifest() -> PluginManifest {
        PluginManifest {
            name: "runtime-fixture".to_owned(),
            version: "1.0.0".to_owned(),
            protocol: rw_plugin_protocol::MIN_PROTOCOL_VERSION,
            capabilities: PluginCapabilities {
                tools: vec![PluginToolCapability {
                    name: "fixture_tool".to_owned(),
                    description: "fixture tool".to_owned(),
                    schema: json!({"type":"object"}),
                    caps: vec![PluginToolEffect::ReadsFilesystem],
                }],
                commands: vec![PluginCommandCapability {
                    name: "fixture".to_owned(),
                    description: "fixture command".to_owned(),
                    argument_hint: None,
                    allowed_tools: Vec::new(),
                }],
                hooks: vec![PluginHookCapability {
                    name: rw_plugin_protocol::PluginHook::PreTool,
                    failure_policy: PluginHookFailurePolicy::FailOpen,
                }],
                providers: vec![PluginProviderCapability {
                    alias_prefix: "fixture/".to_owned(),
                    capabilities: Vec::new(),
                    credential_references: Vec::new(),
                }],
                event_subscriptions: vec!["TurnFinished".to_owned()],
                push: vec![PluginPush::UiNotify],
            },
        }
    }

    struct CatalogClient(Value);

    #[async_trait]
    impl PluginRpcClient for CatalogClient {
        async fn request(&self, method: &str, params: Value) -> Result<Value, PluginRpcError> {
            assert_eq!(method, METHOD_PROVIDER_MODELS);
            assert_eq!(params, json!({"alias_prefix":"fixture/"}));
            Ok(self.0.clone())
        }
    }

    fn catalog_adapter(value: Value) -> RpcProviderAdapter {
        let mut approved = manifest();
        approved.protocol = rw_plugin_protocol::PROTOCOL_VERSION;
        approved.capabilities.providers[0].capabilities = vec!["models".to_owned()];
        let process: Arc<dyn SupervisedPluginProcess> = Arc::new(FakeProcess::default());
        let enforcer = Arc::new(CapabilityEnforcer::new(&approved, process));
        RpcProviderAdapter::new(
            "catalog-fixture",
            "fixture/",
            Capabilities {
                tool_calling: true,
                vision: false,
                thinking: false,
                cache_breakpoints: CacheBreakpointSupport::None,
                max_context_tokens: None,
                max_output_tokens: None,
                wire_mode: WireMode::NormalizedReplay,
            },
            Arc::new(CatalogClient(value)),
            enforcer,
        )
        .with_model_catalog()
    }

    #[tokio::test]
    async fn protocol_two_provider_catalog_is_bounded_and_cached() {
        let provider = catalog_adapter(json!({"models":[{
            "id":"capable",
            "display_name":"Capable",
            "capabilities":{
                "tool_calling":true,
                "vision":true,
                "thinking":true,
                "cache_breakpoints":"explicit"
            },
            "max_context_tokens":u64::MAX,
            "max_output_tokens":0,
            "pricing":{
                "input_per_million_micros_usd":u64::MAX,
                "output_per_million_micros_usd":15_000_000
            }
        }]}));
        let catalog = provider
            .discover_models()
            .await
            .expect("valid bounded catalog")
            .expect("protocol 2 catalog");
        assert_eq!(catalog.provider, "fixture");
        assert_eq!(catalog.models[0].id, "capable");
        let capabilities = provider.capabilities();
        assert!(capabilities.vision);
        assert!(capabilities.thinking);
        assert_eq!(
            capabilities.cache_breakpoints,
            CacheBreakpointSupport::Explicit
        );
        assert_eq!(
            capabilities.max_context_tokens,
            Some(MAX_PLUGIN_MODEL_TOKENS)
        );
        assert_eq!(capabilities.max_output_tokens, Some(1));
        let metadata = provider
            .cached_model_metadata()
            .expect("single-model metadata cache");
        assert_eq!(metadata.accounting, UsageAccounting::ApiDollars);
        assert_eq!(
            metadata
                .pricing
                .expect("catalog pricing")
                .input_per_million_micros_usd,
            MAX_PLUGIN_PRICE_MICROS_USD
        );
    }

    #[tokio::test]
    async fn protocol_two_catalog_caches_metadata_per_model() {
        let provider = catalog_adapter(json!({"models":[{
            "id":"text-only",
            "capabilities":{
                "tool_calling":false,"vision":false,"thinking":false,"cache_breakpoints":"none"
            },
            "pricing":{
                "input_per_million_micros_usd":1_000_000,
                "output_per_million_micros_usd":2_000_000
            }
        },{
            "id":"vision-thinking",
            "capabilities":{
                "tool_calling":true,"vision":true,"thinking":true,"cache_breakpoints":"explicit"
            },
            "pricing":{
                "input_per_million_micros_usd":3_000_000,
                "output_per_million_micros_usd":4_000_000
            }
        }]}));
        provider
            .discover_models()
            .await
            .expect("valid multi-model catalog");

        assert!(provider.cached_model_metadata().is_none());
        let text = provider
            .cached_model_metadata_for("text-only")
            .expect("text model metadata");
        assert!(!text.capabilities.tool_calling);
        assert!(!text.capabilities.vision);
        assert_eq!(
            text.pricing
                .expect("text pricing")
                .input_per_million_micros_usd,
            1_000_000
        );
        let vision = provider
            .cached_model_metadata_for("vision-thinking")
            .expect("vision model metadata");
        assert!(vision.capabilities.tool_calling);
        assert!(vision.capabilities.vision);
        assert!(vision.capabilities.thinking);
        assert_eq!(
            vision
                .pricing
                .expect("vision pricing")
                .input_per_million_micros_usd,
            3_000_000
        );
        assert!(provider.cached_model_metadata_for("missing").is_none());
    }

    #[tokio::test]
    async fn malformed_provider_catalog_degrades_only_that_adapter() {
        let provider = catalog_adapter(json!({"models":[{
            "id":"duplicate",
            "capabilities":{
                "tool_calling":true,"vision":false,"thinking":false,"cache_breakpoints":"none"
            }
        },{
            "id":"duplicate",
            "capabilities":{
                "tool_calling":true,"vision":false,"thinking":false,"cache_breakpoints":"none"
            }
        }]}));
        let error = provider
            .discover_models()
            .await
            .expect_err("duplicate model ids must fail discovery");
        assert_eq!(error.kind, ProviderErrorKind::Protocol);
        assert!(provider.cached_model_metadata().is_none());
        assert_eq!(
            provider.capabilities(),
            Capabilities {
                tool_calling: true,
                vision: false,
                thinking: false,
                cache_breakpoints: CacheBreakpointSupport::None,
                max_context_tokens: None,
                max_output_tokens: None,
                wire_mode: WireMode::NormalizedReplay,
            }
        );
    }

    #[derive(Default)]
    struct MemoryApproval(StdMutex<BTreeMap<String, String>>);

    impl ApprovalStore for MemoryApproval {
        fn approved_fingerprint(&self, name: &str) -> Result<Option<String>, ApprovalStoreError> {
            Ok(self.0.lock().expect("approval lock").get(name).cloned())
        }
        fn record_approval(&self, name: &str, fingerprint: &str) -> Result<(), ApprovalStoreError> {
            self.0
                .lock()
                .expect("approval lock")
                .insert(name.to_owned(), fingerprint.to_owned());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeProcess {
        killed: AtomicUsize,
        waited: AtomicUsize,
        violations: StdMutex<Vec<CapabilityViolation>>,
        kill_fails: AtomicBool,
    }

    #[async_trait]
    impl SupervisedPluginProcess for FakeProcess {
        fn mark_capability_violation(&self, violation: &CapabilityViolation) {
            self.violations
                .lock()
                .expect("violation lock")
                .push(violation.clone());
        }
        fn kill_tree(&self) -> Result<(), PluginProcessError> {
            self.killed.fetch_add(1, Ordering::AcqRel);
            if self.kill_fails.load(Ordering::Acquire) {
                Err(PluginProcessError {
                    message: "seeded kill failure".to_owned(),
                })
            } else {
                Ok(())
            }
        }
        async fn wait(&self) -> Result<Option<i32>, PluginProcessError> {
            self.waited.fetch_add(1, Ordering::AcqRel);
            Ok(Some(0))
        }
    }

    struct MemoryLauncher {
        manifest: PluginManifest,
        process: Arc<FakeProcess>,
        push: Option<String>,
        hang_method: Option<String>,
    }

    #[derive(Default)]
    struct TrackingDirectLauncher(StdMutex<Option<Arc<dyn SupervisedPluginProcess>>>);

    #[async_trait]
    impl PluginLauncher for TrackingDirectLauncher {
        async fn launch(
            &self,
            config: &PluginProcessConfig,
            profile: &PluginSandboxProfile,
        ) -> Result<LaunchedPluginProcess, PluginProcessError> {
            let launched = TestDirectLauncher.launch(config, profile).await?;
            *self.0.lock().expect("tracking launcher") = Some(Arc::clone(&launched.process));
            Ok(launched)
        }
    }

    #[derive(Default)]
    struct RecordingPush(StdMutex<Vec<(String, Value)>>);

    #[async_trait]
    impl PushHandler for RecordingPush {
        async fn handle_push(&self, method: &str, params: Value) -> Result<Value, PluginRpcError> {
            self.0
                .lock()
                .expect("push lock")
                .push((method.to_owned(), params));
            Ok(Value::Null)
        }
    }

    struct CanaryRedactor;

    impl PluginBoundaryRedactor for CanaryRedactor {
        fn redact(&self, mut value: Value) -> Value {
            fn visit(value: &mut Value) {
                match value {
                    Value::String(text) => {
                        *text = text.replace("PLUGIN_CANARY_SECRET", "[REDACTED]");
                    }
                    Value::Array(values) => values.iter_mut().for_each(visit),
                    Value::Object(values) => values.values_mut().for_each(visit),
                    Value::Null | Value::Bool(_) | Value::Number(_) => {}
                }
            }
            visit(&mut value);
            value
        }
    }

    const HTTP_SECRET: &str = "PLUGIN_HTTP_SECRET_CANARY";

    struct HttpSecretRedactor;

    impl PluginBoundaryRedactor for HttpSecretRedactor {
        fn redact(&self, mut value: Value) -> Value {
            fn visit(value: &mut Value) {
                match value {
                    Value::String(text) => *text = text.replace(HTTP_SECRET, "[REDACTED]"),
                    Value::Array(values) => values.iter_mut().for_each(visit),
                    Value::Object(values) => values.values_mut().for_each(visit),
                    Value::Null | Value::Bool(_) | Value::Number(_) => {}
                }
            }
            visit(&mut value);
            value
        }

        fn redact_bytes(&self, value: &[u8]) -> Vec<u8> {
            String::from_utf8_lossy(value)
                .replace(HTTP_SECRET, "[REDACTED]")
                .into_bytes()
        }

        fn redact_streaming_prefix(&self, value: &[u8], retain: usize) -> (Vec<u8>, Vec<u8>) {
            let redactor = rw_providers::FixtureRedactor::new([HTTP_SECRET.to_owned()]);
            redactor.redact_streaming_prefix(value, retain)
        }

        fn maximum_secret_bytes(&self) -> usize {
            HTTP_SECRET.len()
        }
    }

    #[derive(Default)]
    struct FixtureProviderHttp {
        requests: StdMutex<Vec<Value>>,
        cancelled: Arc<AtomicBool>,
    }

    #[async_trait]
    impl PluginProviderHttpHandler for FixtureProviderHttp {
        async fn request(
            &self,
            params: Value,
            cancellation: &CancellationToken,
        ) -> Result<PluginHttpStreamResponse, PluginRpcError> {
            if cancellation.is_cancelled() {
                return Err(rpc_error(
                    "cancelled",
                    "fixture provider HTTP was cancelled",
                ));
            }
            self.requests.lock().expect("request lock").push(params);
            let is_cancellation_fixture = self
                .requests
                .lock()
                .expect("request lock")
                .last()
                .and_then(|params| params.pointer("/request/url"))
                .and_then(Value::as_str)
                .is_some_and(|url| url.ends_with("/cancelled"));
            if is_cancellation_fixture {
                let cancellation = cancellation.clone();
                let cancelled = Arc::clone(&self.cancelled);
                tokio::spawn(async move {
                    cancellation.cancelled().await;
                    cancelled.store(true, Ordering::Release);
                });
                return Ok(PluginHttpStreamResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: Box::pin(futures_util::stream::pending()),
                });
            }
            let wire = format!(
                "{{\"type\":\"message_start\",\"model\":\"tool-model\"}}\n\
                 {{\"type\":\"tool_call_start\",\"id\":\"call-1\",\"name\":\"lookup\"}}\n\
                 {{\"type\":\"tool_call_arguments_delta\",\"id\":\"call-1\",\"json_fragment\":\"{{\\\"city\\\":\\\"Chicago\\\"}}\"}}\n\
                 {{\"type\":\"tool_call_end\",\"id\":\"call-1\",\"arguments\":{{\"city\":\"Chicago\"}}}}\n\
                 {{\"type\":\"text_delta\",\"text\":\"{HTTP_SECRET}\"}}\n\
                 {{\"type\":\"finished\",\"reason\":\"tool_calls\"}}\n"
            );
            let split = wire.find(HTTP_SECRET).expect("secret marker") + 8;
            let chunks = vec![
                Ok(wire.as_bytes()[..split].to_vec()),
                Ok(wire.as_bytes()[split..].to_vec()),
            ];
            Ok(PluginHttpStreamResponse {
                status: 200,
                headers: vec![("x-echo".to_owned(), HTTP_SECRET.to_owned())],
                body: Box::pin(futures_util::stream::iter(chunks)),
            })
        }
    }

    #[tokio::test]
    async fn provider_http_redaction_retains_overlap_after_an_earlier_match() {
        let partial = 8;
        let first = format!("prefix {HTTP_SECRET} {}", &HTTP_SECRET[..partial]);
        let second = format!("{} suffix", &HTTP_SECRET[partial..]);
        let mut body: PluginHttpByteStream = Box::pin(futures_util::stream::iter([
            Ok(first.into_bytes()),
            Ok(second.into_bytes()),
        ]));
        let (writer, mut receiver) = mpsc::channel(WRITER_QUEUE_CAPACITY);

        stream_provider_http_body(
            &RpcId::String("stream-redaction".to_owned()),
            &mut body,
            &CancellationToken::default(),
            &writer,
            &HttpSecretRedactor,
        )
        .await
        .expect("stream redaction");
        drop(writer);

        let mut rendered = Vec::new();
        while let Some(frame) = receiver.recv().await {
            let RpcFrame::Notification(notification) = frame else {
                panic!("provider HTTP body must emit notifications");
            };
            let params = notification.params.expect("notification params");
            if params.pointer("/event/type").and_then(Value::as_str) == Some("body") {
                let encoded = params
                    .pointer("/event/data_base64")
                    .and_then(Value::as_str)
                    .expect("encoded body");
                rendered.extend(BASE64_STANDARD.decode(encoded).expect("valid body base64"));
            }
        }
        assert_eq!(
            String::from_utf8(rendered).expect("UTF-8 fixture"),
            "prefix [REDACTED] [REDACTED] suffix"
        );
    }

    #[async_trait]
    impl PluginLauncher for MemoryLauncher {
        async fn launch(
            &self,
            config: &PluginProcessConfig,
            profile: &PluginSandboxProfile,
        ) -> Result<LaunchedPluginProcess, PluginProcessError> {
            config.validate_executable_identity()?;
            assert_eq!(profile.capabilities, self.manifest.capabilities);
            let (host_stdin, plugin_input) = tokio::io::duplex(64 * 1024);
            let (plugin_output, host_stdout) = tokio::io::duplex(64 * 1024);
            let manifest = self.manifest.clone();
            let push = self.push.clone();
            let hang_method = self.hang_method.clone();
            tokio::spawn(async move {
                let mut input = BufReader::new(plugin_input);
                let mut output = plugin_output;
                let mut line = String::new();
                while input.read_line(&mut line).await.expect("fixture read") != 0 {
                    let frame: RpcFrame =
                        serde_json::from_str(line.trim_end()).expect("host frame");
                    line.clear();
                    match frame {
                        RpcFrame::Request(request) if request.method == METHOD_INITIALIZE => {
                            if let Some(method) = push.as_deref() {
                                let push = RpcFrame::Request(RpcRequest {
                                    jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                                    id: RpcId::String("push-1".to_owned()),
                                    method: method.to_owned(),
                                    params: Some(json!({"message":"hello"})),
                                });
                                output
                                    .write_all(
                                        &encode_frame(&push, MAX_FRAME_BYTES).expect("push frame"),
                                    )
                                    .await
                                    .expect("push write");
                            }
                            let response = RpcFrame::Success(RpcSuccess {
                                jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                                id: Some(request.id),
                                result: serde_json::to_value(&manifest).expect("manifest"),
                            });
                            output
                                .write_all(
                                    &encode_frame(&response, MAX_FRAME_BYTES)
                                        .expect("response frame"),
                                )
                                .await
                                .expect("response write");
                        }
                        RpcFrame::Request(request)
                            if hang_method.as_deref() == Some(&request.method) => {}
                        RpcFrame::Request(request) if request.method == METHOD_TOOL_CALL => {
                            let response = RpcFrame::Success(RpcSuccess {
                                jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                                id: Some(request.id),
                                result: serde_json::to_value(ToolResult::new(
                                    "fixture",
                                    json!({"ok":true}),
                                ))
                                .expect("tool result"),
                            });
                            output
                                .write_all(
                                    &encode_frame(&response, MAX_FRAME_BYTES)
                                        .expect("response frame"),
                                )
                                .await
                                .expect("response write");
                        }
                        RpcFrame::Request(request) if request.method == METHOD_SHUTDOWN => {
                            let response = RpcFrame::Success(RpcSuccess {
                                jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                                id: Some(request.id),
                                result: Value::Null,
                            });
                            output
                                .write_all(
                                    &encode_frame(&response, MAX_FRAME_BYTES)
                                        .expect("response frame"),
                                )
                                .await
                                .expect("response write");
                        }
                        RpcFrame::Notification(notification)
                            if notification.method == METHOD_EXIT =>
                        {
                            break;
                        }
                        _ => {}
                    }
                }
            });
            Ok(LaunchedPluginProcess {
                stdin: Box::pin(host_stdin),
                stdout: Box::pin(BufReader::new(host_stdout)),
                stderr: Box::pin(BufReader::new(tokio::io::empty())),
                process: self.process.clone(),
                executable_identity: config.executable_identity().clone(),
            })
        }
    }

    fn shell_config(root: &TempDir) -> PluginProcessConfig {
        PluginProcessConfig::new(PathBuf::from("/bin/sh"))
            .expect("shell config")
            .with_cwd(root.path())
            .expect("cwd")
    }

    #[test]
    fn null_or_missing_response_ids_decode_for_json_rpc_error_compatibility() {
        let mut decoder = FrameDecoder::default();
        let frames = decoder
            .push(b"{\"jsonrpc\":\"2.0\",\"result\":null}\n")
            .expect("response frame");
        assert!(matches!(
            frames.as_slice(),
            [RpcFrame::Success(RpcSuccess { id: None, .. })]
        ));
    }

    #[test]
    fn environment_is_a_small_safe_allowlist() {
        let config = PluginProcessConfig::new(PathBuf::from("/bin/sh")).expect("shell");
        assert!(
            config
                .clone()
                .with_environment_allowlist(["LANG", "TERM"])
                .is_ok()
        );
        assert!(matches!(
            config
                .clone()
                .with_environment_allowlist(["OPENAI_API_KEY"]),
            Err(crate::plugin::PluginProcessConfigError::UnsafeEnvironmentName)
        ));
        assert!(matches!(
            config.with_environment_allowlist(["DYLD_INSERT_LIBRARIES"]),
            Err(crate::plugin::PluginProcessConfigError::UnsafeEnvironmentName)
        ));
    }

    #[test]
    fn direct_executable_approval_identity_has_no_source_projection() {
        let config = PluginProcessConfig::new(PathBuf::from("/bin/sh")).expect("shell");
        let identity = approval_identity(&manifest(), &config, "user:fixture").expect("identity");
        let serialized = serde_json::to_value(identity).expect("approval identity JSON");
        assert!(serialized.get("source").is_none());
    }

    #[test]
    fn executable_substitution_invalidates_identity_and_approval() {
        use std::os::unix::fs::PermissionsExt;
        let root = TempDir::new().expect("tempdir");
        let executable = root.path().join("plugin");
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").expect("write executable");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .expect("chmod");
        let config = PluginProcessConfig::new(&executable)
            .expect("config")
            .with_cwd(root.path())
            .expect("cwd");
        let store = MemoryApproval::default();
        approve_plugin_launch(&store, &manifest(), &config, "project:test").expect("approve");
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, b"#!/bin/sh\nexit 1\n").expect("replacement");
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o700))
            .expect("chmod replacement");
        std::fs::rename(&replacement, &executable).expect("replace executable");
        assert!(config.validate_executable_identity().is_err());
    }

    #[test]
    fn interpreted_entrypoint_and_lock_mutation_require_rediscovery_and_reapproval() {
        let root = TempDir::new().expect("tempdir");
        let entrypoint = root.path().join("plugin.js");
        let lock = root.path().join("bun.lock");
        std::fs::write(&entrypoint, "console.log('one')\n").expect("entrypoint");
        std::fs::write(&lock, "lock-v1\n").expect("lock");
        let configured = || {
            PluginProcessConfig::new(PathBuf::from("/bin/sh"))
                .expect("shell")
                .with_argv([entrypoint.clone().into_os_string()])
                .expect("argv")
                .with_cwd(root.path())
                .expect("cwd")
                .with_attested_files([entrypoint.clone(), lock.clone()])
                .expect("attestation")
        };
        let original = configured();
        let store = MemoryApproval::default();
        approve_plugin_launch(&store, &manifest(), &original, "project:interpreted")
            .expect("approve");

        std::fs::write(&entrypoint, "console.log('two')\n").expect("mutate entrypoint");
        assert!(original.validate_executable_identity().is_err());
        let rediscovered = configured();
        assert!(matches!(
            plugin_launch_approval_requirement(
                &store,
                &manifest(),
                &rediscovered,
                "project:interpreted"
            )
            .expect("requirement"),
            ApprovalRequirement::ManifestChanged { .. }
        ));

        approve_plugin_launch(&store, &manifest(), &rediscovered, "project:interpreted")
            .expect("reapprove entrypoint");
        std::fs::write(&lock, "lock-v2\n").expect("mutate lock");
        assert!(rediscovered.validate_executable_identity().is_err());
        assert!(matches!(
            plugin_launch_approval_requirement(
                &store,
                &manifest(),
                &configured(),
                "project:interpreted"
            )
            .expect("lock requirement"),
            ApprovalRequirement::ManifestChanged { .. }
        ));
    }

    #[test]
    fn oversized_attested_file_is_rejected_before_hashing() {
        let root = TempDir::new().expect("tempdir");
        let oversized = root.path().join("oversized.lock");
        let file = std::fs::File::create(&oversized).expect("file");
        file.set_len(256 * 1024 * 1024 + 1).expect("sparse length");
        assert!(matches!(
            PluginProcessConfig::new(PathBuf::from("/bin/sh"))
                .expect("shell")
                .with_attested_files([oversized]),
            Err(crate::plugin::PluginProcessConfigError::AttestationLimit)
        ));
    }

    #[test]
    fn code_root_rejects_escape_symlink_and_directory_replacement() {
        let root = TempDir::new().expect("tempdir");
        let code = root.path().join("code");
        std::fs::create_dir(&code).expect("code root");
        let entrypoint = code.join("plugin.js");
        let escaped = root.path().join("escaped.js");
        std::fs::write(&entrypoint, "export {}\n").expect("entrypoint");
        std::fs::write(&escaped, "export {}\n").expect("escaped");
        let config = PluginProcessConfig::new(PathBuf::from("/bin/sh"))
            .expect("shell")
            .with_code_root(&code)
            .expect("code root")
            .with_attested_files([entrypoint])
            .expect("contained attestation");
        assert!(
            PluginProcessConfig::new(PathBuf::from("/bin/sh"))
                .expect("shell")
                .with_code_root(&code)
                .expect("code root")
                .with_attested_files([escaped])
                .is_err()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&code, root.path().join("code-link")).expect("symlink");
            assert!(
                PluginProcessConfig::new(PathBuf::from("/bin/sh"))
                    .expect("shell")
                    .with_code_root(root.path().join("code-link"))
                    .is_err()
            );
        }
        std::fs::rename(&code, root.path().join("old-code")).expect("replace root");
        std::fs::create_dir(&code).expect("new code root");
        assert!(config.validate_executable_identity().is_err());
    }

    #[tokio::test]
    async fn no_reads_manifest_rejects_workspace_root_as_code_root() {
        let root = TempDir::new().expect("tempdir");
        let config = PluginProcessConfig::new(PathBuf::from("/bin/sh"))
            .expect("shell")
            .with_cwd(root.path())
            .expect("cwd")
            .with_code_root(root.path())
            .expect("code root");
        let manifest = PluginManifest {
            name: "workspace-root-code".to_owned(),
            version: "1.0.0".to_owned(),
            protocol: rw_plugin_protocol::MIN_PROTOCOL_VERSION,
            capabilities: PluginCapabilities::default(),
        };
        let store = MemoryApproval::default();
        approve_plugin_launch(&store, &manifest, &config, "project:root-code").expect("approve");
        let result = PluginHost::launch_approved(
            &TestDirectLauncher,
            &store,
            &config,
            "project:root-code",
            &[root.path().to_path_buf()],
            manifest,
            Arc::new(DenyPushHandler),
            Arc::new(NoopPluginBoundaryRedactor),
        )
        .await;
        let Err(error) = result else {
            panic!("workspace root cannot be relabeled as intrinsic code");
        };
        assert!(error.to_string().contains("strict workspace descendant"));
    }

    #[test]
    fn capability_violation_is_permanently_poisoned_and_retries_failed_kill() {
        let process = Arc::new(FakeProcess::default());
        process.kill_fails.store(true, Ordering::Release);
        let enforcer = CapabilityEnforcer::new(&manifest(), process.clone());
        let first = enforcer.check_tool("undeclared").expect_err("violation");
        assert!(first.termination_error.is_some());
        let later = enforcer
            .check_tool("fixture_tool")
            .expect_err("poisoned forever");
        assert_eq!(later, first);
        assert_eq!(process.killed.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn approved_handshake_registers_custom_tool_and_reaps_on_shutdown() {
        let root = TempDir::new().expect("tempdir");
        let config = shell_config(&root)
            .with_allowed_domains(["example.com"])
            .expect("network allowlist");
        let manifest = manifest();
        let store = MemoryApproval::default();
        approve_plugin_launch(&store, &manifest, &config, "project:test").expect("approve");
        let process = Arc::new(FakeProcess::default());
        let launcher = MemoryLauncher {
            manifest: manifest.clone(),
            process: process.clone(),
            push: None,
            hang_method: None,
        };
        let host = PluginHost::launch_approved(
            &launcher,
            &store,
            &config,
            "project:test",
            &[root.path().to_path_buf()],
            manifest.clone(),
            Arc::new(DenyPushHandler),
            Arc::new(NoopPluginBoundaryRedactor),
        )
        .await
        .expect("launch");
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(
                RpcToolAdapter::new(
                    manifest.capabilities.tools[0].clone(),
                    host.client(),
                    host.enforcer(),
                )
                .expect("approved adapter"),
            ))
            .expect("register custom tool");
        let tool = registry.resolve("fixture_tool").expect("resolved tool");
        assert_eq!(
            tool.descriptor().capabilities,
            CapabilityManifest::new([ToolCapability::ReadFilesystem, ToolCapability::Network,])
        );
        let context = ToolContext::new(root.path()).expect("tool context");
        let result = tool
            .execute(&context, json!({}))
            .await
            .expect("tool result");
        assert_eq!(result.content, "fixture");
        host.shutdown().await.expect("shutdown");
        assert!(process.waited.load(Ordering::Acquire) >= 1);
    }

    #[tokio::test]
    async fn dropping_launched_host_kills_process_without_explicit_shutdown() {
        let root = TempDir::new().expect("tempdir");
        let config = shell_config(&root)
            .with_allowed_domains(["example.com"])
            .expect("network allowlist");
        let manifest = manifest();
        let store = MemoryApproval::default();
        approve_plugin_launch(&store, &manifest, &config, "project:drop").expect("approve");
        let process = Arc::new(FakeProcess::default());
        let host = PluginHost::launch_approved(
            &MemoryLauncher {
                manifest: manifest.clone(),
                process: process.clone(),
                push: None,
                hang_method: None,
            },
            &store,
            &config,
            "project:drop",
            &[root.path().to_path_buf()],
            manifest,
            Arc::new(DenyPushHandler),
            Arc::new(NoopPluginBoundaryRedactor),
        )
        .await
        .expect("launch");

        drop(host);

        assert!(
            process.killed.load(Ordering::Acquire) >= 1,
            "the final client owner must terminate an unshut plugin"
        );
    }

    #[tokio::test]
    async fn undeclared_push_kills_and_prevents_handshake() {
        let root = TempDir::new().expect("tempdir");
        let config = shell_config(&root)
            .with_allowed_domains(["example.com"])
            .expect("network allowlist");
        let manifest = manifest();
        let store = MemoryApproval::default();
        approve_plugin_launch(&store, &manifest, &config, "project:test").expect("approve");
        let process = Arc::new(FakeProcess::default());
        let launcher = MemoryLauncher {
            manifest: manifest.clone(),
            process: process.clone(),
            push: Some(rw_plugin_protocol::METHOD_SESSION_SET_STATUS.to_owned()),
            hang_method: None,
        };
        let result = PluginHost::launch_approved(
            &launcher,
            &store,
            &config,
            "project:test",
            &[root.path().to_path_buf()],
            manifest,
            Arc::new(DenyPushHandler),
            Arc::new(NoopPluginBoundaryRedactor),
        )
        .await;
        assert!(result.is_err());
        assert!(process.killed.load(Ordering::Acquire) >= 1);
    }

    #[tokio::test]
    async fn request_timeout_is_bounded_and_shutdown_still_kills() {
        let process = Arc::new(FakeProcess::default());
        let (host_stdin, mut plugin_input) = tokio::io::duplex(4096);
        let (_plugin_output, host_stdout) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut bytes = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut plugin_input, &mut bytes).await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        let enforcer = Arc::new(CapabilityEnforcer::new(&manifest(), process.clone()));
        let client = JsonRpcPluginClient::start(
            LaunchedPluginProcess {
                stdin: Box::pin(host_stdin),
                stdout: Box::pin(BufReader::new(host_stdout)),
                stderr: Box::pin(BufReader::new(tokio::io::empty())),
                process: process.clone(),
                executable_identity: PluginProcessConfig::new(PathBuf::from("/bin/sh"))
                    .expect("shell")
                    .executable_identity()
                    .clone(),
            },
            enforcer,
            Arc::new(DenyPushHandler),
            Arc::new(DenyPluginProviderHttpHandler),
            Arc::new(NoopPluginBoundaryRedactor),
            Duration::from_millis(30),
        );
        let error = client
            .request("hang", Value::Null)
            .await
            .expect_err("timeout");
        assert_eq!(error.code, "timeout");
        client
            .shutdown(Duration::from_millis(30))
            .await
            .expect("bounded kill/reap");
        assert!(process.killed.load(Ordering::Acquire) >= 1);
    }

    #[tokio::test]
    async fn reader_exit_cancels_and_drains_active_provider_http() {
        let process = Arc::new(FakeProcess::default());
        let (plugin_output, host_stdout) = tokio::io::duplex(1024);
        drop(plugin_output);
        let (writer, _receiver) = mpsc::channel(WRITER_QUEUE_CAPACITY);
        let active_provider_http = Arc::new(StdMutex::new(BTreeMap::new()));
        let cancellation = CancellationToken::default();
        active_provider_http
            .lock()
            .expect("active HTTP lock")
            .insert(
                RpcId::String("active-http".to_owned()),
                cancellation.clone(),
            );
        let enforcer = Arc::new(CapabilityEnforcer::new(&manifest(), process.clone()));
        let state = ReaderState {
            writer,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            provider_streams: Arc::new(StdMutex::new(BTreeMap::new())),
            provider_http: Arc::new(DenyPluginProviderHttpHandler),
            active_provider_http: Arc::clone(&active_provider_http),
            abandoned: Arc::new(Mutex::new(BTreeSet::new())),
            enforcer,
            push_handler: Arc::new(DenyPushHandler),
            redactor: Arc::new(NoopPluginBoundaryRedactor),
            process: process.clone(),
        };

        reader_loop(Box::pin(BufReader::new(host_stdout)), state).await;

        assert!(cancellation.is_cancelled());
        assert!(
            active_provider_http
                .lock()
                .expect("active HTTP lock")
                .is_empty()
        );
        assert!(process.killed.load(Ordering::Acquire) >= 1);
    }

    #[tokio::test]
    async fn redaction_is_mandatory_for_hook_event_and_incoming_push_values() {
        let process = Arc::new(FakeProcess::default());
        let (host_stdin, plugin_input) = tokio::io::duplex(16 * 1024);
        let (plugin_output, host_stdout) = tokio::io::duplex(16 * 1024);
        let pushes = Arc::new(RecordingPush::default());
        tokio::spawn(async move {
            let mut input = BufReader::new(plugin_input);
            let mut output = plugin_output;
            let mut line = String::new();
            input.read_line(&mut line).await.expect("hook request");
            assert!(!line.contains("PLUGIN_CANARY_SECRET"));
            let request: RpcRequest = serde_json::from_str(line.trim()).expect("hook frame");
            output
                .write_all(
                    &encode_frame(
                        &RpcFrame::Success(RpcSuccess {
                            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                            id: Some(request.id),
                            result: Value::Null,
                        }),
                        MAX_FRAME_BYTES,
                    )
                    .expect("hook response"),
                )
                .await
                .expect("write hook response");
            line.clear();
            input
                .read_line(&mut line)
                .await
                .expect("event notification");
            assert!(!line.contains("PLUGIN_CANARY_SECRET"));
            output
                .write_all(
                    &encode_frame(
                        &RpcFrame::Request(RpcRequest {
                            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                            id: RpcId::String("push-canary".to_owned()),
                            method: METHOD_UI_NOTIFY.to_owned(),
                            params: Some(json!({
                                "title":"canary",
                                "message":"PLUGIN_CANARY_SECRET"
                            })),
                        }),
                        MAX_FRAME_BYTES,
                    )
                    .expect("push frame"),
                )
                .await
                .expect("write push");
        });
        let enforcer = Arc::new(CapabilityEnforcer::new(&manifest(), process.clone()));
        let identity = PluginProcessConfig::new(PathBuf::from("/bin/sh"))
            .expect("shell")
            .executable_identity()
            .clone();
        let client = JsonRpcPluginClient::start(
            LaunchedPluginProcess {
                stdin: Box::pin(host_stdin),
                stdout: Box::pin(BufReader::new(host_stdout)),
                stderr: Box::pin(BufReader::new(tokio::io::empty())),
                process,
                executable_identity: identity,
            },
            enforcer,
            pushes.clone(),
            Arc::new(DenyPluginProviderHttpHandler),
            Arc::new(CanaryRedactor),
            Duration::from_secs(1),
        );
        client
            .request(
                rw_plugin_protocol::METHOD_HOOK_INVOKE,
                json!({"payload":"PLUGIN_CANARY_SECRET"}),
            )
            .await
            .expect("redacted hook request");
        client
            .notify(
                METHOD_EVENT_PUBLISH,
                json!({"payload":"PLUGIN_CANARY_SECRET"}),
            )
            .await
            .expect("redacted event notification");
        tokio::time::timeout(Duration::from_secs(1), async {
            while pushes.0.lock().expect("push lock").is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("push deadline");
        assert_eq!(
            pushes.0.lock().expect("push lock")[0].1["message"],
            "[REDACTED]"
        );
    }

    fn bun_executable() -> PathBuf {
        let path = std::env::var_os("PATH").expect("PATH is available for Bun conformance");
        std::env::split_paths(&path)
            .map(|entry| entry.join("bun"))
            .find(|candidate| candidate.is_file())
            .expect("Bun is required for plugin conformance")
    }

    fn sdk_fixture_config(name: &str) -> (PathBuf, PluginProcessConfig) {
        let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/plugin-sdk")
            .canonicalize()
            .expect("SDK path");
        let fixture = sdk.join("fixtures/conformance").join(name);
        let config = PluginProcessConfig::new(bun_executable())
            .expect("Bun config")
            .with_argv([fixture.into_os_string()])
            .expect("fixture argv")
            .with_cwd(&sdk)
            .expect("SDK cwd");
        (sdk, config)
    }

    async fn approved_fixture_host(
        config: &PluginProcessConfig,
        root: &std::path::Path,
        push: Arc<dyn PushHandler>,
    ) -> PluginHost {
        approved_fixture_host_with_http(
            config,
            root,
            push,
            Arc::new(DenyPluginProviderHttpHandler),
            Arc::new(NoopPluginBoundaryRedactor),
        )
        .await
    }

    async fn approved_fixture_host_with_http(
        config: &PluginProcessConfig,
        root: &std::path::Path,
        push: Arc<dyn PushHandler>,
        provider_http: Arc<dyn PluginProviderHttpHandler>,
        redactor: Arc<dyn PluginBoundaryRedactor>,
    ) -> PluginHost {
        let manifest = probe_plugin_manifest(
            &TestDirectLauncher,
            config,
            &[root.to_path_buf()],
            Arc::new(NoopPluginBoundaryRedactor),
        )
        .await
        .expect("probe fixture manifest");
        let store = MemoryApproval::default();
        approve_plugin_launch(&store, &manifest, config, "conformance:typescript")
            .expect("approve fixture");
        PluginHost::launch_approved_with_http(
            &TestDirectLauncher,
            &store,
            config,
            "conformance:typescript",
            &[root.to_path_buf()],
            manifest,
            push,
            provider_http,
            redactor,
        )
        .await
        .expect("launch approved fixture")
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn typescript_tool_hook_event_push_and_provider_cross_rust_host() {
        let (sdk, tool_config) = sdk_fixture_config("pre-tool-deny-custom-tool.ts");
        let tool_host = approved_fixture_host(&tool_config, &sdk, Arc::new(DenyPushHandler)).await;
        let declaration = tool_host.manifest().capabilities.tools[0].clone();
        let adapter = RpcToolAdapter::new(declaration, tool_host.client(), tool_host.enforcer())
            .expect("approved tool adapter");
        let context = ToolContext::new(&sdk).expect("tool context");
        let result = adapter
            .execute(&context, json!({"text":"hello"}))
            .await
            .expect("TypeScript tool result");
        assert_eq!(result.content, "hello");
        let hook = crate::RpcHookHandler::new(tool_host.client(), tool_host.enforcer());
        let mut dispatcher = crate::HookDispatcher::new();
        dispatcher
            .register(
                crate::plugin_hook_registration(
                    tool_host.manifest().capabilities.hooks[0],
                    "typescript:pre-tool",
                ),
                hook,
            )
            .expect("register RPC hook");
        assert!(matches!(
            dispatcher
                .dispatch(crate::HookEvent::PreTool, json!({"name":"bash"}))
                .await
                .status(),
            crate::HookDispatchStatus::Blocked { .. }
        ));
        tool_host.shutdown().await.expect("tool host shutdown");

        let (sdk, event_config) = sdk_fixture_config("event-subscriber.ts");
        let pushes = Arc::new(RecordingPush::default());
        let event_host = approved_fixture_host(&event_config, &sdk, pushes.clone()).await;
        PluginEventRouter::new(event_host.client(), event_host.enforcer())
            .publish("TurnFinished", json!({"session_id":"s"}))
            .await
            .expect("publish event");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !pushes.0.lock().expect("push lock").is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("event push deadline");
        assert_eq!(
            pushes.0.lock().expect("push lock")[0].0,
            METHOD_SESSION_SET_STATUS
        );
        event_host.shutdown().await.expect("event host shutdown");

        let (sdk, provider_config) = sdk_fixture_config("provider.ts");
        let provider_config = provider_config
            .with_allowed_domains(["example.com"])
            .expect("provider domains");
        let provider_host =
            approved_fixture_host(&provider_config, &sdk, Arc::new(DenyPushHandler)).await;
        let provider = RpcProviderAdapter::new(
            "typescript-fixture",
            "fixture/",
            Capabilities {
                tool_calling: true,
                vision: false,
                thinking: false,
                cache_breakpoints: CacheBreakpointSupport::None,
                max_context_tokens: None,
                max_output_tokens: None,
                wire_mode: WireMode::NormalizedReplay,
            },
            provider_host.client(),
            provider_host.enforcer(),
        );
        let mut events = provider
            .stream(ProviderRequest {
                model: "model".to_owned(),
                turns: Vec::new(),
                tools: Vec::new(),
                tool_choice: ToolChoice::Auto,
                max_output_tokens: 64,
                temperature: None,
                thinking: ThinkingLevel::Off,
                cache_hint: None,
            })
            .await
            .expect("provider response");
        let stream_started = std::time::Instant::now();
        assert!(matches!(
            events.next().await,
            Some(Ok(ProviderEvent::MessageStart { .. }))
        ));
        assert!(matches!(
            events.next().await,
            Some(Ok(ProviderEvent::TextDelta { text })) if text.contains("fixture/model")
        ));
        let first_delta = stream_started.elapsed();
        while events.next().await.is_some() {}
        let completed = stream_started.elapsed();
        assert!(
            completed.saturating_sub(first_delta) >= Duration::from_millis(50),
            "provider completion was not observably delayed after its first delta: delta={first_delta:?} complete={completed:?}"
        );

        let cancelled = provider
            .stream(ProviderRequest {
                model: "cancelled".to_owned(),
                turns: Vec::new(),
                tools: Vec::new(),
                tool_choice: ToolChoice::Auto,
                max_output_tokens: 64,
                temperature: None,
                thinking: ThinkingLevel::Off,
                cache_hint: None,
            })
            .await
            .expect("cancelled provider stream admission");
        drop(cancelled);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let streams_empty = provider_host
                    .client
                    .provider_streams
                    .lock()
                    .expect("provider stream state")
                    .is_empty();
                let abandoned_empty = provider_host.client.abandoned.lock().await.is_empty();
                if streams_empty && abandoned_empty {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled provider correlation cleanup");
        provider_host
            .shutdown()
            .await
            .expect("provider host shutdown");
    }

    #[tokio::test]
    async fn typescript_protocol_two_catalog_crosses_rust_host() {
        let (sdk, provider_config) = sdk_fixture_config("provider-v2.ts");
        let provider_config = provider_config
            .with_allowed_domains(["example.com"])
            .expect("provider domains");
        let host = approved_fixture_host(&provider_config, &sdk, Arc::new(DenyPushHandler)).await;
        assert_eq!(
            host.manifest().protocol,
            rw_plugin_protocol::PROTOCOL_VERSION
        );
        let provider = RpcProviderAdapter::new(
            "typescript-fixture-v2",
            "fixture-v2/",
            Capabilities {
                tool_calling: true,
                vision: false,
                thinking: false,
                cache_breakpoints: CacheBreakpointSupport::None,
                max_context_tokens: None,
                max_output_tokens: None,
                wire_mode: WireMode::NormalizedReplay,
            },
            host.client(),
            host.enforcer(),
        )
        .with_model_catalog();
        let catalog = provider
            .discover_models()
            .await
            .expect("catalog request")
            .expect("protocol 2 catalog");
        assert_eq!(catalog.provider, "fixture-v2");
        assert_eq!(catalog.models[0].id, "vision-thinking");
        let metadata = provider
            .cached_model_metadata()
            .expect("single model metadata");
        assert!(metadata.capabilities.vision);
        assert!(metadata.capabilities.thinking);
        assert_eq!(metadata.accounting, UsageAccounting::ApiDollars);
        assert_eq!(
            metadata
                .pricing
                .expect("catalog pricing")
                .input_per_million_micros_usd,
            3_000_000
        );
        host.shutdown().await.expect("provider host shutdown");
    }

    #[tokio::test]
    async fn protocol_two_provider_auth_streams_through_host_without_secret_delivery() {
        let (sdk, config) = sdk_fixture_config("provider-auth-v2.ts");
        let config = config
            .with_allowed_domains(["api.example.test"])
            .expect("provider domains");
        let http = Arc::new(FixtureProviderHttp::default());
        let host = approved_fixture_host_with_http(
            &config,
            &sdk,
            Arc::new(DenyPushHandler),
            http.clone(),
            Arc::new(HttpSecretRedactor),
        )
        .await;
        assert_eq!(
            host.manifest().capabilities.providers[0].credential_references,
            ["fixture-token"]
        );
        let provider = RpcProviderAdapter::new(
            "typescript-auth-v2",
            "auth-v2/",
            Capabilities {
                tool_calling: true,
                vision: false,
                thinking: false,
                cache_breakpoints: CacheBreakpointSupport::None,
                max_context_tokens: None,
                max_output_tokens: None,
                wire_mode: WireMode::NormalizedReplay,
            },
            host.client(),
            host.enforcer(),
        );
        let events = provider
            .stream(ProviderRequest {
                model: "tool-model".to_owned(),
                turns: Vec::new(),
                tools: Vec::new(),
                tool_choice: ToolChoice::Auto,
                max_output_tokens: 64,
                temperature: None,
                thinking: ThinkingLevel::Off,
                cache_hint: None,
            })
            .await
            .expect("host-mediated provider stream")
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ProviderEvent::ToolCallEnd { id, arguments })
                if id == "call-1" && arguments["city"] == "Chicago"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ProviderEvent::TextDelta { text }) if text == "[REDACTED]"
        )));
        let serialized_requests =
            serde_json::to_string(&*http.requests.lock().expect("request lock"))
                .expect("serialized captured requests");
        assert!(!serialized_requests.contains(HTTP_SECRET));
        assert!(serialized_requests.contains("fixture-token"));
        let cancelled = provider
            .stream(ProviderRequest {
                model: "cancelled".to_owned(),
                turns: Vec::new(),
                tools: Vec::new(),
                tool_choice: ToolChoice::Auto,
                max_output_tokens: 64,
                temperature: None,
                thinking: ThinkingLevel::Off,
                cache_hint: None,
            })
            .await
            .expect("cancelled HTTP provider admission");
        tokio::time::sleep(Duration::from_millis(25)).await;
        drop(cancelled);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !http.cancelled.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("host-mediated HTTP cancellation deadline");
        host.shutdown().await.expect("auth provider host shutdown");
    }

    #[tokio::test]
    async fn protocol_two_provider_refuses_undeclared_credential_reference_at_call_time() {
        let (sdk, config) = sdk_fixture_config("provider-auth-v2.ts");
        let config = config
            .with_allowed_domains(["api.example.test"])
            .expect("provider domains");
        let http = Arc::new(FixtureProviderHttp::default());
        let host = approved_fixture_host_with_http(
            &config,
            &sdk,
            Arc::new(DenyPushHandler),
            http.clone(),
            Arc::new(HttpSecretRedactor),
        )
        .await;
        let provider = RpcProviderAdapter::new(
            "typescript-auth-v2",
            "auth-v2/",
            Capabilities {
                tool_calling: true,
                vision: false,
                thinking: false,
                cache_breakpoints: CacheBreakpointSupport::None,
                max_context_tokens: None,
                max_output_tokens: None,
                wire_mode: WireMode::NormalizedReplay,
            },
            host.client(),
            host.enforcer(),
        );
        let result = provider
            .stream(ProviderRequest {
                model: "undeclared".to_owned(),
                turns: Vec::new(),
                tools: Vec::new(),
                tool_choice: ToolChoice::Auto,
                max_output_tokens: 64,
                temperature: None,
                thinking: ThinkingLevel::Off,
                cache_hint: None,
            })
            .await;
        if let Ok(mut stream) = result {
            assert!(stream.next().await.is_some_and(|item| item.is_err()));
        }
        assert!(host.enforcer().violated());
        assert!(http.requests.lock().expect("request lock").is_empty());
    }

    #[tokio::test]
    async fn plugin_originated_undeclared_push_is_killed_and_reaped() {
        let (sdk, config) = sdk_fixture_config("undeclared-push.ts");
        let manifest = PluginManifest {
            name: "undeclared-push".to_owned(),
            version: "1.0.0".to_owned(),
            protocol: rw_plugin_protocol::MIN_PROTOCOL_VERSION,
            capabilities: PluginCapabilities::default(),
        };
        let store = MemoryApproval::default();
        approve_plugin_launch(&store, &manifest, &config, "conformance:violation")
            .expect("approve adversarial fixture");
        let launcher = TrackingDirectLauncher::default();
        let host_result = PluginHost::launch_approved(
            &launcher,
            &store,
            &config,
            "conformance:violation",
            &[sdk],
            manifest,
            Arc::new(DenyPushHandler),
            Arc::new(NoopPluginBoundaryRedactor),
        )
        .await;
        if let Ok(host) = &host_result {
            tokio::time::timeout(Duration::from_secs(2), async {
                while !host.enforcer().violated() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("capability violation deadline");
        }
        let process = launcher
            .0
            .lock()
            .expect("tracking launcher")
            .clone()
            .expect("tracked process");
        tokio::time::timeout(Duration::from_secs(2), process.wait())
            .await
            .expect("violator reap deadline")
            .expect("violator wait");
    }

    #[tokio::test]
    async fn direct_argv_launcher_never_invokes_a_shell_implicitly() {
        let root = TempDir::new().expect("tempdir");
        let marker = root.path().join("marker");
        let config = PluginProcessConfig::new(PathBuf::from("/bin/sh"))
            .expect("shell")
            .with_argv([
                "-c",
                "read request; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}'",
            ])
            .expect("argv")
            .with_cwd(root.path())
            .expect("cwd");
        let launched = TestDirectLauncher
            .launch(
                &config,
                &PluginSandboxProfile {
                    mode: PluginSandboxMode::Approved,
                    capabilities: PluginCapabilities::default(),
                    approved_roots: vec![root.path().to_path_buf()],
                    allowed_domains: Vec::new(),
                },
            )
            .await
            .expect("direct launch");
        assert!(!marker.exists());
        launched.process.kill_tree().expect("kill direct child");
        tokio::time::timeout(Duration::from_secs(2), launched.process.reap())
            .await
            .expect("bounded reap")
            .expect("reap");
    }
}
