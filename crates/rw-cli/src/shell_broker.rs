//! Authenticated foreground-shell broker owned by the supervising `rw` process.

use std::{
    collections::HashSet,
    io,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::{
    Method, Request, StatusCode,
    body::{Bytes, Incoming},
    client::conn::http1 as client_http1,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE},
};
use hyper_util::rt::TokioIo;
use rw_core::{
    ClientCommand, CommandMeta, CommandOutcome, EngineEvent, PROTOCOL_VERSION, RequestId,
    SequenceId, SessionId, ShellId,
};
use serde::de::DeserializeOwned;
use tokio::{
    net::UnixStream,
    sync::{Mutex, oneshot},
};

use crate::{
    server::ClientCredentials,
    tty::{
        OutputRedactor, ShellCompletionGate, TokioTerminalSpawner, TtyError, UnixTerminalSignals,
        remote_tty_argv, run_after_durable_shell_start, run_argv_after_durable_shell_start,
    },
};

const CLIENT_HEADER: &str = "x-rottweiler-client";
const CAPABILITY_HEADER: &str = "x-rottweiler-capability";
const SHELL_BROKER_CAPABILITY: &str = "shell_broker";
const MAX_CONTROL_BODY_BYTES: usize = 64 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellTarget {
    Local,
    Remote { host: String },
}

#[derive(Clone, Debug)]
pub struct ShellBrokerConfig {
    pub socket: PathBuf,
    pub token_file: PathBuf,
    pub session_id: SessionId,
    pub target: ShellTarget,
}

#[derive(Debug)]
pub enum ShellBrokerError {
    Io(io::Error),
    Transport(&'static str),
    Protocol(String),
    Tty(TtyError),
}

impl std::fmt::Display for ShellBrokerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "foreground-shell broker I/O failure: {error}"),
            Self::Transport(message) => {
                write!(
                    formatter,
                    "foreground-shell broker transport failure: {message}"
                )
            }
            Self::Protocol(message) => {
                write!(
                    formatter,
                    "foreground-shell broker protocol failure: {message}"
                )
            }
            Self::Tty(error) => write!(formatter, "foreground-shell handover failed: {error}"),
        }
    }
}

impl std::error::Error for ShellBrokerError {}

impl From<io::Error> for ShellBrokerError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Runs the parent-side broker until its authenticated SSE stream ends with a
/// non-recoverable error. `ready` resolves only after the broker is subscribed,
/// so the supervisor can start the TUI without an active-event race.
pub async fn run(
    config: ShellBrokerConfig,
    ready: oneshot::Sender<Result<(), String>>,
) -> Result<(), ShellBrokerError> {
    let mut ready = Some(ready);
    let result = run_inner(&config, &mut ready).await;
    if let (Err(error), Some(ready)) = (&result, ready.take()) {
        let _ = ready.send(Err(error.to_string()));
    }
    result
}

