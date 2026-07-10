use std::{
    collections::HashMap,
    convert::Infallible,
    fmt,
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use http_body_util::{BodyExt as _, Full, Limited, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Frame, Incoming},
    header::{ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use miette::{IntoDiagnostic as _, Result, miette};
use rw_core::{ClientCommand, ClientId, CommandOutcome, EngineEvent, SequenceId, SessionId};
use serde::{Deserialize, Serialize};
use tokio::{net::UnixListener, sync::mpsc};

const COMMAND_BODY_LIMIT: usize = 2 * 1024 * 1024;
const HOST_EVENT_FORWARD_CAPACITY: usize = 256;
const CLIENT_HEADER: &str = "x-rottweiler-client";

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
        let bootstrap = SecretToken::generate()?;
        write_private_new(&paths.token, bootstrap.encode().as_bytes())?;
        let descriptor = serde_json::to_vec(&RuntimeDescriptor {
            version: 1,
            pid: std::process::id(),
            socket: &paths.socket,
            token_file: &paths.token,
        })
        .into_diagnostic()?;
        write_private_new(&paths.descriptor, &descriptor)?;

        let listener = std::os::unix::net::UnixListener::bind(&paths.socket).into_diagnostic()?;
        set_mode(&paths.socket, 0o600)?;
        listener.set_nonblocking(true).into_diagnostic()?;
        Ok((Self { paths, bootstrap }, listener))
    }

    fn bootstrap(&self) -> &SecretToken {
        &self.bootstrap
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(miette!("server runtime root is not a real directory"));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).into_diagnostic()?;
        }
        Err(error) => return Err(error).into_diagnostic(),
    }
    set_mode(path, 0o700)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).into_diagnostic()
}

fn write_private_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .into_diagnostic()?;
    file.write_all(bytes).into_diagnostic()?;
    file.flush().into_diagnostic()?;
    file.sync_all().into_diagnostic()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClientCredentials {
    pub client_id: ClientId,
    pub token: String,
}

#[derive(Debug)]
struct ClientRegistry {
    clients: Mutex<HashMap<ClientId, SecretToken>>,
}

impl ClientRegistry {
    fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
        }
    }

    fn mint(&self) -> Result<ClientCredentials> {
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
            clients.insert(client_id.clone(), token.clone());
            return Ok(ClientCredentials {
                client_id,
                token: token.encode(),
            });
        }
        Err(miette!(
            "could not allocate a unique engine client identity"
        ))
    }

    fn authenticate(&self, client_id: &str, token: &str) -> Option<ClientId> {
        let client_id = ClientId(client_id.to_owned());
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&client_id)
            .filter(|expected| expected.matches_encoded(token))
            .map(|_| client_id)
    }
}

/// Protocol host boundary consumed by the HTTP/SSE transport.
#[async_trait]
pub trait ServerEngine: Send + Sync + 'static {
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
}

#[derive(Clone)]
pub struct ServerState {
    engine: Arc<dyn ServerEngine>,
    bootstrap: SecretToken,
    clients: Arc<ClientRegistry>,
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
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted.into_diagnostic()?;
                let connection_state = state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        handle_request(request, connection_state.clone())
                    });
                    if let Err(error) = http1::Builder::new()
                        .keep_alive(true)
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        tracing::debug!(reason = %error, "engine client connection closed");
                    }
                });
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_request(
    request: Request<Incoming>,
    state: ServerState,
) -> std::result::Result<Response<HttpBody>, Infallible> {
    let response = match (request.method(), request.uri().path()) {
        (&Method::POST, "/v1/connect") => {
            if authenticate_bootstrap(&request, &state.bootstrap) {
                match state
                    .clients
                    .mint()
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
                json_response(StatusCode::OK, br#"{"ready":true}"#.to_vec())
            } else {
                unauthorized()
            }
        }
        (&Method::POST, "/v1/command") => {
            let Some(client_id) = authenticate_client(&request, &state.clients) else {
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
                            command.meta_mut().client_id = client_id.clone();
                            match state.engine.dispatch(client_id, command).await {
                                Ok(outcome) => match serde_json::to_vec(&outcome) {
                                    Ok(bytes) => json_response(StatusCode::ACCEPTED, bytes),
                                    Err(_) => error_response(
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        "command outcome could not serialize",
                                    ),
                                },
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
        (&Method::GET, "/v1/events") => {
            let Some(client_id) = authenticate_client(&request, &state.clients) else {
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
                .subscribe(client_id, session_id, last_seen)
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

fn authenticate_client(request: &Request<Incoming>, registry: &ClientRegistry) -> Option<ClientId> {
    let client_id = request.headers().get(CLIENT_HEADER)?.to_str().ok()?;
    registry.authenticate(client_id, bearer(request)?)
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
        CommandAckMeta, CommandMeta, EngineError, EngineErrorCategory, PROTOCOL_VERSION, RequestId,
        SessionDescriptor,
    };
    use tempfile::tempdir;
    use tokio::net::UnixStream;

    use super::*;

    #[derive(Default)]
    struct StubEngine {
        dispatches: AtomicUsize,
        received: Mutex<Vec<(ClientId, ClientCommand)>>,
    }

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
                .push((bound_client, command));
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
    fn client_tokens_are_bound_to_server_minted_ids() {
        let registry = ClientRegistry::new();
        let first = registry.mint().expect("first client");
        let second = registry.mint().expect("second client");
        assert_eq!(
            registry.authenticate(&first.client_id.0, &first.token),
            Some(first.client_id.clone())
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

    #[tokio::test]
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
}
