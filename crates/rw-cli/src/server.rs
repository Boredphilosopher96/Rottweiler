mod client_authority;
mod command_input;
use client_authority::{AuthenticatedClient, ClientAuthority, ClientCapability};
pub(crate) use command_input::LANE_HEADER as COMMAND_LANE_HEADER;
use std::{
    collections::HashSet,
    convert::Infallible,
    fmt,
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use http_body_util::{BodyExt as _, Full, Limited, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Frame, Incoming},
    header::{ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONNECTION, CONTENT_TYPE, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use miette::{IntoDiagnostic as _, Result, miette};
use rw_core::{
    ClientCommand, ClientId, CommandOutcome, EngineError, EngineErrorCategory, ProviderApiKey,
    ProviderApiKeySubmission, SequenceId, SessionId, ShellId,
};
use serde::{Deserialize, Serialize};
use tokio::{
    net::UnixListener,
    sync::{Notify, mpsc},
};

// A legal message may carry two 5 MiB images. Inline base64 expands that to
// roughly 13.4 MiB before the bounded JSON envelope, so the command transport
// must be at least as large as the protocol's already-bounded SSE envelope.
const COMMAND_BODY_LIMIT: usize = 16 * 1024 * 1024;
const PROVIDER_API_KEY_BODY_LIMIT: usize = 16 * 1024;
const PROVIDER_API_KEY_LIMIT: usize = 8 * 1024;
const MAX_PROVIDER_API_KEY_ATTEMPTS: usize = 256;
const HOST_EVENT_FORWARD_CAPACITY: usize = 256;
pub(crate) const CLIENT_HEADER: &str = "x-rottweiler-client";
pub(crate) const CAPABILITY_HEADER: &str = "x-rottweiler-capability";

type HttpBody = UnsyncBoxBody<Bytes, Infallible>;

#[derive(Clone, Eq, PartialEq)]
struct SecretToken([u8; 32]);

impl SecretToken {
    fn generate() -> Result<Self> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).into_diagnostic()?;
        Ok(Self(bytes))
    }

    fn encode(&self) -> String {
        let mut encoded = String::with_capacity(self.0.len() * 2);
        for byte in self.0 {
            use std::fmt::Write as _;
            let _ = write!(&mut encoded, "{byte:02x}");
        }
        encoded
    }

    fn parse(encoded: &str) -> Option<Self> {
        if encoded.len() != 64 || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let mut bytes = [0_u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let offset = index * 2;
            *byte = u8::from_str_radix(&encoded[offset..offset + 2], 16).ok()?;
        }
        Some(Self(bytes))
    }

    fn matches_encoded(&self, encoded: &str) -> bool {
        let Some(candidate) = Self::parse(encoded) else {
            return false;
        };
        self.0
            .iter()
            .zip(candidate.0)
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretToken([REDACTED])")
    }
}

/// Private files used by one local engine server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerRuntimePaths {
    pub directory: PathBuf,
    pub socket: PathBuf,
    pub token: PathBuf,
    pub descriptor: PathBuf,
}

#[derive(Debug, Serialize)]
struct RuntimeDescriptor<'a> {
    version: u16,
    pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    socket: &'a Path,
    token_file: &'a Path,
}

/// An initialized server runtime. The bootstrap token remains memory-only
/// except for its user-private token file and is always redacted from Debug.
#[derive(Debug)]
pub struct ServerRuntime {
    pub paths: ServerRuntimePaths,
    bootstrap: SecretToken,
}

