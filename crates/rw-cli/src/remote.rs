//! SSH-forwarded remote engine command construction.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::{
    Method, Request, StatusCode,
    body::{Bytes, Incoming},
    client::conn::http1 as client_http1,
    header::{AUTHORIZATION, CONNECTION, CONTENT_TYPE, HOST, HeaderValue},
};
use hyper_util::rt::TokioIo;
use rw_core::{ClientCommand, CommandMeta, CommandOutcome, PROTOCOL_VERSION, RequestId, SessionId};
use rw_types::PermissionModeDescriptor;
use tokio::net::UnixStream;

use crate::server::{CLIENT_HEADER, ClientCredentials};

const CONTROL_BODY_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

#[derive(Clone, Debug)]
pub struct RemoteConfig {
    pub ssh_executable: PathBuf,
    pub host: String,
    pub remote_rw_executable: PathBuf,
    pub remote_socket: PathBuf,
    pub local_socket: PathBuf,
    pub session_id: String,
    pub remote_workspace: PathBuf,
    pub additional_workspaces: Vec<PathBuf>,
    pub dangerously_trust: bool,
    pub model: Option<String>,
    pub permission_mode: Option<PermissionModeDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteError {
    Host,
    SocketPath,
    RemoteExecutable,
    Session,
    Workspace,
    Model,
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Host => "remote SSH host is invalid",
            Self::SocketPath => "remote forwarding requires absolute Unix socket paths",
            Self::RemoteExecutable => "remote rw executable must be an absolute path",
            Self::Session => "remote session id is invalid",
            Self::Workspace => "remote workspace must be an absolute safe path",
            Self::Model => "remote model alias is invalid",
        })
    }
}

impl std::error::Error for RemoteError {}

impl RemoteConfig {
    pub fn validate(&self) -> Result<(), RemoteError> {
        if self.host.is_empty()
            || self.host.starts_with('-')
            || self
                .host
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(RemoteError::Host);
        }
        if !is_absolute_socket(&self.remote_socket) || !is_absolute_socket(&self.local_socket) {
            return Err(RemoteError::SocketPath);
        }
        if !is_safe_absolute_path(&self.remote_rw_executable) {
            return Err(RemoteError::RemoteExecutable);
        }
        if SessionId::validate(&self.session_id).is_err() {
            return Err(RemoteError::Session);
        }
        if !is_safe_absolute_path(&self.remote_workspace) {
            return Err(RemoteError::Workspace);
        }
        if self
            .additional_workspaces
            .iter()
            .any(|path| !is_safe_absolute_path(path))
        {
            return Err(RemoteError::Workspace);
        }
        if self.model.as_ref().is_some_and(|model| {
            model.is_empty()
                || model
                    .chars()
                    .any(|value| value == '\0' || value.is_control())
        }) {
            return Err(RemoteError::Model);
        }
        Ok(())
    }

    /// Starts or attaches the remote engine as a detached remote process. The
    /// local lifecycle supervisor later shuts down only an engine it created;
    /// explicit detach and pre-existing engines remain remote-owned. An omitted
    /// permission mode lets the remote host load its own user policy.
    pub fn engine_start_command(&self) -> Result<SshCommand, RemoteError> {
        self.engine_command(false)
    }

    /// Starts a replacement engine which may wait for the crashed engine's
    /// watchdog to release workspace execution ownership.
    pub fn engine_recovery_command(&self) -> Result<SshCommand, RemoteError> {
        self.engine_command(true)
    }

    fn engine_command(&self, wait_for_execution_lease: bool) -> Result<SshCommand, RemoteError> {
        self.validate()?;
        let mut remote_argv = vec![
            self.remote_rw_executable.to_string_lossy().into_owned(),
            "serve".to_owned(),
            "--detach".to_owned(),
        ];
        if wait_for_execution_lease {
            remote_argv.push("--wait-for-execution-lease".to_owned());
        }
        if let Some(mode) = self.permission_mode {
            remote_argv.extend(["--permission-mode".to_owned(), mode.as_str().to_owned()]);
        }
        remote_argv.extend([
            "--socket".to_owned(),
            self.remote_socket.to_string_lossy().into_owned(),
            "--session".to_owned(),
            self.session_id.clone(),
            "--workspace".to_owned(),
            self.remote_workspace.to_string_lossy().into_owned(),
        ]);
        if let Some(model) = &self.model {
            remote_argv.extend(["--model".to_owned(), model.clone()]);
        }
        for root in &self.additional_workspaces {
            remote_argv.extend(["--add-dir".to_owned(), root.to_string_lossy().into_owned()]);
        }
        if self.dangerously_trust {
            remote_argv.push("--dangerously-trust".to_owned());
        }
        let remote_command = remote_argv
            .iter()
            .map(|argument| shell_quote(argument))
            .collect::<Vec<_>>()
            .join(" ");
        let args = vec![
            OsString::from("-T"),
            OsString::from("-o"),
            OsString::from("BatchMode=yes"),
            OsString::from("--"),
            OsString::from(&self.host),
            OsString::from(remote_command),
        ];
        Ok(SshCommand {
            program: self.ssh_executable.clone(),
            args,
        })
    }

    /// Creates a `StreamLocal` Unix-socket tunnel. There is deliberately no TCP
    /// bind address or non-loopback daemon surface.
    pub fn forward_command(&self) -> Result<SshCommand, RemoteError> {
        self.validate()?;
        let forwarding = format!(
            "{}:{}",
            self.local_socket.display(),
            self.remote_socket.display()
        );
        Ok(SshCommand {
            program: self.ssh_executable.clone(),
            args: vec![
                OsString::from("-N"),
                OsString::from("-T"),
                OsString::from("-o"),
                OsString::from("ExitOnForwardFailure=yes"),
                OsString::from("-o"),
                OsString::from("StreamLocalBindUnlink=yes"),
                OsString::from("-L"),
                OsString::from(forwarding),
                OsString::from("--"),
                OsString::from(&self.host),
            ],
        })
    }
}

