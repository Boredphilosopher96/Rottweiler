//! SSH-forwarded remote engine command construction.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use http_body_util::{BodyExt as _, Empty};
use hyper::{
    Method, Request, StatusCode,
    body::Bytes,
    client::conn::http1 as client_http1,
    header::{AUTHORIZATION, HOST, HeaderValue},
};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemotePermissionMode {
    Strict,
    AutoSafe,
    Yolo,
}

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
    pub permission_mode: RemotePermissionMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteError {
    StrictPermissionRequired,
    InvalidHost,
    InvalidSocketPath,
    InvalidRemoteExecutable,
    InvalidSession,
    InvalidWorkspace,
    InvalidModel,
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::StrictPermissionRequired => "remote sessions require strict permission mode",
            Self::InvalidHost => "remote SSH host is invalid",
            Self::InvalidSocketPath => "remote forwarding requires absolute Unix socket paths",
            Self::InvalidRemoteExecutable => "remote rw executable must be an absolute path",
            Self::InvalidSession => "remote session id is invalid",
            Self::InvalidWorkspace => "remote workspace must be an absolute safe path",
            Self::InvalidModel => "remote model alias is invalid",
        })
    }
}

impl std::error::Error for RemoteError {}

impl RemoteConfig {
    pub fn validate(&self) -> Result<(), RemoteError> {
        if self.permission_mode != RemotePermissionMode::Strict {
            return Err(RemoteError::StrictPermissionRequired);
        }
        if self.host.is_empty()
            || self.host.starts_with('-')
            || self
                .host
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(RemoteError::InvalidHost);
        }
        if !is_absolute_socket(&self.remote_socket) || !is_absolute_socket(&self.local_socket) {
            return Err(RemoteError::InvalidSocketPath);
        }
        if !is_safe_absolute_path(&self.remote_rw_executable) {
            return Err(RemoteError::InvalidRemoteExecutable);
        }
        if self.session_id.is_empty()
            || self.session_id.len() > 128
            || !self
                .session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(RemoteError::InvalidSession);
        }
        if !is_safe_absolute_path(&self.remote_workspace) {
            return Err(RemoteError::InvalidWorkspace);
        }
        if self
            .additional_workspaces
            .iter()
            .any(|path| !is_safe_absolute_path(path))
        {
            return Err(RemoteError::InvalidWorkspace);
        }
        if self.model.as_ref().is_some_and(|model| {
            model.is_empty()
                || model
                    .chars()
                    .any(|value| value == '\0' || value.is_control())
        }) {
            return Err(RemoteError::InvalidModel);
        }
        Ok(())
    }

    /// Starts or attaches the remote engine. Detach and strict mode are
    /// unconditional, so a local UI exit never terminates the remote engine.
    pub fn engine_start_command(&self) -> Result<SshCommand, RemoteError> {
        self.validate()?;
        let mut remote_argv = vec![
            self.remote_rw_executable.to_string_lossy().into_owned(),
            "serve".to_owned(),
            "--detach".to_owned(),
            "--permission-mode".to_owned(),
            "strict".to_owned(),
            "--socket".to_owned(),
            self.remote_socket.to_string_lossy().into_owned(),
            "--session".to_owned(),
            self.session_id.clone(),
            "--workspace".to_owned(),
            self.remote_workspace.to_string_lossy().into_owned(),
        ];
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

/// Testable process/transport seam for remote recovery. Implementations must
/// make `attach_or_start` idempotent: a live engine is attached to and its
/// existing token returned; only a dead engine may be replaced.
#[async_trait]
pub trait RemoteRecoveryRuntime: Send {
    async fn authenticated_health(&mut self) -> Result<bool, String>;
    async fn tunnel_alive(&mut self) -> Result<bool, String>;
    async fn restart_tunnel(&mut self) -> Result<(), String>;
    async fn attach_or_start(&mut self) -> Result<String, String>;
    async fn install_bootstrap_token(&mut self, token: &str) -> Result<(), String>;
}

/// Establishes a remote engine connection for the first time. The remote
/// attach-or-start operation happens before the tunnel is exposed locally, and
/// the local token is installed atomically by the runtime implementation.
pub async fn initialize_remote<R: RemoteRecoveryRuntime>(
    runtime: &mut R,
) -> Result<RecoveryOutcome, String> {
    let token = runtime.attach_or_start().await?;
    runtime.install_bootstrap_token(&token).await?;
    runtime.restart_tunnel().await?;
    if runtime.authenticated_health().await? {
        Ok(RecoveryOutcome::EngineAndTunnelRecovered)
    } else {
        Err("forwarded remote engine failed authenticated health after startup".to_owned())
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

    let token = runtime.attach_or_start().await?;
    runtime.install_bootstrap_token(&token).await?;
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
            .body(Empty::<Bytes>::new())
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
            permission_mode: RemotePermissionMode::Strict,
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
        assert_eq!(candidate.validate(), Err(RemoteError::InvalidWorkspace));
    }

    #[test]
    fn remote_never_relaxes_permissions_or_accepts_ssh_option_injection() {
        for mode in [RemotePermissionMode::AutoSafe, RemotePermissionMode::Yolo] {
            let mut candidate = config();
            candidate.permission_mode = mode;
            assert_eq!(
                candidate.validate(),
                Err(RemoteError::StrictPermissionRequired)
            );
        }
        let mut candidate = config();
        candidate.host = "-oProxyCommand=bad".to_owned();
        assert_eq!(candidate.validate(), Err(RemoteError::InvalidHost));
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
        attach_tokens: VecDeque<Result<String, String>>,
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
            self.0.lock().await.calls.push("restart_tunnel");
            Ok(())
        }

        async fn attach_or_start(&mut self) -> Result<String, String> {
            let mut state = self.0.lock().await;
            state.calls.push("attach_or_start");
            state
                .attach_tokens
                .pop_front()
                .unwrap_or_else(|| Err("missing mock attach result".to_owned()))
        }

        async fn install_bootstrap_token(&mut self, token: &str) -> Result<(), String> {
            let mut state = self.0.lock().await;
            state.calls.push("install_token");
            state.installed.push(token.to_owned());
            Ok(())
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
            attach_tokens: VecDeque::from([Ok("b".repeat(64))]),
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
                "attach_or_start",
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
            attach_tokens: VecDeque::from([Ok("c".repeat(64))]),
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
                "attach_or_start",
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
            attach_tokens: VecDeque::from([Ok("d".repeat(64))]),
            ..MockRecoveryState::default()
        });

        assert_eq!(
            initialize_remote(&mut runtime).await.expect("initialized"),
            RecoveryOutcome::EngineAndTunnelRecovered
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
