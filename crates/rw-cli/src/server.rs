use std::{
    collections::{HashMap, HashSet},
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
    ClientCommand, ClientId, CommandOutcome, EngineError, EngineErrorCategory, EngineEvent,
    ProviderApiKey, ProviderApiKeySubmission, SequenceId, SessionId, ShellId,
};
use serde::{Deserialize, Serialize};
use tokio::{
    net::UnixListener,
    sync::{Notify, mpsc},
};

const COMMAND_BODY_LIMIT: usize = 2 * 1024 * 1024;
const PROVIDER_API_KEY_BODY_LIMIT: usize = 16 * 1024;
const PROVIDER_API_KEY_LIMIT: usize = 8 * 1024;
const MAX_PROVIDER_API_KEY_ATTEMPTS: usize = 256;
const HOST_EVENT_FORWARD_CAPACITY: usize = 256;
const CLIENT_HEADER: &str = "x-rottweiler-client";
const CAPABILITY_HEADER: &str = "x-rottweiler-capability";

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

#[derive(Debug)]
struct ClientRegistry {
    clients: Mutex<HashMap<ClientId, RegisteredClient>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClientCapability {
    Interactive,
    ShellBroker,
}

#[derive(Clone, Debug)]
struct RegisteredClient {
    token: SecretToken,
    capability: ClientCapability,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AuthenticatedClient {
    client_id: ClientId,
    capability: ClientCapability,
}

impl ClientRegistry {
    fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
        }
    }

    fn mint(&self, capability: ClientCapability) -> Result<ClientCredentials> {
        for _ in 0..32 {
            let token = SecretToken::generate()?;
            let client_id = ClientId(format!("client-{}", &token.encode()[..24]));
            let mut clients = self
                .clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if clients.contains_key(&client_id) {
                continue;
            }
            clients.insert(
                client_id.clone(),
                RegisteredClient {
                    token: token.clone(),
                    capability,
                },
            );
            return Ok(ClientCredentials {
                client_id,
                token: token.encode(),
            });
        }
        Err(miette!(
            "could not allocate a unique engine client identity"
        ))
    }