fn is_absolute_socket(path: &Path) -> bool {
    is_safe_absolute_path(path) && path.to_str().is_some_and(|value| !value.contains(':'))
}

fn is_safe_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .to_str()
            .is_some_and(|value| !value.is_empty() && !value.chars().any(char::is_control))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Testable seam proving loopback remote mode uses the same client-side socket
/// consumer as a local engine; only socket establishment differs.
pub trait ForwardedSocketConsumer {
    type Output;
    type Error;

    fn connect(&self, socket: &Path) -> Result<Self::Output, Self::Error>;
}

pub fn connect_forwarded<C: ForwardedSocketConsumer>(
    config: &RemoteConfig,
    consumer: &C,
) -> Result<C::Output, RemoteConnectError<C::Error>> {
    config.validate().map_err(RemoteConnectError::Config)?;
    consumer
        .connect(&config.local_socket)
        .map_err(RemoteConnectError::Consumer)
}

#[derive(Debug)]
pub enum RemoteConnectError<E> {
    Config(RemoteError),
    Consumer(E),
}

/// Result of one watchdog recovery pass. The distinction is useful for
/// diagnostics and, more importantly, makes it explicit that a dead SSH
/// tunnel is repaired before asking the remote host to attach or start an
/// engine. That prevents needless bootstrap-token rotation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryOutcome {
    Healthy,
    TunnelRecreated,
    EngineAttachedOrStarted,
    EngineAndTunnelRecovered,
}

/// Token and lifecycle ownership returned by one idempotent remote attach.
/// `started` is true only when this invocation created the engine; attaching
/// to an already-live, user-owned detached engine must never claim ownership.
#[derive(Clone, Eq, PartialEq)]
pub struct RemoteAttachment {
    pub bootstrap_token: String,
    pub started: bool,
}