impl ServerRuntime {
    /// Creates a fresh private runtime directory, auth token, descriptor, and
    /// Unix socket. Existing paths are never removed or reused.
    pub fn create(root: &Path) -> Result<(Self, std::os::unix::net::UnixListener)> {
        ensure_private_directory(root)?;
        let directory = (0..32)
            .find_map(|_| {
                let suffix = SecretToken::generate().ok()?.encode();
                let path = root.join(format!("engine-{}", &suffix[..16]));
                match fs::create_dir(&path) {
                    Ok(()) => Some(Ok(path)),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .transpose()
            .into_diagnostic()?
            .ok_or_else(|| miette!("could not allocate a unique engine runtime directory"))?;
        set_mode(&directory, 0o700)?;

        let paths = ServerRuntimePaths {
            socket: directory.join("engine.sock"),
            token: directory.join("auth.token"),
            descriptor: directory.join("runtime.json"),
            directory,
        };
        Self::create_at(paths)
    }

    /// Creates a server at supervisor-selected private paths. Stale artifacts
    /// from a crashed server may be replaced, but only inside the same
    /// owner-private runtime directory and only when their file types match the
    /// artifact being replaced.
    pub fn create_at(
        paths: ServerRuntimePaths,
    ) -> Result<(Self, std::os::unix::net::UnixListener)> {
        Self::create_for_session(paths, None)
    }

    /// Creates a server at supervisor-selected paths and projects the owning
    /// session into the private discovery descriptor. The session binding lets
    /// supervisors clean up only the runtime they launched.
    pub fn create_for_session(
        paths: ServerRuntimePaths,
        session_id: Option<&str>,
    ) -> Result<(Self, std::os::unix::net::UnixListener)> {
        validate_runtime_paths(&paths)?;
        ensure_private_directory(&paths.directory)?;
        refuse_live_listener(&paths.socket)?;
        remove_stale_artifact(&paths.socket, RuntimeArtifact::Socket)?;
        remove_stale_artifact(&paths.token, RuntimeArtifact::RegularFile)?;
        remove_stale_artifact(&paths.descriptor, RuntimeArtifact::RegularFile)?;
        let bootstrap = SecretToken::generate()?;
        // Runtime credentials are ephemeral and rotate after a crash; they
        // must be visible and private before bind, but do not need storage
        // durability beyond the lifetime of this server process.
        write_private_new_relaxed(&paths.token, bootstrap.encode().as_bytes())?;
        let descriptor = serde_json::to_vec(&RuntimeDescriptor {
            version: 1,
            pid: std::process::id(),
            session_id,
            socket: &paths.socket,
            token_file: &paths.token,
        })
        .into_diagnostic()?;
        // The descriptor is a disposable discovery projection. The token is
        // synced above; delaying startup on a second fsync provides no crash
        // safety because a missing descriptor is rebuilt with the server.
        write_private_new_relaxed(&paths.descriptor, &descriptor)?;

        let listener = std::os::unix::net::UnixListener::bind(&paths.socket).into_diagnostic()?;
        set_mode(&paths.socket, 0o600)?;
        listener.set_nonblocking(true).into_diagnostic()?;
        Ok((Self { paths, bootstrap }, listener))
    }

    fn bootstrap(&self) -> &SecretToken {
        &self.bootstrap
    }
}

fn refuse_live_listener(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !is_socket(&metadata) {
                return Err(miette!(
                    "refusing to replace an unexpected server runtime artifact"
                ));
            }
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(miette!("refusing to replace a live engine server socket"));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).into_diagnostic(),
    }
}

#[derive(Clone, Copy)]
enum RuntimeArtifact {
    Socket,
    RegularFile,
}

fn validate_runtime_paths(paths: &ServerRuntimePaths) -> Result<()> {
    if !paths.directory.is_absolute()
        || paths.socket.parent() != Some(paths.directory.as_path())
        || paths.token.parent() != Some(paths.directory.as_path())
        || paths.descriptor.parent() != Some(paths.directory.as_path())
        || [&paths.socket, &paths.token, &paths.descriptor]
            .iter()
            .any(|path| path.file_name().is_none())
    {
        return Err(miette!(
            "server runtime artifacts must be direct children of one absolute private directory"
        ));
    }
    Ok(())
}

fn remove_stale_artifact(path: &Path, expected: RuntimeArtifact) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let matches = match expected {
                RuntimeArtifact::Socket => is_socket(&metadata),
                RuntimeArtifact::RegularFile => metadata.is_file(),
            };
            if metadata.file_type().is_symlink() || !matches {
                return Err(miette!(
                    "refusing to replace an unexpected server runtime artifact"
                ));
            }
            fs::remove_file(path).into_diagnostic()
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).into_diagnostic(),
    }
}