    fn authenticate(&self, client_id: &str, token: &str) -> Option<AuthenticatedClient> {
        let client_id = ClientId(client_id.to_owned());
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&client_id)
            .filter(|registered| registered.token.matches_encoded(token))
            .map(|registered| AuthenticatedClient {
                client_id,
                capability: registered.capability,
            })
    }
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
    ) -> std::result::Result<CommandOutcome, String>;

    async fn subscribe(
        &self,
        bound_client: ClientId,
        session_id: Option<SessionId>,
        last_seen: Option<SequenceId>,
    ) -> std::result::Result<mpsc::Receiver<std::result::Result<EngineEvent, String>>, String>;

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
    ) -> std::result::Result<ProviderApiKeySubmission, String> {
        Err("provider credential submission is unavailable".to_owned())
    }

    async fn activate_provider(
        &self,
        _bound_client: ClientId,
        _session_id: SessionId,
        _provider: String,
    ) -> std::result::Result<(), String> {
        Err("provider activation is unavailable".to_owned())
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
    ) -> std::result::Result<CommandOutcome, String> {
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
    ) -> std::result::Result<mpsc::Receiver<std::result::Result<EngineEvent, String>>, String> {
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
            .map_err(|error| error.to_string())?;
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
    clients: Arc<ClientRegistry>,
    shutdown_notifier: Arc<Notify>,
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
            clients: Arc::new(ClientRegistry::new()),
            shutdown_notifier: Arc::new(Notify::new()),
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
                let connection_state = state.clone();
                tokio::spawn(async move {
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
            if request
                .headers()
                .get(hyper::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<usize>().ok())
                .is_some_and(|length| length > COMMAND_BODY_LIMIT)
            {
                return Ok(error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "command body exceeds the transport limit",
                ));
            }
            let body = request.into_body();
            match Limited::new(body, COMMAND_BODY_LIMIT).collect().await {
                Ok(collected) => {
                    match serde_json::from_slice::<ClientCommand>(&collected.to_bytes()) {
                        Ok(mut command) => {
                            command.meta_mut().client_id = client.client_id.clone();
                            let shutdown_requested =
                                matches!(&command, ClientCommand::ShutdownHost { .. });
                            let outcome = if client.capability == ClientCapability::ShellBroker {
                                dispatch_shell_broker(&*state.engine, command).await
                            } else {
                                state.engine.dispatch(client.client_id, command).await
                            };
                            match outcome {
                                Ok(outcome) => {
                                    let accepted = outcome == CommandOutcome::Accepted;
                                    let mut response = match serde_json::to_vec(&outcome) {
                                        Ok(bytes) => json_response(StatusCode::ACCEPTED, bytes),
                                        Err(_) => error_response(
                                            StatusCode::INTERNAL_SERVER_ERROR,
                                            "command outcome could not serialize",
                                        ),
                                    };
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
                        Err(_) => error_response(
                            StatusCode::BAD_REQUEST,
                            "command body is not valid protocol JSON",
                        ),
                    }
                }
                Err(_) => error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "command body exceeds the transport limit",
                ),
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
                            if session_id.is_empty()
                                || session_id.len() > 512
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
                        }) if !session_id.is_empty()
                            && session_id.len() <= 512
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
                    "session_id" if !value.is_empty() => {
                        session_id = Some(SessionId(value.into_owned()));
                    }
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
                Err(error) => error_response(StatusCode::BAD_REQUEST, &error),
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
    registry: &ClientRegistry,
) -> Option<AuthenticatedClient> {
    let client_id = request.headers().get(CLIENT_HEADER)?.to_str().ok()?;
    registry.authenticate(client_id, bearer(request)?)
}

fn requested_capability(
    request: &Request<Incoming>,
) -> std::result::Result<ClientCapability, &'static str> {
    match request.headers().get(CAPABILITY_HEADER) {
        None => Ok(ClientCapability::Interactive),
        Some(value) if value.as_bytes() == b"shell_broker" => Ok(ClientCapability::ShellBroker),
        Some(_) => Err("unknown engine client capability"),
    }
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
        Ok(()) => Ok(CommandOutcome::Accepted),
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
    mut receiver: mpsc::Receiver<std::result::Result<EngineEvent, String>>,
) -> Response<HttpBody> {
    let stream = async_stream::stream! {
        while let Some(item) = receiver.recv().await {
            match item {
                Ok(event) => {
                    let mut frame = String::new();
                    if let Some(meta) = event.meta() {
                        use std::fmt::Write as _;
                        let _ = writeln!(&mut frame, "id: {}", meta.sequence_id.0);
                    }
                    frame.push_str("event: engine\n");
                    match serde_json::to_string(&event) {
                        Ok(data) => {
                            frame.push_str("data: ");
                            frame.push_str(&data);
                            frame.push_str("\n\n");
                            yield Ok::<Frame<Bytes>, Infallible>(Frame::data(Bytes::from(frame)));
                        }
                        Err(_) => break,
                    }
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

fn json_response(status: StatusCode, bytes: Vec<u8>) -> Response<HttpBody> {
    let mut response = Response::new(Full::new(Bytes::from(bytes)).boxed_unsync());
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

fn unauthorized() -> Response<HttpBody> {
    error_response(StatusCode::UNAUTHORIZED, "engine authentication failed")
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hyper::{Uri, client::conn::http1 as client_http1};
    use rw_core::{
        CommandAckMeta, CommandMeta, EngineError, EngineErrorCategory, EventMeta, PROTOCOL_VERSION,
        RequestId, SessionDescriptor, TurnId,
    };
    use rw_store::session::SessionEventLog;
    use tempfile::tempdir;
    use tokio::net::UnixStream;

    use super::*;

    #[derive(Default)]
    struct StubEngine {
        dispatches: AtomicUsize,
        received: Mutex<Vec<(ClientId, ClientCommand)>>,
        completions: Mutex<Vec<ShellCompletionFixture>>,
        provider_keys: Mutex<Vec<(ClientId, SessionId, String, String)>>,
    }

    type ShellCompletionFixture = (SessionId, ShellId, i32, Option<String>);

    #[async_trait]
    impl ServerEngine for StubEngine {
        async fn dispatch(
            &self,
            bound_client: ClientId,
            command: ClientCommand,
        ) -> std::result::Result<CommandOutcome, String> {
            self.dispatches.fetch_add(1, Ordering::Relaxed);
            self.received
                .lock()
                .expect("received commands")
                .push((bound_client, command.clone()));
            if matches!(command, ClientCommand::ShutdownHost { .. }) {
                return Ok(CommandOutcome::Accepted);
            }
            Ok(CommandOutcome::Rejected {
                error: EngineError {
                    category: EngineErrorCategory::Protocol,
                    code: "fixture".to_owned(),
                    message: "fixture".to_owned(),
                    retryable: false,
                    details: None,
                },
            })
        }

        async fn subscribe(
            &self,
            _bound_client: ClientId,
            _session_id: Option<SessionId>,
            _last_seen: Option<SequenceId>,
        ) -> std::result::Result<mpsc::Receiver<std::result::Result<EngineEvent, String>>, String>
        {
            let (send, receive) = mpsc::channel(1);
            send.send(Ok(EngineEvent::SessionsListed {
                meta: CommandAckMeta {
                    protocol_version: PROTOCOL_VERSION,
                    client_id: ClientId("fixture".to_owned()),
                    request_id: RequestId("fixture".to_owned()),
                    emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
                },
                sessions: Vec::<SessionDescriptor>::new(),
            }))
            .await
            .map_err(|_| "fixture receiver closed".to_owned())?;
            Ok(receive)
        }

        async fn complete_shell(
            &self,
            session_id: SessionId,
            shell_id: ShellId,
            status: i32,
            captured_output: Option<String>,
        ) -> std::result::Result<(), String> {
            self.completions.lock().expect("shell completions").push((
                session_id,
                shell_id,
                status,
                captured_output,
            ));
            Ok(())
        }

        async fn submit_provider_api_key(
            &self,
            bound_client: ClientId,
            session_id: SessionId,
            provider: String,
            api_key: ProviderApiKey,
        ) -> std::result::Result<ProviderApiKeySubmission, String> {
            self.provider_keys.lock().expect("provider keys").push((
                bound_client,
                session_id,
                provider,
                api_key.expose_secret().to_owned(),
            ));
            Ok(ProviderApiKeySubmission {
                stored: true,
                activated: true,
                warnings: vec!["fixture keychain warning".to_owned()],
            })
        }
    }

    async fn unix_request(socket: &Path, request: Request<Full<Bytes>>) -> Response<Incoming> {
        let stream = UnixStream::connect(socket).await.expect("connect test UDS");
        let (mut sender, connection) = client_http1::handshake(TokioIo::new(stream))
            .await
            .expect("HTTP handshake");
        tokio::spawn(async move {
            connection.await.expect("test HTTP connection");
        });
        sender.send_request(request).await.expect("send request")
    }

    fn request_builder(method: Method, path: &str) -> hyper::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(path.parse::<Uri>().expect("fixture URI"))
            .header(hyper::header::HOST, "localhost")
    }

    #[test]
    fn runtime_files_are_private_and_token_debug_is_redacted() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().expect("runtime root");
        let (runtime, _listener) = ServerRuntime::create(root.path()).expect("runtime");
        assert_eq!(
            fs::metadata(&runtime.paths.directory)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for path in [
            &runtime.paths.socket,
            &runtime.paths.token,
            &runtime.paths.descriptor,
        ] {
            assert_eq!(
                fs::metadata(path)
                    .expect("private metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let debug = format!("{runtime:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(&runtime.bootstrap.encode()));
        let descriptor = fs::read_to_string(&runtime.paths.descriptor).expect("descriptor");
        assert!(!descriptor.contains(&runtime.bootstrap.encode()));
    }

    #[test]
    fn supervisor_selected_runtime_paths_rotate_only_expected_stale_artifacts() {
        let root = tempdir().expect("runtime root");
        let directory = root.path().join("selected");
        fs::create_dir(&directory).expect("selected directory");
        set_mode(&directory, 0o700).expect("private selected directory");
        let paths = ServerRuntimePaths {
            socket: directory.join("engine.sock"),
            token: directory.join("auth.token"),
            descriptor: directory.join("runtime.json"),
            directory,
        };
        let (first, first_listener) =
            ServerRuntime::create_for_session(paths.clone(), Some("session-selected"))
                .expect("first selected runtime");
        let first_token = fs::read_to_string(&first.paths.token).expect("first token");
        let first_descriptor = fs::read_to_string(&first.paths.descriptor).expect("descriptor");
        assert!(first_descriptor.contains(r#""session_id":"session-selected""#));
        assert!(ServerRuntime::create_at(paths.clone()).is_err());
        assert_eq!(
            fs::read_to_string(&first.paths.token).expect("live token remains"),
            first_token
        );
        drop(first_listener);
        let (second, _second_listener) =
            ServerRuntime::create_at(paths).expect("restarted selected runtime");
        let second_token = fs::read_to_string(&second.paths.token).expect("second token");
        assert_ne!(first_token, second_token);
    }

    #[test]
    fn selected_runtime_refuses_symlink_artifacts() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("runtime root");
        let directory = root.path().join("selected");
        fs::create_dir(&directory).expect("selected directory");
        set_mode(&directory, 0o700).expect("private selected directory");
        let outside = root.path().join("outside");
        fs::write(&outside, b"do not replace").expect("outside fixture");
        let token = directory.join("auth.token");
        symlink(&outside, &token).expect("token symlink");
        let paths = ServerRuntimePaths {
            socket: directory.join("engine.sock"),
            token,
            descriptor: directory.join("runtime.json"),
            directory,
        };
        assert!(ServerRuntime::create_at(paths).is_err());
        assert_eq!(
            fs::read(&outside).expect("outside remains"),
            b"do not replace"
        );
    }

    #[test]
    fn client_tokens_are_bound_to_server_minted_ids() {
        let registry = ClientRegistry::new();
        let first = registry
            .mint(ClientCapability::Interactive)
            .expect("first client");
        let second = registry
            .mint(ClientCapability::ShellBroker)
            .expect("second client");
        assert_eq!(
            registry.authenticate(&first.client_id.0, &first.token),
            Some(AuthenticatedClient {
                client_id: first.client_id.clone(),
                capability: ClientCapability::Interactive,
            })
        );
        assert_eq!(
            registry.authenticate(&first.client_id.0, &second.token),
            None
        );
        assert_eq!(
            registry.authenticate(&second.client_id.0, &first.token),
            None
        );
    }

    #[test]
    fn protocol_fixture_command_has_transport_overwritable_meta() {
        let mut command = ClientCommand::ListSessions {
            meta: CommandMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId("spoofed".to_owned()),
                request_id: RequestId("request".to_owned()),
            },
        };
        command.meta_mut().client_id = ClientId("bound".to_owned());
        assert_eq!(command.meta().client_id.0, "bound");
    }

    #[test]
    fn provider_api_key_request_debug_is_redacted() {
        let canary = "rw-secret-canary-never-debug";
        let request = ProviderApiKeyRequest {
            session_id: "session".to_owned(),
            provider: "openai".to_owned(),
            api_key: canary.to_owned(),
        };
        let debug = format!("{request:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(canary));
    }

    #[test]
    fn provider_api_key_attempt_guard_is_bounded_and_drop_cleans_reservation() {
        let attempts = Arc::new(Mutex::new(HashSet::new()));
        let first =
            ProviderApiKeyAttemptGuard::reserve(Arc::clone(&attempts), "company-openai".to_owned())
                .expect("first reservation");
        assert!(ProviderApiKeyAttemptGuard::reserve(
            Arc::clone(&attempts),
            "company-openai".to_owned(),
        )
        .is_none());
        drop(first);
        assert!(
            ProviderApiKeyAttemptGuard::reserve(attempts, "company-openai".to_owned(),).is_some()
        );
    }

    #[tokio::test]
    async fn provider_api_key_uses_authenticated_non_protocol_channel_and_sanitized_response() {
        let root = tempdir().expect("runtime root");
        let (runtime, listener) = ServerRuntime::create(root.path()).expect("runtime");
        let engine = Arc::new(StubEngine::default());
        let state = ServerState::new(engine.clone(), &runtime);
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(serve(listener, state, shutdown_rx));
        let bootstrap = fs::read_to_string(&runtime.paths.token).expect("bootstrap token");
        let connected = unix_request(
            &runtime.paths.socket,
            request_builder(Method::POST, "/v1/connect")
                .header(AUTHORIZATION, format!("Bearer {}", bootstrap.trim()))
                .body(Full::new(Bytes::new()))
                .expect("connect request"),
        )
        .await;
        let credentials: ClientCredentials = serde_json::from_slice(
            &connected
                .into_body()
                .collect()
                .await
                .expect("connect body")
                .to_bytes(),
        )
        .expect("credentials");
        let canary = "rw-secret-canary-never-wire-back";
        let body = serde_json::to_vec(&serde_json::json!({
            "session_id": "session-secret",
            "provider": "company-openai",
            "api_key": canary,
        }))
        .expect("secret body");
        let response = unix_request(
            &runtime.paths.socket,
            request_builder(Method::POST, "/v1/provider-api-key")
                .header(AUTHORIZATION, format!("Bearer {}", credentials.token))
                .header(CLIENT_HEADER, &credentials.client_id.0)
                .header(CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(body)))
                .expect("credential request"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let response_bytes = response
            .into_body()
            .collect()
            .await
            .expect("response")
            .to_bytes();
        assert!(
            !response_bytes
                .windows(canary.len())
                .any(|window| window == canary.as_bytes())
        );
        assert!(String::from_utf8_lossy(&response_bytes).contains("fixture keychain warning"));
        {
            let keys = engine.provider_keys.lock().expect("provider keys");
            assert_eq!(keys.len(), 1);
            assert_eq!(keys[0].0, credentials.client_id);
            assert_eq!(keys[0].1.0, "session-secret");
            assert_eq!(keys[0].2, "company-openai");
            assert_eq!(keys[0].3, canary);
        }
        assert!(
            !serde_json::to_string(&ClientCommand::ListSessions {
                meta: CommandMeta {
                    protocol_version: PROTOCOL_VERSION,
                    client_id: ClientId("client".to_owned()),
                    request_id: RequestId("request".to_owned()),
                },
            })
            .expect("protocol JSON")
            .contains(canary)
        );
        shutdown.send(true).expect("shutdown");
        server.await.expect("server task").expect("server result");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn uds_auth_mints_bound_identity_and_overwrites_spoofed_meta() {
        let root = tempdir().expect("runtime root");
        let (runtime, listener) = ServerRuntime::create(root.path()).expect("runtime");
        let engine = Arc::new(StubEngine::default());
        let state = ServerState::new(engine.clone(), &runtime);
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(serve(listener, state, shutdown_rx));

        let missing = unix_request(
            &runtime.paths.socket,
            request_builder(Method::POST, "/v1/connect")
                .body(Full::new(Bytes::new()))
                .expect("missing-auth request"),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        let bootstrap = fs::read_to_string(&runtime.paths.token).expect("bootstrap token");
        let connected = unix_request(
            &runtime.paths.socket,
            request_builder(Method::POST, "/v1/connect")
                .header(AUTHORIZATION, format!("Bearer {}", bootstrap.trim()))
                .body(Full::new(Bytes::new()))
                .expect("connect request"),
        )
        .await;
        assert_eq!(connected.status(), StatusCode::CREATED);
        let credentials: ClientCredentials = serde_json::from_slice(
            &connected
                .into_body()
                .collect()
                .await
                .expect("connect body")
                .to_bytes(),
        )
        .expect("client credentials");

        let command = ClientCommand::ListSessions {
            meta: CommandMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId("spoofed-driver".to_owned()),
                request_id: RequestId("list".to_owned()),
            },
        };
        let command_bytes = serde_json::to_vec(&command).expect("command JSON");
        let wrong_token = unix_request(
            &runtime.paths.socket,
            request_builder(Method::POST, "/v1/command")
                .header(AUTHORIZATION, format!("Bearer {bootstrap}"))
                .header(CLIENT_HEADER, &credentials.client_id.0)
                .body(Full::new(Bytes::from(command_bytes.clone())))
                .expect("wrong-token command"),
        )
        .await;
        assert_eq!(wrong_token.status(), StatusCode::UNAUTHORIZED);

        let accepted = unix_request(
            &runtime.paths.socket,
            request_builder(Method::POST, "/v1/command")
                .header(AUTHORIZATION, format!("Bearer {}", credentials.token))
                .header(CLIENT_HEADER, &credentials.client_id.0)
                .body(Full::new(Bytes::from(command_bytes)))
                .expect("bound command"),
        )
        .await;
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);
        {
            let received = engine.received.lock().expect("received command");
            assert_eq!(received.len(), 1);
            assert_eq!(received[0].0, credentials.client_id);
            assert_eq!(received[0].1.meta().client_id, credentials.client_id);
        }

        let broker_connected = unix_request(
            &runtime.paths.socket,
            request_builder(Method::POST, "/v1/connect")
                .header(AUTHORIZATION, format!("Bearer {}", bootstrap.trim()))
                .header(CAPABILITY_HEADER, "shell_broker")
                .body(Full::new(Bytes::new()))
                .expect("broker connect"),
        )
        .await;
        assert_eq!(broker_connected.status(), StatusCode::CREATED);
        let broker: ClientCredentials = serde_json::from_slice(
            &broker_connected
                .into_body()
                .collect()
                .await
                .expect("broker connect body")
                .to_bytes(),
        )
        .expect("broker credentials");
        let completion = ClientCommand::UserShellEnded {
            meta: CommandMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId("spoofed-broker".to_owned()),
                request_id: RequestId("complete-shell".to_owned()),
            },
            session_id: SessionId("session-tty".to_owned()),
            shell_id: ShellId("shell-9".to_owned()),
            status: 130,
            captured_output: Some("interrupted".to_owned()),
        };
        let trusted_completion = unix_request(
            &runtime.paths.socket,
            request_builder(Method::POST, "/v1/command")
                .header(AUTHORIZATION, format!("Bearer {}", broker.token))
                .header(CLIENT_HEADER, &broker.client_id.0)
                .body(Full::new(Bytes::from(
                    serde_json::to_vec(&completion).expect("completion JSON"),
                )))
                .expect("trusted shell completion"),
        )
        .await;
        assert_eq!(trusted_completion.status(), StatusCode::ACCEPTED);
        assert_eq!(
            engine
                .completions
                .lock()
                .expect("shell completions")
                .as_slice(),
            [(
                SessionId("session-tty".to_owned()),
                ShellId("shell-9".to_owned()),
                130,
                Some("interrupted".to_owned()),
            )]
        );

        let events = unix_request(
            &runtime.paths.socket,
            request_builder(Method::GET, "/v1/events")
                .header(AUTHORIZATION, format!("Bearer {}", credentials.token))
                .header(CLIENT_HEADER, &credentials.client_id.0)
                .header(ACCEPT, "text/event-stream")
                .body(Full::new(Bytes::new()))
                .expect("events request"),
        )
        .await;
        assert_eq!(events.status(), StatusCode::OK);
        let event_body = events
            .into_body()
            .collect()
            .await
            .expect("event body")
            .to_bytes();
        let event_body = String::from_utf8(event_body.to_vec()).expect("UTF-8 SSE");
        assert!(event_body.contains("event: engine"));
        assert!(event_body.contains("\"type\":\"sessions_listed\""));

        shutdown.send(true).expect("stop server");
        server.await.expect("server join").expect("server result");
    }

    #[tokio::test]
    async fn accepted_shutdown_host_stops_the_transport_listener() {
        let root = tempdir().expect("runtime root");
        let (runtime, listener) = ServerRuntime::create(root.path()).expect("runtime");
        let engine = Arc::new(StubEngine::default());
        let state = ServerState::new(engine.clone(), &runtime);
        let (_shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(serve(listener, state, shutdown_rx));
        let bootstrap = fs::read_to_string(&runtime.paths.token).expect("bootstrap token");

        crate::remote::shutdown_authenticated_host(
            &runtime.paths.socket,
            bootstrap.trim(),
            std::time::Duration::from_secs(1),
        )
        .await
        .expect("attached remote shutdown");
        tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .expect("server must stop after accepted host shutdown")
            .expect("server task")
            .expect("server result");
        assert!(matches!(
            engine
                .received
                .lock()
                .expect("received commands")
                .last()
                .map(|(_, command)| command),
            Some(ClientCommand::ShutdownHost { .. })
        ));
    }

    #[tokio::test]
    async fn remote_auth_canaries_never_enter_events_persistence_or_diagnostics() {
        let root = tempdir().expect("runtime root");
        let (runtime, listener) = ServerRuntime::create(root.path()).expect("runtime");
        let bootstrap = fs::read_to_string(&runtime.paths.token).expect("bootstrap token");
        let engine = Arc::new(StubEngine::default());
        let state = ServerState::new(engine, &runtime);
        let initial_diagnostics = format!("{runtime:?} {state:?}");
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(serve(listener, state, shutdown_rx));

        let connected = unix_request(
            &runtime.paths.socket,
            request_builder(Method::POST, "/v1/connect")
                .header(AUTHORIZATION, format!("Bearer {}", bootstrap.trim()))
                .body(Full::new(Bytes::new()))
                .expect("connect request"),
        )
        .await;
        assert_eq!(connected.status(), StatusCode::CREATED);
        let credentials: ClientCredentials = serde_json::from_slice(
            &connected
                .into_body()
                .collect()
                .await
                .expect("connect body")
                .to_bytes(),
        )
        .expect("client credentials");

        let events = unix_request(
            &runtime.paths.socket,
            request_builder(Method::GET, "/v1/events")
                .header(AUTHORIZATION, format!("Bearer {}", credentials.token))
                .header(CLIENT_HEADER, &credentials.client_id.0)
                .header(ACCEPT, "text/event-stream")
                .body(Full::new(Bytes::new()))
                .expect("events request"),
        )
        .await;
        assert_eq!(events.status(), StatusCode::OK);
        let event_body = String::from_utf8(
            events
                .into_body()
                .collect()
                .await
                .expect("event body")
                .to_bytes()
                .to_vec(),
        )
        .expect("UTF-8 SSE");
        let streamed_json = event_body
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("serialized SSE event");
        let streamed_event: EngineEvent =
            serde_json::from_str(streamed_json).expect("streamed EngineEvent");
        let reserialized_streamed =
            serde_json::to_string(&streamed_event).expect("streamed event serializes");

        let durable_event = EngineEvent::TurnStarted {
            meta: EventMeta {
                protocol_version: PROTOCOL_VERSION,
                session_id: SessionId("remote-canary-session".to_owned()),
                sequence_id: SequenceId(0),
                emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
                caused_by: None,
            },
            turn_id: TurnId("1".to_owned()),
        };
        let serialized_durable =
            serde_json::to_string(&durable_event).expect("durable event serializes");
        let mut log =
            SessionEventLog::open(root.path(), "remote-canary-session").expect("session event log");
        log.append_expected(SequenceId(0), durable_event.clone())
            .expect("persist durable event");
        let persisted = fs::read_to_string(log.path()).expect("events.jsonl");
        let diagnostics = format!("{initial_diagnostics} {streamed_event:?} {durable_event:?}");

        for canary in [bootstrap.trim(), credentials.token.as_str()] {
            for artifact in [
                event_body.as_str(),
                reserialized_streamed.as_str(),
                serialized_durable.as_str(),
                persisted.as_str(),
                diagnostics.as_str(),
            ] {
                assert!(
                    !artifact.contains(canary),
                    "remote authentication canary entered an event or diagnostic"
                );
            }
        }

        shutdown.send(true).expect("stop server");
        server.await.expect("server join").expect("server result");
    }
}