async fn run_inner(
    config: &ShellBrokerConfig,
    ready: &mut Option<oneshot::Sender<Result<(), String>>>,
) -> Result<(), ShellBrokerError> {
    let mut cursor = None;
    let mut replay = ReplayProjection::default();
    loop {
        let bootstrap = match read_bootstrap_token(&config.token_file) {
            Ok(token) => token,
            Err(ShellBrokerError::Io(_) | ShellBrokerError::Transport(_)) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        let client =
            match BrokerClient::connect(&config.socket, &bootstrap, &config.session_id).await {
                Ok(client) => client,
                Err(ShellBrokerError::Io(_) | ShellBrokerError::Transport(_)) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
        replay.begin_replay();
        let response = match client.subscribe(cursor).await {
            Ok(response) => response,
            Err(ShellBrokerError::Protocol(message)) if message == "session_not_ready" => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            Err(ShellBrokerError::Io(_) | ShellBrokerError::Transport(_)) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        if let Some(sender) = ready.take() {
            let _ = sender.send(Ok(()));
        }

        let gate = BrokerCompletionGate::new(config, client.clone());
        let mut body = response;
        let mut parser = SseFrames::default();
        loop {
            let Ok(Some(frame)) = body.frame().await.transpose() else {
                break;
            };
            let Ok(data) = frame.into_data() else {
                continue;
            };
            for event in parser.push(&data)? {
                if let Some(meta) = event.meta() {
                    cursor = Some(meta.sequence_id);
                }
                if let Some(launch) = replay.observe(&event) {
                    match run_launch(config, &gate, launch).await {
                        Ok(()) | Err(ShellBrokerError::Tty(TtyError::Spawn(_))) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn run_launch(
    config: &ShellBrokerConfig,
    completion: &impl ShellCompletionGate,
    launch: ShellLaunch,
) -> Result<(), ShellBrokerError> {
    let mut signals = UnixTerminalSignals::new()?;
    let spawner = TokioTerminalSpawner::default();
    let redactor = BrokerOutputRedactor;
    let result = match &config.target {
        ShellTarget::Local => {
            run_after_durable_shell_start(
                &launch.command,
                launch.shell_id,
                completion,
                &spawner,
                &mut signals,
                &redactor,
            )
            .await
        }
        ShellTarget::Remote { host } => {
            let argv = remote_tty_argv(host, &launch.command).map_err(|message| {
                ShellBrokerError::Protocol(format!("invalid remote foreground command: {message}"))
            })?;
            run_argv_after_durable_shell_start(
                &argv,
                launch.shell_id,
                completion,
                &spawner,
                &mut signals,
                &redactor,
            )
            .await
        }
    };
    result.map(|_| ()).map_err(ShellBrokerError::Tty)
}

#[derive(Clone)]
struct BrokerClient {
    socket: PathBuf,
    credentials: ClientCredentials,
    session_id: SessionId,
}

impl BrokerClient {
    async fn connect(
        socket: &Path,
        bootstrap: &str,
        session_id: &SessionId,
    ) -> Result<Self, ShellBrokerError> {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/connect")
            .header(hyper::header::HOST, "localhost")
            .header(AUTHORIZATION, format!("Bearer {bootstrap}"))
            .header(CAPABILITY_HEADER, SHELL_BROKER_CAPABILITY)
            .body(Full::new(Bytes::new()))
            .map_err(|_| ShellBrokerError::Transport("could not build broker handshake"))?;
        let response = unix_request(socket, request).await?;
        if response.status() != StatusCode::CREATED {
            return Err(ShellBrokerError::Transport(
                "engine rejected broker authentication",
            ));
        }
        let credentials = collect_json(response.into_body()).await?;
        Ok(Self {
            socket: socket.to_owned(),
            credentials,
            session_id: session_id.clone(),
        })
    }

    async fn subscribe(&self, last_seen: Option<SequenceId>) -> Result<Incoming, ShellBrokerError> {
        let query = {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            serializer.append_pair("session_id", &self.session_id.0);
            if let Some(sequence) = last_seen {
                serializer.append_pair("last_seen_sequence", &sequence.0.to_string());
            }
            serializer.finish()
        };
        let request = Request::builder()
            .method(Method::GET)
            .uri(format!("/v1/events?{query}"))
            .header(hyper::header::HOST, "localhost")
            .header(AUTHORIZATION, format!("Bearer {}", self.credentials.token))
            .header(CLIENT_HEADER, &self.credentials.client_id.0)
            .header(ACCEPT, "text/event-stream")
            .body(Full::new(Bytes::new()))
            .map_err(|_| ShellBrokerError::Transport("could not build broker subscription"))?;
        let response = unix_request(&self.socket, request).await?;
        match response.status() {
            StatusCode::OK => Ok(response.into_body()),
            StatusCode::BAD_REQUEST => {
                Err(ShellBrokerError::Protocol("session_not_ready".to_owned()))
            }
            _ => Err(ShellBrokerError::Transport(
                "engine rejected broker event subscription",
            )),
        }
    }

    async fn complete(
        &self,
        request_id: &RequestId,
        shell_id: &ShellId,
        status: i32,
        captured_output: Option<&str>,
    ) -> Result<(), ShellBrokerError> {
        let command = ClientCommand::UserShellEnded {
            meta: CommandMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: self.credentials.client_id.clone(),
                request_id: request_id.clone(),
            },
            session_id: self.session_id.clone(),
            shell_id: shell_id.clone(),
            status,
            captured_output: captured_output.map(str::to_owned),
        };
        let body = serde_json::to_vec(&command)
            .map_err(|_| ShellBrokerError::Protocol("completion could not serialize".to_owned()))?;
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/command")
            .header(hyper::header::HOST, "localhost")
            .header(AUTHORIZATION, format!("Bearer {}", self.credentials.token))
            .header(CLIENT_HEADER, &self.credentials.client_id.0)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| ShellBrokerError::Transport("could not build shell completion"))?;
        let response = unix_request(&self.socket, request).await?;
        if response.status() != StatusCode::ACCEPTED {
            return Err(ShellBrokerError::Transport(
                "engine rejected shell completion transport",
            ));
        }
        let outcome: CommandOutcome = collect_json(response.into_body()).await?;
        match outcome {
            CommandOutcome::Accepted => Ok(()),
            CommandOutcome::Rejected { error } => Err(ShellBrokerError::Protocol(format!(
                "shell completion was rejected: {}",
                error.code
            ))),
        }
    }
}

struct BrokerCompletionGate {
    socket: PathBuf,
    token_file: PathBuf,
    session_id: SessionId,
    client: Mutex<Option<BrokerClient>>,
}

impl BrokerCompletionGate {
    fn new(config: &ShellBrokerConfig, client: BrokerClient) -> Self {
        Self {
            socket: config.socket.clone(),
            token_file: config.token_file.clone(),
            session_id: config.session_id.clone(),
            client: Mutex::new(Some(client)),
        }
    }

    async fn connected_client(&self) -> Result<BrokerClient, ShellBrokerError> {
        if let Some(client) = self.client.lock().await.clone() {
            return Ok(client);
        }
        let bootstrap = read_bootstrap_token(&self.token_file)?;
        let client = BrokerClient::connect(&self.socket, &bootstrap, &self.session_id).await?;
        *self.client.lock().await = Some(client.clone());
        Ok(client)
    }

    async fn invalidate(&self, client: &BrokerClient) {
        let mut current = self.client.lock().await;
        if current
            .as_ref()
            .is_some_and(|value| value.credentials.client_id == client.credentials.client_id)
        {
            *current = None;
        }
    }
}

#[async_trait]
impl ShellCompletionGate for BrokerCompletionGate {
    async fn shell_ended(
        &self,
        shell_id: ShellId,
        status: i32,
        captured_output: Option<String>,
    ) -> io::Result<()> {
        let request_id = RequestId(format!(
            "shell-end-{}",
            blake3::hash(shell_id.0.as_bytes()).to_hex()
        ));
        loop {
            let client = match self.connected_client().await {
                Ok(client) => client,
                Err(ShellBrokerError::Io(_) | ShellBrokerError::Transport(_)) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
                Err(error) => return Err(io::Error::other(error)),
            };
            if client
                .complete(&request_id, &shell_id, status, captured_output.as_deref())
                .await
                .is_ok()
            {
                return Ok(());
            }
            match shell_replay_state(&client, &shell_id).await {
                Ok(ShellReplayState::Completed) => return Ok(()),
                Ok(ShellReplayState::Active | ShellReplayState::Missing) => {}
                Err(_) => self.invalidate(&client).await,
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellReplayState {
    Missing,
    Active,
    Completed,
}

async fn shell_replay_state(
    client: &BrokerClient,
    shell_id: &ShellId,
) -> Result<ShellReplayState, ShellBrokerError> {
    let mut body = client.subscribe(None).await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut parser = SseFrames::default();
        let mut state = ShellReplayState::Missing;
        loop {
            let frame = body
                .frame()
                .await
                .transpose()
                .map_err(|_| ShellBrokerError::Transport("shell replay stream failed"))?
                .ok_or(ShellBrokerError::Transport(
                    "shell replay stream ended before its durable tail",
                ))?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            for event in parser.push(&data)? {
                if let Some(completed) = observe_shell_replay(&mut state, &event, shell_id) {
                    return Ok(completed);
                }
            }
        }
    })
    .await
    .map_err(|_| ShellBrokerError::Transport("shell replay confirmation timed out"))?
}

fn observe_shell_replay(
    state: &mut ShellReplayState,
    event: &EngineEvent,
    shell_id: &ShellId,
) -> Option<ShellReplayState> {
    match event {
        EngineEvent::UserShellStateChanged {
            shell_id: event_shell,
            active,
            ..
        } if event_shell == shell_id => {
            *state = if *active {
                ShellReplayState::Active
            } else {
                ShellReplayState::Completed
            };
            None
        }
        EngineEvent::SessionReplayCompleted { .. } => Some(*state),
        _ => None,
    }
}

async fn unix_request(
    socket: &Path,
    request: Request<Full<Bytes>>,
) -> Result<hyper::Response<Incoming>, ShellBrokerError> {
    let stream = UnixStream::connect(socket).await?;
    let (mut sender, connection) = client_http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| ShellBrokerError::Transport("engine HTTP handshake failed"))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    sender
        .send_request(request)
        .await
        .map_err(|_| ShellBrokerError::Transport("engine HTTP request failed"))
}

async fn collect_json<T: DeserializeOwned>(body: Incoming) -> Result<T, ShellBrokerError> {
    let bytes = Limited::new(body, MAX_CONTROL_BODY_BYTES)
        .collect()
        .await
        .map_err(|_| ShellBrokerError::Transport("engine control response exceeded its limit"))?
        .to_bytes();
    serde_json::from_slice(&bytes)
        .map_err(|_| ShellBrokerError::Protocol("engine control response was invalid".to_owned()))
}

fn read_bootstrap_token(path: &Path) -> Result<String, ShellBrokerError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() != 64
    {
        return Err(ShellBrokerError::Transport(
            "engine token handoff is not one owner-private regular file",
        ));
    }
    let token = std::fs::read_to_string(path)?;
    let token = token.trim();
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ShellBrokerError::Transport(
            "engine token handoff is malformed",
        ));
    }
    Ok(token.to_owned())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ShellLaunch {
    shell_id: ShellId,
    command: String,
}

struct ReplayProjection {
    replaying: bool,
    projected_active: Option<ShellLaunch>,
    launched: HashSet<ShellId>,
}

impl ReplayProjection {
    fn begin_replay(&mut self) {
        self.replaying = true;
    }

    fn observe(&mut self, event: &EngineEvent) -> Option<ShellLaunch> {
        match event {
            EngineEvent::UserShellStateChanged {
                shell_id,
                command,
                active: true,
                ..
            } => {
                let command = command
                    .as_ref()
                    .filter(|command| !command.trim().is_empty())?;
                let launch = ShellLaunch {
                    shell_id: shell_id.clone(),
                    command: command.clone(),
                };
                self.projected_active = Some(launch.clone());
                if self.replaying || !self.launched.insert(shell_id.clone()) {
                    None
                } else {
                    Some(launch)
                }
            }
            EngineEvent::UserShellStateChanged {
                shell_id,
                active: false,
                ..
            } => {
                if self
                    .projected_active
                    .as_ref()
                    .is_some_and(|active| active.shell_id == *shell_id)
                {
                    self.projected_active = None;
                }
                None
            }
            EngineEvent::SessionReplayCompleted { .. } => {
                self.replaying = false;
                let launch = self.projected_active.clone()?;
                self.launched
                    .insert(launch.shell_id.clone())
                    .then_some(launch)
            }
            _ => None,
        }
    }
}

impl Default for ReplayProjection {
    fn default() -> Self {
        Self {
            replaying: true,
            projected_active: None,
            launched: HashSet::new(),
        }
    }
}

#[derive(Default)]
struct SseFrames {
    pending: Vec<u8>,
}

impl SseFrames {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<EngineEvent>, ShellBrokerError> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() > MAX_SSE_EVENT_BYTES {
            return Err(ShellBrokerError::Protocol(
                "engine event exceeded the broker SSE limit".to_owned(),
            ));
        }
        let mut events = Vec::new();
        while let Some(end) = find_frame_end(&self.pending) {
            let frame = self.pending.drain(..end).collect::<Vec<_>>();
            self.pending.drain(..frame_delimiter_len(&self.pending));
            let text = std::str::from_utf8(&frame)
                .map_err(|_| ShellBrokerError::Protocol("engine event was not UTF-8".to_owned()))?;
            let data = text
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .collect::<Vec<_>>()
                .join("\n");
            if !data.is_empty() {
                events.push(serde_json::from_str(&data).map_err(|_| {
                    ShellBrokerError::Protocol("engine event JSON was invalid".to_owned())
                })?);
            }
        }
        Ok(events)
    }
}

fn find_frame_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .or_else(|| bytes.windows(4).position(|window| window == b"\r\n\r\n"))
}

fn frame_delimiter_len(bytes: &[u8]) -> usize {
    if bytes.starts_with(b"\r\n\r\n") {
        4
    } else {
        2.min(bytes.len())
    }
}

struct BrokerOutputRedactor;

impl OutputRedactor for BrokerOutputRedactor {
    fn redact(&self, value: &str) -> String {
        value.to_owned()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{
        os::unix::fs::PermissionsExt as _,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use rw_core::{ClientId, CommandAckMeta, EventMeta};
    use tokio::sync::{Notify, mpsc, watch};

    use super::*;
    use crate::server::{ServerEngine, ServerRuntime, ServerState, serve};

    fn shell_event(sequence: u64, active: bool) -> EngineEvent {
        EngineEvent::UserShellStateChanged {
            meta: EventMeta {
                protocol_version: PROTOCOL_VERSION,
                session_id: SessionId("session".to_owned()),
                sequence_id: SequenceId(sequence),
                emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                caused_by: None,
            },
            shell_id: ShellId("shell-1".to_owned()),
            command: Some("python -q".to_owned()),
            active,
            status: (!active).then_some(0),
            captured_output: None,
        }
    }

    fn replay_complete() -> EngineEvent {
        EngineEvent::SessionReplayCompleted {
            meta: CommandAckMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId("broker".to_owned()),
                request_id: RequestId("replay".to_owned()),
                emitted_at: "2026-01-01T00:00:00Z".to_owned(),
            },
            session_id: SessionId("session".to_owned()),
            through_sequence: Some(SequenceId(2)),
        }
    }

    struct CompletionEngine {
        first_generation: bool,
        subscribed: Arc<Notify>,
        completions: AtomicUsize,
    }

    #[async_trait]
    impl ServerEngine for CompletionEngine {
        async fn dispatch(
            &self,
            _bound_client: ClientId,
            _command: ClientCommand,
        ) -> Result<CommandOutcome, String> {
            Err("interactive dispatch is unused by this fixture".to_owned())
        }

        async fn subscribe(
            &self,
            _bound_client: ClientId,
            _session_id: Option<SessionId>,
            _last_seen: Option<SequenceId>,
        ) -> Result<mpsc::Receiver<Result<EngineEvent, String>>, String> {
            let (send, receive) = mpsc::channel(4);
            self.subscribed.notify_one();
            if !self.first_generation {
                send.send(Ok(shell_event(1, true)))
                    .await
                    .map_err(|_| "fixture replay receiver closed".to_owned())?;
                send.send(Ok(shell_event(2, false)))
                    .await
                    .map_err(|_| "fixture replay receiver closed".to_owned())?;
                send.send(Ok(replay_complete()))
                    .await
                    .map_err(|_| "fixture replay receiver closed".to_owned())?;
            }
            Ok(receive)
        }

        async fn complete_shell(
            &self,
            _session_id: SessionId,
            _shell_id: ShellId,
            _status: i32,
            _captured_output: Option<String>,
        ) -> Result<(), String> {
            self.completions.fetch_add(1, Ordering::Relaxed);
            Err(if self.first_generation {
                "engine crashed after receiving completion".to_owned()
            } else {
                "matching completion was already durable".to_owned()
            })
        }
    }

    #[test]
    fn historical_closed_shell_is_not_relaunched_after_replay() {
        let mut projection = ReplayProjection::default();
        assert!(projection.observe(&shell_event(1, true)).is_none());
        assert!(projection.observe(&shell_event(2, false)).is_none());
        assert!(projection.observe(&replay_complete()).is_none());
    }

    #[test]
    fn active_replay_and_live_start_launch_exactly_once() {
        let mut replayed = ReplayProjection::default();
        assert!(replayed.observe(&shell_event(1, true)).is_none());
        assert_eq!(
            replayed.observe(&replay_complete()),
            Some(ShellLaunch {
                shell_id: ShellId("shell-1".to_owned()),
                command: "python -q".to_owned(),
            })
        );
        assert!(replayed.observe(&shell_event(1, true)).is_none());

        let mut live = ReplayProjection::default();
        assert!(live.observe(&replay_complete()).is_none());
        assert!(live.observe(&shell_event(3, true)).is_some());
        assert!(live.observe(&shell_event(3, true)).is_none());
    }

    #[test]
    fn sse_parser_accepts_split_frames_and_rejects_oversize() {
        let encoded = serde_json::to_string(&shell_event(1, true)).expect("event JSON");
        let frame = format!("event: engine\ndata: {encoded}\n\n");
        let split = frame.len() / 2;
        let mut parser = SseFrames::default();
        assert!(
            parser
                .push(&frame.as_bytes()[..split])
                .expect("first")
                .is_empty()
        );
        assert_eq!(
            parser.push(&frame.as_bytes()[split..]).expect("second"),
            vec![shell_event(1, true)]
        );
        assert!(parser.push(&vec![b'x'; MAX_SSE_EVENT_BYTES + 1]).is_err());
    }

    #[test]
    fn completion_replay_makes_matching_shell_end_idempotent_and_keeps_output_usable() {
        let shell_id = ShellId("shell-1".to_owned());
        let mut state = ShellReplayState::Missing;
        assert_eq!(
            observe_shell_replay(&mut state, &shell_event(1, true), &shell_id),
            None
        );
        assert_eq!(state, ShellReplayState::Active);
        assert_eq!(
            observe_shell_replay(&mut state, &shell_event(2, false), &shell_id),
            None
        );
        assert_eq!(state, ShellReplayState::Completed);
        assert_eq!(
            observe_shell_replay(&mut state, &replay_complete(), &shell_id),
            Some(ShellReplayState::Completed)
        );
        assert_eq!(
            BrokerOutputRedactor.redact("visible output token"),
            "visible output token"
        );
    }

    #[tokio::test]
    async fn completion_remints_after_runtime_rotation_and_accepts_recovered_durable_end() {
        let root = tempfile::tempdir().expect("runtime root");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private runtime root");
        let (runtime, listener) = ServerRuntime::create(root.path()).expect("first runtime");
        let paths = runtime.paths.clone();
        let first_subscribed = Arc::new(Notify::new());
        let first_engine = Arc::new(CompletionEngine {
            first_generation: true,
            subscribed: Arc::clone(&first_subscribed),
            completions: AtomicUsize::new(0),
        });
        let (first_shutdown, first_shutdown_rx) = watch::channel(false);
        let first_server = tokio::spawn(serve(
            listener,
            ServerState::new(first_engine.clone(), &runtime),
            first_shutdown_rx,
        ));
        let bootstrap = read_bootstrap_token(&paths.token).expect("first bootstrap");
        let session_id = SessionId("session".to_owned());
        let client = BrokerClient::connect(&paths.socket, &bootstrap, &session_id)
            .await
            .expect("first broker client");
        let config = ShellBrokerConfig {
            socket: paths.socket.clone(),
            token_file: paths.token.clone(),
            session_id,
            target: ShellTarget::Local,
        };
        let gate = Arc::new(BrokerCompletionGate::new(&config, client));
        let completion_gate = Arc::clone(&gate);
        let completion = tokio::spawn(async move {
            completion_gate
                .shell_ended(
                    ShellId("shell-1".to_owned()),
                    23,
                    Some("usable output".to_owned()),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), first_subscribed.notified())
            .await
            .expect("first completion replay attempt");
        first_shutdown.send(true).expect("stop first runtime");
        first_server
            .await
            .expect("first server join")
            .expect("first server stop");

        let rotation_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let (second_runtime, second_listener) = loop {
            match ServerRuntime::create_at(paths.clone()) {
                Ok(runtime) => break runtime,
                Err(error) if tokio::time::Instant::now() < rotation_deadline => {
                    tracing::debug!(reason = %error, "waiting for stopped runtime socket to quiesce");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("rotated runtime: {error}"),
            }
        };
        let second_engine = Arc::new(CompletionEngine {
            first_generation: false,
            subscribed: Arc::new(Notify::new()),
            completions: AtomicUsize::new(0),
        });
        let (second_shutdown, second_shutdown_rx) = watch::channel(false);
        let second_server = tokio::spawn(serve(
            second_listener,
            ServerState::new(second_engine.clone(), &second_runtime),
            second_shutdown_rx,
        ));
        tokio::time::timeout(Duration::from_secs(3), completion)
            .await
            .expect("completion retry timeout")
            .expect("completion task")
            .expect("completion became durable");
        assert!(first_engine.completions.load(Ordering::Relaxed) >= 1);
        assert!(second_engine.completions.load(Ordering::Relaxed) >= 1);
        second_shutdown.send(true).expect("stop second runtime");
        second_server
            .await
            .expect("second server join")
            .expect("second server stop");
    }
}