impl std::fmt::Debug for RemoteAttachment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteAttachment")
            .field("bootstrap_token", &"[REDACTED]")
            .field("started", &self.started)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteInitialization {
    pub outcome: RecoveryOutcome,
    pub attachment: RemoteAttachment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteInitializationError {
    pub message: String,
    /// Present once attach-or-start returned a validated descriptor. Keeping
    /// the token out of the message lets the caller transactionally unwind.
    pub attachment: Option<RemoteAttachment>,
}

impl std::fmt::Display for RemoteInitializationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RemoteInitializationError {}

/// Testable process/transport seam for remote recovery. Implementations must
/// make `attach_or_start` idempotent: a live engine is attached to and its
/// existing token returned; only a dead engine may be replaced.
#[async_trait]
pub trait RemoteRecoveryRuntime: Send {
    async fn authenticated_health(&mut self) -> Result<bool, String>;
    async fn tunnel_alive(&mut self) -> Result<bool, String>;
    async fn restart_tunnel(&mut self) -> Result<(), String>;
    async fn attach_or_start(
        &mut self,
        wait_for_execution_lease: bool,
    ) -> Result<RemoteAttachment, String>;
    async fn install_bootstrap_token(&mut self, token: &str) -> Result<(), String>;
}

/// Establishes a remote engine connection for the first time. The remote
/// attach-or-start operation happens before the tunnel is exposed locally, and
/// the local token is installed atomically by the runtime implementation.
pub async fn initialize_remote<R: RemoteRecoveryRuntime>(
    runtime: &mut R,
) -> Result<RemoteInitialization, RemoteInitializationError> {
    let attachment =
        runtime
            .attach_or_start(false)
            .await
            .map_err(|message| RemoteInitializationError {
                message,
                attachment: None,
            })?;
    let after_attach = |message| RemoteInitializationError {
        message,
        attachment: Some(attachment.clone()),
    };
    runtime
        .install_bootstrap_token(&attachment.bootstrap_token)
        .await
        .map_err(after_attach)?;
    runtime.restart_tunnel().await.map_err(after_attach)?;
    if runtime.authenticated_health().await.map_err(after_attach)? {
        Ok(RemoteInitialization {
            outcome: RecoveryOutcome::EngineAndTunnelRecovered,
            attachment,
        })
    } else {
        Err(after_attach(
            "forwarded remote engine failed authenticated health after startup".to_owned(),
        ))
    }
}

/// Repairs a forwarded remote connection without replacing live state.
///
/// A failed health probe is first treated as a tunnel failure. Only after a
/// live/recreated tunnel still cannot authenticate do we issue the idempotent
/// remote attach-or-start command and install the returned token. A final
/// tunnel recreation covers half-open SSH processes that still report alive.
pub async fn recover_remote<R: RemoteRecoveryRuntime>(
    runtime: &mut R,
) -> Result<RecoveryOutcome, String> {
    if runtime.authenticated_health().await? {
        return Ok(RecoveryOutcome::Healthy);
    }

    if !runtime.tunnel_alive().await? {
        runtime.restart_tunnel().await?;
        if runtime.authenticated_health().await? {
            return Ok(RecoveryOutcome::TunnelRecreated);
        }
    }

    let attachment = runtime.attach_or_start(true).await?;
    runtime
        .install_bootstrap_token(&attachment.bootstrap_token)
        .await?;
    if runtime.authenticated_health().await? {
        return Ok(RecoveryOutcome::EngineAttachedOrStarted);
    }

    // A process can remain alive while its forwarding channel is half-open.
    // Recreate only the local tunnel after the idempotent engine operation.
    runtime.restart_tunnel().await?;
    if runtime.authenticated_health().await? {
        Ok(RecoveryOutcome::EngineAndTunnelRecovered)
    } else {
        Err(
            "remote engine remained unhealthy after attach-or-start and tunnel recreation"
                .to_owned(),
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct WatchdogPolicy {
    pub interval: Duration,
    pub maximum_consecutive_failures: u32,
}

impl Default for WatchdogPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(250),
            maximum_consecutive_failures: 8,
        }
    }
}

/// Runs until explicitly shut down or the bounded recovery budget is
/// exhausted. Transient SSH failures are retried; a healthy pass resets the
/// consecutive-failure count.
pub async fn run_watchdog<R: RemoteRecoveryRuntime>(
    mut runtime: R,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    policy: WatchdogPolicy,
) -> Result<(), String> {
    if policy.maximum_consecutive_failures == 0 {
        return Err("remote watchdog recovery budget must be positive".to_owned());
    }
    let mut failures = 0_u32;
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            () = tokio::time::sleep(policy.interval) => {
                match recover_remote(&mut runtime).await {
                    Ok(_) => failures = 0,
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        if failures >= policy.maximum_consecutive_failures {
                            return Err(format!(
                                "remote watchdog recovery budget exhausted after {failures} failures: {error}"
                            ));
                        }
                    }
                }
            }
        }
    }
}

/// Production watchdog control. Pausing is acknowledged only after any
/// in-flight recovery pass has completed, so the caller can use the still-live
/// tunnel for one final authenticated shutdown without racing a restart.
pub enum WatchdogCommand {
    Pause(tokio::sync::oneshot::Sender<()>),
    Shutdown,
}