#[cfg(unix)]
fn is_socket(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    metadata.file_type().is_socket()
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(miette!("server runtime root is not a real directory"));
            }
            if metadata.uid() != rustix::process::geteuid().as_raw() {
                return Err(miette!("server runtime root is not owned by this user"));
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                set_mode(path, 0o700)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).into_diagnostic()?;
            set_mode(path, 0o700)?;
        }
        Err(error) => return Err(error).into_diagnostic(),
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).into_diagnostic()
}

fn write_private_new_relaxed(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .into_diagnostic()?;
    file.write_all(bytes).into_diagnostic()?;
    file.flush().into_diagnostic()?;
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClientCredentials {
    pub client_id: ClientId,
    pub token: String,
}

/// Protocol host boundary consumed by the HTTP/SSE transport.
#[async_trait]
pub trait ServerEngine: Send + Sync + 'static {
    fn is_ready(&self) -> bool {
        true
    }

    async fn dispatch(
        &self,
        bound_client: ClientId,
        command: ClientCommand,
    ) -> std::result::Result<rw_core::HostReply, String>;

    async fn subscribe(
        &self,
        bound_client: ClientId,
        session_id: Option<SessionId>,
        last_seen: Option<SequenceId>,
    ) -> std::result::Result<
        mpsc::Receiver<std::result::Result<rw_core::HostEvent, String>>,
        EventSubscriptionError,
    >;

    /// Releases a foreground-shell gate from the trusted CLI parent without
    /// transferring the interactive driver's lease.
    async fn complete_shell(
        &self,
        session_id: SessionId,
        shell_id: ShellId,
        status: i32,
        captured_output: Option<String>,
    ) -> std::result::Result<(), String>;

    async fn submit_provider_api_key(
        &self,
        _bound_client: ClientId,
        _session_id: SessionId,
        _provider: String,
        _api_key: ProviderApiKey,
    ) -> std::result::Result<ProviderApiKeySubmission, String>;

    async fn activate_provider(
        &self,
        _bound_client: ClientId,
        _session_id: SessionId,
        _provider: String,
    ) -> std::result::Result<(), String>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventSubscriptionError {
    ReplayCursorAhead,
    Other(String),
}

impl From<String> for EventSubscriptionError {
    fn from(error: String) -> Self {
        Self::Other(error)
    }
}

/// Production adapter from the core multi-session host to the transport trait.
#[derive(Clone, Debug)]
pub struct HostedEngine {
    host: rw_core::EngineHost,
}

impl HostedEngine {
    #[must_use]
    pub fn new(host: rw_core::EngineHost) -> Self {
        Self { host }
    }
}

#[async_trait]
impl ServerEngine for HostedEngine {
    async fn dispatch(
        &self,
        bound_client: ClientId,
        command: ClientCommand,
    ) -> std::result::Result<rw_core::HostReply, String> {
        Ok(self
            .host
            .dispatch(
                rw_core::BoundClient {
                    client_id: bound_client,
                },
                command,
            )
            .await)
    }

    async fn subscribe(
        &self,
        bound_client: ClientId,
        session_id: Option<SessionId>,
        last_seen: Option<SequenceId>,
    ) -> std::result::Result<
        mpsc::Receiver<std::result::Result<rw_core::HostEvent, String>>,
        EventSubscriptionError,
    > {
        let mut source = self
            .host
            .subscribe(
                rw_core::BoundClient {
                    client_id: bound_client,
                },
                session_id,
                last_seen,
            )
            .await
            .map_err(|error| match error {
                rw_core::HostError::ReplayCursorAhead => EventSubscriptionError::ReplayCursorAhead,
                other => EventSubscriptionError::Other(other.to_string()),
            })?;
        let (send, receive) = mpsc::channel(HOST_EVENT_FORWARD_CAPACITY);
        tokio::spawn(async move {
            while let Some(event) = source.recv().await {
                if send
                    .send(event.map_err(|error| error.to_string()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        Ok(receive)
    }

    async fn complete_shell(
        &self,
        session_id: SessionId,
        shell_id: ShellId,
        status: i32,
        captured_output: Option<String>,
    ) -> std::result::Result<(), String> {
        self.host
            .complete_user_shell(&session_id, shell_id, status, captured_output)
            .await
            .map_err(|error| error.to_string())
    }

    async fn submit_provider_api_key(
        &self,
        bound_client: ClientId,
        session_id: SessionId,
        provider: String,
        api_key: ProviderApiKey,
    ) -> std::result::Result<ProviderApiKeySubmission, String> {
        self.host
            .submit_provider_api_key(
                rw_core::BoundClient {
                    client_id: bound_client,
                },
                &session_id,
                &provider,
                api_key,
            )
            .await
            .map_err(|error| error.to_string())
    }

    async fn activate_provider(
        &self,
        bound_client: ClientId,
        session_id: SessionId,
        provider: String,
    ) -> std::result::Result<(), String> {
        self.host
            .activate_provider_for_client(
                rw_core::BoundClient {
                    client_id: bound_client,
                },
                &session_id,
                &provider,
            )
            .await
            .map_err(|error| error.to_string())
    }
}

#[derive(Deserialize)]
struct ProviderApiKeyRequest {
    session_id: String,
    provider: String,
    api_key: String,
}

impl fmt::Debug for ProviderApiKeyRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderApiKeyRequest")
            .field("session_id", &self.session_id)
            .field("provider", &self.provider)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

#[derive(Serialize)]
struct ProviderApiKeyResponse {
    stored: bool,
    activated: bool,
    warnings: Vec<String>,
}

#[derive(Deserialize)]
struct ActivateProviderRequest {
    session_id: String,
    provider: String,
}

struct ProviderApiKeyAttemptGuard {
    attempts: Arc<Mutex<HashSet<String>>>,
    key: String,
}

impl ProviderApiKeyAttemptGuard {
    fn reserve(attempts: Arc<Mutex<HashSet<String>>>, key: String) -> Option<Self> {
        let reserved = {
            let mut entries = attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries.len() < MAX_PROVIDER_API_KEY_ATTEMPTS && entries.insert(key.clone())
        };
        reserved.then_some(Self { attempts, key })
    }
}

impl Drop for ProviderApiKeyAttemptGuard {
    fn drop(&mut self) {
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
    }
}

#[derive(Clone)]
pub struct ServerState {
    engine: Arc<dyn ServerEngine>,
    bootstrap: SecretToken,
    clients: Arc<ClientAuthority>,
    shutdown_notifier: Arc<Notify>,
    command_ingress: Arc<command_input::CommandIngress>,
    connections: Arc<tokio::sync::Semaphore>,
    provider_api_key_attempts: Arc<Mutex<HashSet<String>>>,
}

impl fmt::Debug for ServerState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerState")
            .field("bootstrap", &self.bootstrap)
            .finish_non_exhaustive()
    }
}

impl ServerState {
    #[must_use]
    pub fn new(engine: Arc<dyn ServerEngine>, runtime: &ServerRuntime) -> Self {
        Self {
            engine,
            bootstrap: runtime.bootstrap().clone(),
            clients: Arc::new(ClientAuthority::new(runtime.bootstrap())),
            shutdown_notifier: Arc::new(Notify::new()),
            command_ingress: Arc::default(),
            connections: Arc::new(tokio::sync::Semaphore::new(128)),
            provider_api_key_attempts: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

/// Serves authenticated HTTP/1.1 command and SSE traffic over a pre-bound Unix
/// socket until `shutdown` changes to true.
pub async fn serve(
    listener: std::os::unix::net::UnixListener,
    state: ServerState,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let listener = UnixListener::from_std(listener).into_diagnostic()?;
    loop {
        tokio::select! {
            () = state.shutdown_notifier.notified() => return Ok(()),
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted.into_diagnostic()?;
                let Ok(connection_permit) = state.connections.clone().try_acquire_owned() else { continue; };
                let connection_state = state.clone();
                tokio::spawn(async move {
                    let _connection_permit = connection_permit;
                    let shutdown_state = connection_state.clone();
                    let connection_shutdown = Arc::new(AtomicBool::new(false));
                    let request_shutdown = Arc::clone(&connection_shutdown);
                    let service = service_fn(move |request| {
                        handle_request(
                            request,
                            connection_state.clone(),
                            Arc::clone(&request_shutdown),
                        )
                    });
                    if let Err(error) = http1::Builder::new()
                        .keep_alive(true)
                        .max_buf_size(16 * 1024)
                        .timer(hyper_util::rt::TokioTimer::new())
                        .header_read_timeout(std::time::Duration::from_secs(3))
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        tracing::debug!(reason = %error, "engine client connection closed");
                    }
                    if connection_shutdown.load(Ordering::Acquire) {
                        shutdown_state.shutdown_notifier.notify_one();
                    }
                });
            }
        }
    }
}

#[allow(clippy::collapsible_else_if, clippy::too_many_lines)]
async fn handle_request(
    request: Request<Incoming>,
    state: ServerState,
    connection_shutdown: Arc<AtomicBool>,
) -> std::result::Result<Response<HttpBody>, Infallible> {
    let response = match (request.method(), request.uri().path()) {
        (&Method::POST, "/v1/connect") => {
            if authenticate_bootstrap(&request, &state.bootstrap) {
                let capability = match requested_capability(&request) {
                    Ok(capability) => capability,
                    Err(message) => return Ok(error_response(StatusCode::BAD_REQUEST, message)),
                };
                match state
                    .clients
                    .mint(capability)
                    .and_then(|credentials| serde_json::to_vec(&credentials).into_diagnostic())
                {
                    Ok(bytes) => json_response(StatusCode::CREATED, bytes),
                    Err(error) => {
                        error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string())
                    }
                }
            } else {
                unauthorized()
            }
        }
        (&Method::GET, "/v1/health") => {
            if authenticate_bootstrap(&request, &state.bootstrap) {
                if state.engine.is_ready() {
                    json_response(StatusCode::OK, br#"{"ready":true}"#.to_vec())
                } else {
                    json_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        br#"{"ready":false}"#.to_vec(),
                    )
                }
            } else {
                unauthorized()
            }
        }
        (&Method::POST, "/v1/command") => {
            let Some(client) = authenticate_client(&request, &state.clients) else {
                return Ok(unauthorized());
            };
            let Some(lane) = request
                .headers()
                .get(command_input::LANE_HEADER)
                .and_then(|value| value.to_str().ok())
            else {
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    "command lane is required",
                ));
            };
            let length = request
                .headers()
                .get(hyper::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<usize>().ok());
            let input = match state
                .command_ingress
                .acquire(&client.client_id, lane, length)
            {
                Ok(input) => input,
                Err(command_input::AdmissionError::InvalidLane) => {
                    return Ok(error_response(
                        StatusCode::BAD_REQUEST,
                        "command lane must be normal or urgent",
                    ));
                }
                Err(command_input::AdmissionError::BodyLimit) => {
                    return Ok(error_response(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "command body exceeds its lane limit",
                    ));
                }
                Err(command_input::AdmissionError::Busy) => {
                    return Ok(error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "command input admission exhausted",
                    ));
                }
            };
            let body = request.into_body();
            match tokio::time::timeout(
                command_input::BODY_TIMEOUT,
                Limited::new(body, input.limit).collect(),
            )
            .await
            {
                Ok(Ok(collected)) => {
                    match command_input::decode(collected.to_bytes(), input).await {
                        Ok(command_input::ParsedCommand {
                            mut command,
                            lease: _input,
                        }) => {
                            command.meta_mut().client_id = client.client_id.clone();
                            let shutdown_requested =
                                matches!(&command, ClientCommand::ShutdownHost { .. });
                            let outcome = match client.capability {
                                ClientCapability::Interactive => {
                                    state.engine.dispatch(client.client_id, command).await
                                }
                                ClientCapability::PluginDevelopment => {
                                    dispatch_plugin_development(&*state.engine, command).await
                                }
                                ClientCapability::ShellBroker => {
                                    dispatch_shell_broker(&*state.engine, command)
                                        .await
                                        .map(rw_core::HostReply::command)
                                }
                            };
                            match outcome {
                                Ok(outcome) => {
                                    let accepted = outcome.outcome == CommandOutcome::Accepted {};
                                    let mut response =
                                        json_response(StatusCode::ACCEPTED, outcome.bytes);
                                    if shutdown_requested && accepted {
                                        connection_shutdown.store(true, Ordering::Release);
                                        response
                                            .headers_mut()
                                            .insert(CONNECTION, HeaderValue::from_static("close"));
                                    }
                                    response
                                }
                                Err(error) => error_response(StatusCode::BAD_GATEWAY, &error),
                            }
                        }
                        Err(command_input::DecodeError::Busy) => error_response(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "command decode admission exhausted",
                        ),
                        Err(command_input::DecodeError::Invalid(_)) => error_response(
                            StatusCode::BAD_REQUEST,
                            "command body is not valid protocol JSON",
                        ),
                    }
                }
                Ok(Err(_)) => error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "command body exceeds the transport limit",
                ),
                Err(_) => {
                    error_response(StatusCode::REQUEST_TIMEOUT, "command body deadline expired")
                }
            }
        }
        (&Method::POST, "/v1/provider-api-key") => {
            let Some(client) = authenticate_client(&request, &state.clients) else {
                return Ok(unauthorized());
            };
            if client.capability != ClientCapability::Interactive {
                return Ok(error_response(
                    StatusCode::FORBIDDEN,
                    "interactive client required",
                ));
            }
            if request
                .headers()
                .get(hyper::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<usize>().ok())
                .is_some_and(|length| length > PROVIDER_API_KEY_BODY_LIMIT)
            {
                return Ok(error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "credential body exceeds the transport limit",
                ));
            }
            match Limited::new(request.into_body(), PROVIDER_API_KEY_BODY_LIMIT)
                .collect()
                .await
            {
                Ok(collected) => {
                    match serde_json::from_slice::<ProviderApiKeyRequest>(&collected.to_bytes()) {
                        Ok(secret_request) => {
                            let ProviderApiKeyRequest {
                                session_id,
                                provider,
                                api_key,
                            } = secret_request;
                            if SessionId::validate(&session_id).is_err()
                                || provider.len() > 128
                                || !provider.bytes().all(|byte| {
                                    byte.is_ascii_alphanumeric()
                                        || matches!(byte, b'-' | b'_' | b'.')
                                })
                                || api_key.len() > PROVIDER_API_KEY_LIMIT
                            {
                                error_response(
                                    StatusCode::BAD_REQUEST,
                                    "credential request is invalid",
                                )
                            } else {
                                if let Some(attempt_guard) = ProviderApiKeyAttemptGuard::reserve(
                                    Arc::clone(&state.provider_api_key_attempts),
                                    provider.clone(),
                                ) {
                                    let api_key = ProviderApiKey::from_terminal_input(api_key);
                                    let result = match api_key {
                                        Ok(api_key) => {
                                            let engine = Arc::clone(&state.engine);
                                            tokio::spawn(async move {
                                                let _attempt_guard = attempt_guard;
                                                engine
                                                    .submit_provider_api_key(
                                                        client.client_id,
                                                        SessionId(session_id),
                                                        provider,
                                                        api_key,
                                                    )
                                                    .await
                                            })
                                            .await
                                            .unwrap_or_else(|_| {
                                                Err("provider credential submission failed"
                                                    .to_owned())
                                            })
                                        }
                                        Err(_) => Err("API key must not be empty".to_owned()),
                                    };
                                    match result {
                                        Ok(submission) => {
                                            match serde_json::to_vec(&ProviderApiKeyResponse {
                                                stored: submission.stored,
                                                activated: submission.activated,
                                                warnings: submission.warnings,
                                            }) {
                                                Ok(bytes) => json_response(StatusCode::OK, bytes),
                                                Err(_) => error_response(
                                                    StatusCode::INTERNAL_SERVER_ERROR,
                                                    "credential result could not serialize",
                                                ),
                                            }
                                        }
                                        Err(_) => error_response(
                                            StatusCode::BAD_REQUEST,
                                            "provider credential submission failed",
                                        ),
                                    }
                                } else {
                                    error_response(
                                        StatusCode::CONFLICT,
                                        "provider credential submission is already in progress",
                                    )
                                }
                            }
                        }
                        Err(_) => {
                            error_response(StatusCode::BAD_REQUEST, "credential body is invalid")
                        }
                    }
                }
                Err(_) => error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "credential body exceeds the transport limit",
                ),
            }
        }
        (&Method::POST, "/v1/activate-provider") => {
            let Some(client) = authenticate_client(&request, &state.clients) else {
                return Ok(unauthorized());
            };
            if client.capability != ClientCapability::Interactive {
                return Ok(error_response(
                    StatusCode::FORBIDDEN,
                    "interactive client required",
                ));
            }
            match Limited::new(request.into_body(), 1_024).collect().await {
                Ok(collected) => {
                    match serde_json::from_slice::<ActivateProviderRequest>(&collected.to_bytes()) {
                        Ok(ActivateProviderRequest {
                            session_id,
                            provider,
                        }) if SessionId::validate(&session_id).is_ok()
                            && !provider.is_empty()
                            && provider.len() <= 128
                            && provider.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                            }) =>
                        {
                            match state
                                .engine
                                .activate_provider(
                                    client.client_id,
                                    SessionId(session_id),
                                    provider,
                                )
                                .await
                            {
                                Ok(()) => {
                                    json_response(StatusCode::OK, br#"{"activated":true}"#.to_vec())
                                }
                                Err(_) => error_response(
                                    StatusCode::BAD_REQUEST,
                                    "provider activation failed",
                                ),
                            }
                        }
                        _ => error_response(
                            StatusCode::BAD_REQUEST,
                            "provider activation request is invalid",
                        ),
                    }
                }
                Err(_) => error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "provider activation request is too large",
                ),
            }
        }
        (&Method::GET, "/v1/events") => {
            let Some(client) = authenticate_client(&request, &state.clients) else {
                return Ok(unauthorized());
            };
            if request.headers().get(ACCEPT).is_some_and(|value| {
                value
                    .to_str()
                    .ok()
                    .is_none_or(|value| !value.contains("text/event-stream"))
            }) {
                return Ok(error_response(
                    StatusCode::NOT_ACCEPTABLE,
                    "event stream requires text/event-stream",
                ));
            }
            let query = request.uri().query().unwrap_or_default();
            let mut session_id = None;
            let mut last_seen = None;
            for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
                match key.as_ref() {
                    "session_id" if !value.is_empty() => match SessionId::parse(value.into_owned())
                    {
                        Ok(value) => session_id = Some(value),
                        Err(_) => {
                            return Ok(error_response(
                                StatusCode::BAD_REQUEST,
                                "session_id is invalid",
                            ));
                        }
                    },
                    "last_seen_sequence" if !value.is_empty() => match value.parse::<u64>() {
                        Ok(sequence) => last_seen = Some(SequenceId(sequence)),
                        Err(_) => {
                            return Ok(error_response(
                                StatusCode::BAD_REQUEST,
                                "last_seen_sequence is not a decimal u64",
                            ));
                        }
                    },
                    _ => {}
                }
            }
            match state
                .engine
                .subscribe(client.client_id, session_id, last_seen)
                .await
            {
                Ok(receiver) => sse_response(receiver),
                Err(EventSubscriptionError::ReplayCursorAhead) => coded_error_response(
                    StatusCode::CONFLICT,
                    "replay_cursor_ahead",
                    "last seen sequence is ahead of the durable log",
                ),
                Err(EventSubscriptionError::Other(error)) => {
                    error_response(StatusCode::BAD_REQUEST, &error)
                }
            }
        }
        _ => error_response(StatusCode::NOT_FOUND, "unknown engine endpoint"),
    };
    Ok(response)
}