pub async fn run_controlled_watchdog<R: RemoteRecoveryRuntime>(
    mut runtime: R,
    mut control: tokio::sync::mpsc::Receiver<WatchdogCommand>,
    policy: WatchdogPolicy,
) -> Result<(), String> {
    if policy.maximum_consecutive_failures == 0 {
        return Err("remote watchdog recovery budget must be positive".to_owned());
    }
    let mut failures = 0_u32;
    loop {
        tokio::select! {
            biased;
            command = control.recv() => match command {
                Some(WatchdogCommand::Pause(acknowledged)) => {
                    let _ = acknowledged.send(());
                    loop {
                        match control.recv().await {
                            Some(WatchdogCommand::Shutdown) | None => return Ok(()),
                            Some(WatchdogCommand::Pause(acknowledged)) => {
                                let _ = acknowledged.send(());
                            }
                        }
                    }
                }
                Some(WatchdogCommand::Shutdown) | None => return Ok(()),
            },
            () = tokio::time::sleep(policy.interval) => {
                match recover_remote(&mut runtime).await {
                    Ok(_) => failures = 0,
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        if failures >= policy.maximum_consecutive_failures {
                            return Err(format!(
                                "remote watchdog recovery budget exhausted after {failures} failures: {error}"
                            ));
                        }
                    }
                }
            }
        }
    }
}

/// Sends the same bootstrap-authenticated health request used by production
/// clients over the forwarded Unix socket. A non-200 response is a valid
/// negative probe; connection and protocol failures are returned to the
/// watchdog for bounded retry.
pub async fn probe_authenticated_health(
    socket: &Path,
    bootstrap_token: &str,
    timeout: Duration,
) -> Result<bool, String> {
    let authorization = HeaderValue::from_str(&format!("Bearer {bootstrap_token}"))
        .map_err(|_| "remote bootstrap token is not a valid HTTP credential".to_owned())?;
    let operation = async {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|error| format!("forwarded socket connection failed: {error}"))?;
        let (mut sender, connection) = client_http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| format!("forwarded HTTP handshake failed: {error}"))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = Request::builder()
            .method(Method::GET)
            .uri("/v1/health")
            .header(HOST, "localhost")
            .header(AUTHORIZATION, authorization)
            .body(Full::new(Bytes::new()))
            .map_err(|_| "could not build authenticated remote health request".to_owned())?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|error| format!("forwarded health request failed: {error}"))?;
        let status = response.status();
        let _ = response.into_body().collect().await;
        Ok(status == StatusCode::OK)
    };
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| "forwarded authenticated health request timed out".to_owned())?
}

/// Authenticates through the forwarded socket and asks the remote host to
/// terminate. This is used only for the default attached lifecycle; explicit
/// `--detach` deliberately skips it.
pub async fn shutdown_authenticated_host(
    socket: &Path,
    bootstrap_token: &str,
    timeout: Duration,
) -> Result<(), String> {
    if bootstrap_token.len() != 64 || !bootstrap_token.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("remote shutdown bootstrap token is invalid".to_owned());
    }
    tokio::time::timeout(
        timeout,
        shutdown_authenticated_host_inner(socket, bootstrap_token),
    )
    .await
    .map_err(|_| "remote host shutdown timed out".to_owned())?
}

async fn shutdown_authenticated_host_inner(
    socket: &Path,
    bootstrap_token: &str,
) -> Result<(), String> {
    let connect = Request::builder()
        .method(Method::POST)
        .uri("/v1/connect")
        .header(HOST, "localhost")
        .header(AUTHORIZATION, format!("Bearer {bootstrap_token}"))
        .body(Full::new(Bytes::new()))
        .map_err(|_| "could not build remote shutdown handshake".to_owned())?;
    let connected = unix_request(socket, connect).await?;
    if connected.status() != StatusCode::CREATED {
        return Err("remote engine rejected shutdown authentication".to_owned());
    }
    let credentials: ClientCredentials = collect_control_json(connected.into_body()).await?;
    let command = ClientCommand::ShutdownHost {
        meta: CommandMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: credentials.client_id.clone(),
            request_id: RequestId("remote-supervisor-shutdown".to_owned()),
        },
    };
    let body = serde_json::to_vec(&command)
        .map_err(|_| "could not serialize remote shutdown command".to_owned())?;
    let shutdown = Request::builder()
        .method(Method::POST)
        .uri("/v1/command")
        .header(HOST, "localhost")
        .header(AUTHORIZATION, format!("Bearer {}", credentials.token))
        .header(CLIENT_HEADER, &credentials.client_id.0)
        .header(CONNECTION, "close")
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .map_err(|_| "could not build remote shutdown command".to_owned())?;
    let response = unix_request(socket, shutdown).await?;
    if response.status() != StatusCode::ACCEPTED {
        return Err("remote engine rejected host shutdown".to_owned());
    }
    match collect_control_json::<rw_core::CommandReply>(response.into_body())
        .await?
        .outcome()
    {
        CommandOutcome::Accepted => Ok(()),
        CommandOutcome::Rejected { error } => Err(format!(
            "remote engine rejected host shutdown: {}",
            error.code
        )),
    }
}