fn bearer(request: &Request<Incoming>) -> Option<&str> {
    request
        .headers()
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn authenticate_bootstrap(request: &Request<Incoming>, expected: &SecretToken) -> bool {
    bearer(request).is_some_and(|candidate| expected.matches_encoded(candidate))
}

fn authenticate_client(
    request: &Request<Incoming>,
    registry: &ClientAuthority,
) -> Option<AuthenticatedClient> {
    let client_id = request.headers().get(CLIENT_HEADER)?.to_str().ok()?;
    registry.authenticate(client_id, bearer(request)?)
}

fn requested_capability<B>(
    request: &Request<B>,
) -> std::result::Result<ClientCapability, &'static str> {
    match request.headers().get(CAPABILITY_HEADER) {
        None => Ok(ClientCapability::Interactive),
        Some(value) if value.as_bytes() == b"plugin_development" => {
            Ok(ClientCapability::PluginDevelopment)
        }
        Some(value) if value.as_bytes() == b"shell_broker" => Ok(ClientCapability::ShellBroker),
        Some(_) => Err("unknown engine client capability"),
    }
}

async fn dispatch_plugin_development(
    engine: &dyn ServerEngine,
    command: ClientCommand,
) -> std::result::Result<rw_core::HostReply, String> {
    if !matches!(
        command,
        ClientCommand::AttachDevelopmentPlugin { .. }
            | ClientCommand::DetachDevelopmentPlugin { .. }
    ) {
        return Ok(rw_core::HostReply::command(CommandOutcome::Rejected {
            error: EngineError {
                category: EngineErrorCategory::Protocol,
                code: "plugin_development_capability".to_owned(),
                message: "the plugin-development capability may only attach or detach a development plugin"
                    .to_owned(),
                retryable: false,
                details: None,
            },
        }));
    }
    engine
        .dispatch(command.meta().client_id.clone(), command)
        .await
}