async fn unix_request(
    socket: &Path,
    request: Request<Full<Bytes>>,
) -> Result<hyper::Response<Incoming>, String> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|error| format!("forwarded socket connection failed: {error}"))?;
    let (mut sender, connection) = client_http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|error| format!("forwarded HTTP handshake failed: {error}"))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    sender
        .send_request(request)
        .await
        .map_err(|error| format!("forwarded HTTP request failed: {error}"))
}

async fn collect_control_json<T: serde::de::DeserializeOwned>(body: Incoming) -> Result<T, String> {
    let bytes = Limited::new(body, CONTROL_BODY_LIMIT)
        .collect()
        .await
        .map_err(|_| "remote control response exceeded its limit".to_owned())?
        .to_bytes();
    serde_json::from_slice(&bytes).map_err(|_| "remote control response was invalid".to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use std::{collections::VecDeque, sync::Arc};

    fn config() -> RemoteConfig {
        RemoteConfig {
            ssh_executable: PathBuf::from("/usr/bin/ssh"),
            host: "localhost".to_owned(),
            remote_rw_executable: PathBuf::from("/usr/local/bin/rw"),
            remote_socket: PathBuf::from("/tmp/rottweiler/engine.sock"),
            local_socket: PathBuf::from("/tmp/rottweiler-forward.sock"),
            session_id: "session-1".to_owned(),
            remote_workspace: PathBuf::from("/work/project"),
            additional_workspaces: Vec::new(),
            dangerously_trust: false,
            model: None,
            permission_mode: Some(PermissionModeDescriptor::Strict),
        }
    }

    #[test]
    fn exact_ssh_argv_is_detached_strict_streamlocal_and_secret_free() {
        let config = config();
        let start = config.engine_start_command().expect("start command");
        assert_eq!(
            start.args,
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "--",
                "localhost",
                "'/usr/local/bin/rw' 'serve' '--detach' '--permission-mode' 'strict' '--socket' '/tmp/rottweiler/engine.sock' '--session' 'session-1' '--workspace' '/work/project'",
            ]
            .map(OsString::from)
        );
        let recovery = config.engine_recovery_command().expect("recovery command");
        assert!(recovery.args.last().is_some_and(|command| {
            command
                .to_string_lossy()
                .contains("'--wait-for-execution-lease'")
        }));
        let forward = config.forward_command().expect("forward command");
        assert_eq!(
            forward.args,
            [
                "-N",
                "-T",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "StreamLocalBindUnlink=yes",
                "-L",
                "/tmp/rottweiler-forward.sock:/tmp/rottweiler/engine.sock",
                "--",
                "localhost",
            ]
            .map(OsString::from)
        );
        assert!(!format!("{start:?}{forward:?}").contains("token"));
    }

    #[test]
    fn remote_start_carries_safe_added_roots_and_explicit_trust_only() {
        let mut candidate = config();
        candidate.additional_workspaces = vec![PathBuf::from("/work/second repo")];
        candidate.dangerously_trust = true;
        let command = candidate.engine_start_command().expect("remote command");
        let rendered = command.args.last().expect("remote argv").to_string_lossy();
        assert!(rendered.contains("--add-dir"));
        assert!(rendered.contains("'/work/second repo'"));
        assert!(rendered.contains("--dangerously-trust"));

        candidate.additional_workspaces = vec![PathBuf::from("relative")];
        assert_eq!(candidate.validate(), Err(RemoteError::Workspace));
    }

    #[test]
    fn remote_forwards_explicit_permission_modes_and_rejects_ssh_option_injection() {
        for mode in [
            PermissionModeDescriptor::AutoSafe,
            PermissionModeDescriptor::Yolo,
        ] {
            let mut candidate = config();
            candidate.permission_mode = Some(mode);
            let command = candidate.engine_start_command().expect("permission mode");
            let rendered = command.args.last().expect("remote argv").to_string_lossy();
            assert!(rendered.contains(&format!("'{}'", mode.as_str())));
        }
        let mut inherited = config();
        inherited.permission_mode = None;
        let command = inherited.engine_start_command().expect("inherited policy");
        assert!(
            !command
                .args
                .last()
                .expect("remote argv")
                .to_string_lossy()
                .contains("--permission-mode")
        );
        let mut candidate = config();
        candidate.host = "-oProxyCommand=bad".to_owned();
        assert_eq!(candidate.validate(), Err(RemoteError::Host));
        let mut candidate = config();
        candidate.remote_socket = PathBuf::from("/tmp/socket;touch-pwned");
        assert!(candidate.validate().is_ok());
        assert!(
            candidate
                .engine_start_command()
                .expect("quoted command")
                .args
                .last()
                .is_some_and(|command| command
                    .to_string_lossy()
                    .contains("'/tmp/socket;touch-pwned'"))
        );

        let mut spaced = config();
        spaced.remote_workspace = PathBuf::from("/work/project with spaces/it's-safe");
        assert!(spaced.validate().is_ok());
        assert!(
            spaced
                .engine_start_command()
                .expect("space-safe command")
                .args
                .last()
                .is_some_and(|command| command
                    .to_string_lossy()
                    .contains("'/work/project with spaces/it'\"'\"'s-safe'"))
        );
    }

    #[test]
    fn remote_rejects_dot_component_session_ids() {
        for session_id in [".", ".."] {
            let mut candidate = config();
            candidate.session_id = session_id.to_owned();
            assert_eq!(candidate.validate(), Err(RemoteError::Session));
        }
    }

    struct RecordingConsumer(Mutex<Vec<PathBuf>>);

    use std::sync::Mutex;

    impl ForwardedSocketConsumer for RecordingConsumer {
        type Output = &'static str;
        type Error = ();

        fn connect(&self, socket: &Path) -> Result<Self::Output, Self::Error> {
            self.0.lock().expect("paths").push(socket.to_path_buf());
            Ok("same-client-path")
        }
    }

    #[test]
    fn loopback_remote_uses_the_forwarded_local_socket_seam() {
        let consumer = RecordingConsumer(Mutex::new(Vec::new()));
        assert_eq!(
            connect_forwarded(&config(), &consumer).expect("connect"),
            "same-client-path"
        );
        assert_eq!(
            consumer.0.lock().expect("paths").as_slice(),
            [PathBuf::from("/tmp/rottweiler-forward.sock")]
        );
    }

    #[derive(Default)]
    struct MockRecoveryState {
        health: VecDeque<Result<bool, String>>,
        tunnel_alive: VecDeque<Result<bool, String>>,
        attachments: VecDeque<Result<RemoteAttachment, String>>,
        tunnel_restarts: VecDeque<Result<(), String>>,
        token_installs: VecDeque<Result<(), String>>,
        calls: Vec<&'static str>,
        installed: Vec<String>,
    }

    #[derive(Clone)]
    struct MockRecovery(Arc<tokio::sync::Mutex<MockRecoveryState>>);

    impl MockRecovery {
        fn new(state: MockRecoveryState) -> Self {
            Self(Arc::new(tokio::sync::Mutex::new(state)))
        }

        async fn calls(&self) -> Vec<&'static str> {
            self.0.lock().await.calls.clone()
        }

        async fn installed(&self) -> Vec<String> {
            self.0.lock().await.installed.clone()
        }
    }

    #[async_trait]
    impl RemoteRecoveryRuntime for MockRecovery {
        async fn authenticated_health(&mut self) -> Result<bool, String> {
            let mut state = self.0.lock().await;
            state.calls.push("health");
            state
                .health
                .pop_front()
                .unwrap_or_else(|| Err("missing mock health result".to_owned()))
        }

        async fn tunnel_alive(&mut self) -> Result<bool, String> {
            let mut state = self.0.lock().await;
            state.calls.push("tunnel_alive");
            state
                .tunnel_alive
                .pop_front()
                .unwrap_or_else(|| Err("missing mock tunnel result".to_owned()))
        }

        async fn restart_tunnel(&mut self) -> Result<(), String> {
            let mut state = self.0.lock().await;
            state.calls.push("restart_tunnel");
            state.tunnel_restarts.pop_front().unwrap_or(Ok(()))
        }

        async fn attach_or_start(
            &mut self,
            wait_for_execution_lease: bool,
        ) -> Result<RemoteAttachment, String> {
            let mut state = self.0.lock().await;
            state.calls.push(if wait_for_execution_lease {
                "attach_or_start_wait"
            } else {
                "attach_or_start"
            });
            state
                .attachments
                .pop_front()
                .unwrap_or_else(|| Err("missing mock attach result".to_owned()))
        }

        async fn install_bootstrap_token(&mut self, token: &str) -> Result<(), String> {
            let mut state = self.0.lock().await;
            state.calls.push("install_token");
            state.installed.push(token.to_owned());
            state.token_installs.pop_front().unwrap_or(Ok(()))
        }
    }

    #[tokio::test]
    async fn recovery_repairs_a_dead_tunnel_before_touching_remote_engine() {
        let mut runtime = MockRecovery::new(MockRecoveryState {
            health: VecDeque::from([Ok(false), Ok(true)]),
            tunnel_alive: VecDeque::from([Ok(false)]),
            ..MockRecoveryState::default()
        });

        assert_eq!(
            recover_remote(&mut runtime).await.expect("recovered"),
            RecoveryOutcome::TunnelRecreated
        );
        assert_eq!(
            runtime.calls().await,
            ["health", "tunnel_alive", "restart_tunnel", "health"]
        );
        assert!(runtime.installed().await.is_empty());
    }

    #[tokio::test]
    async fn recovery_attaches_then_rotates_token_without_replacing_live_tunnel() {
        let mut runtime = MockRecovery::new(MockRecoveryState {
            health: VecDeque::from([Ok(false), Ok(true)]),
            tunnel_alive: VecDeque::from([Ok(true)]),
            attachments: VecDeque::from([Ok(RemoteAttachment {
                bootstrap_token: "b".repeat(64),
                started: false,
            })]),
            ..MockRecoveryState::default()
        });

        assert_eq!(
            recover_remote(&mut runtime).await.expect("recovered"),
            RecoveryOutcome::EngineAttachedOrStarted
        );
        assert_eq!(
            runtime.calls().await,
            [
                "health",
                "tunnel_alive",
                "attach_or_start_wait",
                "install_token",
                "health"
            ]
        );
        assert_eq!(runtime.installed().await, ["b".repeat(64)]);
    }

    #[tokio::test]
    async fn recovery_replaces_a_half_open_tunnel_only_after_safe_attach() {
        let mut runtime = MockRecovery::new(MockRecoveryState {
            health: VecDeque::from([Ok(false), Ok(false), Ok(true)]),
            tunnel_alive: VecDeque::from([Ok(true)]),
            attachments: VecDeque::from([Ok(RemoteAttachment {
                bootstrap_token: "c".repeat(64),
                started: false,
            })]),
            ..MockRecoveryState::default()
        });

        assert_eq!(
            recover_remote(&mut runtime).await.expect("recovered"),
            RecoveryOutcome::EngineAndTunnelRecovered
        );
        assert_eq!(
            runtime.calls().await,
            [
                "health",
                "tunnel_alive",
                "attach_or_start_wait",
                "install_token",
                "health",
                "restart_tunnel",
                "health"
            ]
        );
    }

    #[tokio::test]
    async fn initialization_installs_token_before_exposing_forwarded_socket() {
        let mut runtime = MockRecovery::new(MockRecoveryState {
            health: VecDeque::from([Ok(true)]),
            attachments: VecDeque::from([Ok(RemoteAttachment {
                bootstrap_token: "d".repeat(64),
                started: true,
            })]),
            ..MockRecoveryState::default()
        });

        assert_eq!(
            initialize_remote(&mut runtime).await.expect("initialized"),
            RemoteInitialization {
                outcome: RecoveryOutcome::EngineAndTunnelRecovered,
                attachment: RemoteAttachment {
                    bootstrap_token: "d".repeat(64),
                    started: true,
                },
            }
        );
        assert_eq!(
            runtime.calls().await,
            [
                "attach_or_start",
                "install_token",
                "restart_tunnel",
                "health"
            ]
        );
    }

    #[tokio::test]
    async fn initialization_errors_retain_owned_attachment_for_transactional_cleanup() {
        for (token_installs, tunnel_restarts, health, expected) in [
            (
                VecDeque::from([Err("token install failed".to_owned())]),
                VecDeque::new(),
                VecDeque::new(),
                "token install failed",
            ),
            (
                VecDeque::new(),
                VecDeque::from([Err("tunnel failed".to_owned())]),
                VecDeque::new(),
                "tunnel failed",
            ),
            (
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::from([Ok(false)]),
                "failed authenticated health",
            ),
            (
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::from([Err("health transport failed".to_owned())]),
                "health transport failed",
            ),
        ] {
            let attachment = RemoteAttachment {
                bootstrap_token: "e".repeat(64),
                started: true,
            };
            let mut runtime = MockRecovery::new(MockRecoveryState {
                attachments: VecDeque::from([Ok(attachment.clone())]),
                token_installs,
                tunnel_restarts,
                health,
                ..MockRecoveryState::default()
            });
            let error = initialize_remote(&mut runtime)
                .await
                .expect_err("initialization failpoint");
            assert!(error.message.contains(expected));
            assert_eq!(error.attachment, Some(attachment));
        }
    }

    #[tokio::test]
    async fn initialization_attach_failure_does_not_claim_remote_ownership() {
        let mut runtime = MockRecovery::new(MockRecoveryState {
            attachments: VecDeque::from([Err("ssh failed".to_owned())]),
            ..MockRecoveryState::default()
        });
        let error = initialize_remote(&mut runtime)
            .await
            .expect_err("attach failure");
        assert_eq!(error.attachment, None);
        assert_eq!(error.message, "ssh failed");
    }

    #[test]
    fn remote_attachment_debug_never_exposes_bootstrap_token() {
        let attachment = RemoteAttachment {
            bootstrap_token: "secret-canary".to_owned(),
            started: true,
        };
        let rendered = format!("{attachment:?}");
        assert!(!rendered.contains("secret-canary"));
        assert!(rendered.contains("REDACTED"));
    }

    #[tokio::test]
    async fn watchdog_bounds_consecutive_process_or_transport_failures() {
        let runtime = MockRecovery::new(MockRecoveryState {
            health: VecDeque::from([Err("offline one".to_owned()), Err("offline two".to_owned())]),
            ..MockRecoveryState::default()
        });
        let (_shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
        let error = run_watchdog(
            runtime.clone(),
            shutdown,
            WatchdogPolicy {
                interval: Duration::from_millis(1),
                maximum_consecutive_failures: 2,
            },
        )
        .await
        .expect_err("watchdog budget");
        assert!(error.contains("recovery budget exhausted after 2 failures"));
        assert_eq!(runtime.calls().await, ["health", "health"]);
    }

    #[tokio::test]
    async fn controlled_watchdog_acknowledges_a_quiescent_pause_before_shutdown() {
        let runtime = MockRecovery::new(MockRecoveryState::default());
        let (control, commands) = tokio::sync::mpsc::channel(2);
        let watchdog = tokio::spawn(run_controlled_watchdog(
            runtime.clone(),
            commands,
            WatchdogPolicy {
                interval: Duration::from_millis(50),
                maximum_consecutive_failures: 2,
            },
        ));
        let (acknowledged, paused) = tokio::sync::oneshot::channel();
        control
            .send(WatchdogCommand::Pause(acknowledged))
            .await
            .expect("pause command");
        paused.await.expect("pause acknowledgement");
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(runtime.calls().await.is_empty());
        control
            .send(WatchdogCommand::Shutdown)
            .await
            .expect("shutdown command");
        watchdog.await.expect("watchdog join").expect("watchdog");
    }

    #[tokio::test]
    async fn authenticated_health_uses_forwarded_unix_socket_and_bearer_token() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let directory = tempfile::tempdir().expect("tempdir");
        let socket = directory.path().join("forward.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind loopback UDS");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept health probe");
            let mut request = vec![0_u8; 4096];
            let read = stream.read(&mut request).await.expect("read request");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("GET /v1/health HTTP/1.1\r\n"));
            assert!(request.contains("authorization: Bearer probe-secret\r\n"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 14\r\n\r\n{\"ready\":true}",
                )
                .await
                .expect("write response");
        });

        assert!(
            probe_authenticated_health(&socket, "probe-secret", Duration::from_secs(1))
                .await
                .expect("health probe")
        );
        server.await.expect("loopback server");
    }
}