async fn dispatch_shell_broker(
    engine: &dyn ServerEngine,
    command: ClientCommand,
) -> std::result::Result<CommandOutcome, String> {
    let ClientCommand::UserShellEnded {
        session_id,
        shell_id,
        status,
        captured_output,
        ..
    } = command
    else {
        return Ok(CommandOutcome::Rejected {
            error: EngineError {
                category: EngineErrorCategory::Protocol,
                code: "shell_broker_capability".to_owned(),
                message: "the shell-broker capability may only complete a foreground shell"
                    .to_owned(),
                retryable: false,
                details: None,
            },
        });
    };
    match engine
        .complete_shell(session_id, shell_id, status, captured_output)
        .await
    {
        Ok(()) => Ok(CommandOutcome::Accepted {}),
        Err(message) => Ok(CommandOutcome::Rejected {
            error: EngineError {
                category: EngineErrorCategory::Protocol,
                code: "shell_completion_rejected".to_owned(),
                message,
                retryable: false,
                details: None,
            },
        }),
    }
}

fn sse_response(
    mut receiver: mpsc::Receiver<std::result::Result<rw_core::HostEvent, String>>,
) -> Response<HttpBody> {
    let stream = async_stream::stream! {
        while let Some(item) = receiver.recv().await {
            match item {
                Ok(event) => {
                    let mut prefix = String::with_capacity(64);
                    if let Some(sequence) = event.sequence {
                        use std::fmt::Write as _;
                        let _ = writeln!(&mut prefix, "id: {}", sequence.0);
                    }
                    prefix.push_str("event: engine\ndata: ");
                    yield Ok::<Frame<Bytes>, Infallible>(Frame::data(Bytes::from(prefix)));
                    yield Ok::<Frame<Bytes>, Infallible>(Frame::data(event.json));
                    yield Ok::<Frame<Bytes>, Infallible>(Frame::data(Bytes::from_static(b"\n\n")));
                }
                Err(_) => break,
            }
        }
    };
    let body = StreamBody::new(stream).boxed_unsync();
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

fn json_response(status: StatusCode, bytes: impl Into<Bytes>) -> Response<HttpBody> {
    let mut response = Response::new(Full::new(bytes.into()).boxed_unsync());
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

fn error_response(status: StatusCode, message: &str) -> Response<HttpBody> {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "error": message,
    }))
    .unwrap_or_else(|_| br#"{"error":"transport failure"}"#.to_vec());
    json_response(status, bytes)
}

fn coded_error_response(status: StatusCode, code: &str, message: &str) -> Response<HttpBody> {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "error": {
            "code": code,
            "message": message,
        },
    }))
    .unwrap_or_else(|_| {
        br#"{"error":{"code":"transport_failure","message":"transport failure"}}"#.to_vec()
    });
    json_response(status, bytes)
}

fn unauthorized() -> Response<HttpBody> {
    error_response(StatusCode::UNAUTHORIZED, "engine authentication failed")
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests;
