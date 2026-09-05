//! Process supervision for the local engine and compiled TUI.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io,
    os::unix::process::{CommandExt as _, ExitStatusExt as _},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use rw_core::SequenceId;
use tokio::{
    io::AsyncReadExt as _,
    process::{Child, Command},
};

const SOCKET_ENV: &str = "ROTTWEILER_ENGINE_SOCKET";
const TOKEN_FILE_ENV: &str = "ROTTWEILER_ENGINE_TOKEN_FILE";
const SESSION_ENV: &str = "ROTTWEILER_SESSION_ID";
const LAST_SEEN_ENV: &str = "ROTTWEILER_LAST_SEEN_SEQUENCE";
const LAST_SEEN_FILE_ENV: &str = "ROTTWEILER_LAST_SEEN_FILE";
const TUI_RECYCLE_STATE_FILE_ENV: &str = "ROTTWEILER_TUI_RECYCLE_STATE_FILE";
const FORK_OPERATION_DIRECTORY_ENV: &str = "ROTTWEILER_FORK_OPERATION_DIRECTORY";
const WAIT_FOR_EXECUTION_LEASE_ARG: &str = "--wait-for-execution-lease";
const TUI_KEYBINDINGS_ENV: &str = "ROTTWEILER_TUI_KEYBINDINGS";
const TUI_THEME_ENV: &str = "ROTTWEILER_TUI_THEME";
const SUPERVISOR_PID_ENV: &str = crate::parent_death::SUPERVISOR_PID_ENV;
const ENGINE_STDERR_TAIL_BYTES: usize = 16 * 1024;
const TUI_RECYCLE_EXIT_CODE: i32 = 75;

type ShellBrokerResult = Result<(), crate::shell_broker::ShellBrokerError>;
type ShellBrokerTask = tokio::task::JoinHandle<ShellBrokerResult>;
type ShellBrokerReady = tokio::sync::oneshot::Receiver<Result<(), String>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StdioMode {
    Inherit,
    Null,
    CaptureStderr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: BTreeMap<OsString, OsString>,
    pub stdio: StdioMode,
    pub new_process_group: bool,
}

#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    pub rw_executable: PathBuf,
    pub tui_executable: PathBuf,
    pub socket: PathBuf,
    pub token_file: PathBuf,
    pub last_seen_file: PathBuf,
    pub fork_operation_directory: PathBuf,
    pub session_id: String,
    pub tui_keybindings: Option<String>,
    pub tui_theme: String,
    pub permission_mode: Option<crate::PermissionMode>,
    pub max_turns: usize,
    pub model: Option<String>,
    pub additional_workspaces: Vec<PathBuf>,
    pub dangerously_trust: bool,
    pub in_memory_replay_script: Option<PathBuf>,
    pub record_script_delay_ms: u64,
    pub shell_target: Option<crate::shell_broker::ShellTarget>,
    pub detach: bool,
    pub restart_policy: RestartPolicy,
}

#[derive(Clone, Copy, Debug)]
pub struct RestartPolicy {
    pub max_consecutive_failures: u32,
    pub initial_delay: Duration,
    pub maximum_delay: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_consecutive_failures: 5,
            initial_delay: Duration::from_millis(50),
            maximum_delay: Duration::from_secs(2),
        }
    }
}

#[derive(Debug)]
pub enum SupervisorError {
    InvalidConfig(&'static str),
    Spawn {
        component: &'static str,
        source: io::Error,
    },
    Wait {
        component: &'static str,
        source: io::Error,
    },
    RestartBudgetExhausted,
    Readiness(io::Error),
    ShellBroker(String),
    Signal(io::Error),
}

enum EngineStartOutcome {
    Ready(io::Result<()>),
    Exited {
        status: io::Result<ExitStatus>,
        stderr_tail: Option<String>,
    },
}

impl std::fmt::Display for SupervisorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(message) => formatter.write_str(message),
            Self::Spawn { component, source } => {
                write!(formatter, "could not spawn {component}: {source}")
            }
            Self::Wait { component, source } => {
                write!(formatter, "could not wait for {component}: {source}")
            }
            Self::RestartBudgetExhausted => formatter.write_str("restart budget exhausted"),
            Self::Readiness(error) => write!(formatter, "engine did not become ready: {error}"),
            Self::ShellBroker(error) => {
                write!(formatter, "foreground-shell broker failed: {error}")
            }
            Self::Signal(error) => write!(formatter, "could not monitor shutdown signals: {error}"),
        }
    }
}

impl std::error::Error for SupervisorError {}

#[derive(Clone, Default)]
pub struct ResumeHandoff {
    last_seen: Arc<Mutex<Option<SequenceId>>>,
}

impl ResumeHandoff {
    pub fn update(&self, last_seen: Option<SequenceId>) {
        *self
            .last_seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = last_seen;
    }

    #[must_use]
    pub fn last_seen(&self) -> Option<SequenceId> {
        *self
            .last_seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessSignal {
    Interrupt,
    Terminate,
    Kill,
    WindowChanged,
}

#[async_trait]
pub trait ManagedChild: Send + 'static {
    async fn wait(&mut self) -> io::Result<ExitStatus>;
    async fn stderr_tail(&mut self) -> io::Result<Option<String>> {
        Ok(None)
    }
    fn signal_group(&self, signal: ProcessSignal) -> io::Result<()>;
}

#[async_trait]
pub trait ProcessBackend: Send + Sync {
    type Child: ManagedChild;

    async fn spawn(&self, spec: ChildSpec) -> io::Result<Self::Child>;

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn wait_ready(&self, _socket: &Path, _token_file: &Path) -> io::Result<()> {
        Ok(())
    }

    async fn wait_shutdown_signal(&self) -> io::Result<()> {
        std::future::pending::<()>().await;
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct TokioProcessBackend;

pub struct TokioManagedChild {
    child: Child,
    pid: Option<rustix::process::Pid>,
    process_group: Option<rustix::process::Pid>,
    stderr_reader: Option<tokio::task::JoinHandle<io::Result<Vec<u8>>>>,
}

#[async_trait]
impl ManagedChild for TokioManagedChild {
    async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    async fn stderr_tail(&mut self) -> io::Result<Option<String>> {
        let Some(reader) = self.stderr_reader.take() else {
            return Ok(None);
        };
        let bytes = reader
            .await
            .map_err(|error| io::Error::other(format!("stderr reader failed: {error}")))??;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn signal_group(&self, signal: ProcessSignal) -> io::Result<()> {
        let Some(pid) = self.pid else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child has no process id",
            ));
        };
        let signal = match signal {
            ProcessSignal::Interrupt => rustix::process::Signal::INT,
            ProcessSignal::Terminate => rustix::process::Signal::TERM,
            ProcessSignal::Kill => rustix::process::Signal::KILL,
            ProcessSignal::WindowChanged => rustix::process::Signal::WINCH,
        };
        if let Some(group) = self.process_group {
            Ok(rustix::process::kill_process_group(group, signal)?)
        } else {
            Ok(rustix::process::kill_process(pid, signal)?)
        }
    }
}

#[async_trait]
impl ProcessBackend for TokioProcessBackend {
    type Child = TokioManagedChild;

    async fn spawn(&self, spec: ChildSpec) -> io::Result<Self::Child> {
        let mut command = command_from_spec(&spec);
        match spec.stdio {
            StdioMode::Inherit => {
                command
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
            }
            StdioMode::Null => {
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
            }
            StdioMode::CaptureStderr => {
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped());
            }
        }
        if spec.new_process_group {
            command.as_std_mut().process_group(0);
        }
        let mut child = command.spawn()?;
        let stderr_reader = child.stderr.take().map(|stderr| {
            tokio::spawn(async move {
                let mut stderr = stderr;
                let mut tail = Vec::with_capacity(ENGINE_STDERR_TAIL_BYTES);
                let mut chunk = [0_u8; 4096];
                loop {
                    let read = stderr.read(&mut chunk).await?;
                    if read == 0 {
                        break;
                    }
                    tail.extend_from_slice(&chunk[..read]);
                    if tail.len() > ENGINE_STDERR_TAIL_BYTES {
                        tail.drain(..tail.len() - ENGINE_STDERR_TAIL_BYTES);
                    }
                }
                Ok(tail)
            })
        });
        let pid = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw);
        let process_group = spec.new_process_group.then_some(pid).flatten();
        Ok(TokioManagedChild {
            child,
            pid,
            process_group,
            stderr_reader,
        })
    }

    async fn wait_ready(&self, socket: &Path, token_file: &Path) -> io::Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let socket_ready = std::fs::symlink_metadata(socket).is_ok_and(|metadata| {
                use std::os::unix::fs::FileTypeExt as _;
                !metadata.file_type().is_symlink() && metadata.file_type().is_socket()
            });
            let token_ready = std::fs::symlink_metadata(token_file).is_ok_and(|metadata| {
                !metadata.file_type().is_symlink() && metadata.is_file() && metadata.len() == 64
            });
            if socket_ready && token_ready {
                let token = std::fs::read_to_string(token_file)?;
                if crate::remote::probe_authenticated_health(
                    socket,
                    token.trim(),
                    Duration::from_millis(250),
                )
                .await
                .unwrap_or(false)
                {
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "engine did not pass authenticated health within 30 seconds",
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_shutdown_signal(&self) -> io::Result<()> {
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
        Ok(())
    }
}

fn command_from_spec(spec: &ChildSpec) -> Command {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_remove(TUI_KEYBINDINGS_ENV)
        .env_remove(TUI_THEME_ENV)
        .envs(&spec.env);
    command
}

#[derive(Clone, Copy, Debug)]
struct RestartBudget {
    policy: RestartPolicy,
    failures: u32,
}

impl RestartBudget {
    fn new(policy: RestartPolicy) -> Self {
        Self {
            policy,
            failures: 0,
        }
    }

    fn failure_delay(&mut self) -> Result<Duration, SupervisorError> {
        if self.failures >= self.policy.max_consecutive_failures {
            return Err(SupervisorError::RestartBudgetExhausted);
        }
        let exponent = self.failures.min(31);
        self.failures = self.failures.saturating_add(1);
        let multiplier = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
        Ok(self
            .policy
            .initial_delay
            .saturating_mul(multiplier)
            .min(self.policy.maximum_delay))
    }
}

pub struct Supervisor<B: ProcessBackend> {
    config: SupervisorConfig,
    backend: B,
    handoff: ResumeHandoff,
}

impl<B: ProcessBackend> Supervisor<B> {
    pub fn new(
        config: SupervisorConfig,
        backend: B,
        handoff: ResumeHandoff,
    ) -> Result<Self, SupervisorError> {
        validate_private_path(&config.socket)?;
        validate_private_path(&config.token_file)?;
        validate_private_path(&config.last_seen_file)?;
        if rw_core::SessionId::validate(&config.session_id).is_err() {
            return Err(SupervisorError::InvalidConfig("session id is invalid"));
        }
        if config.max_turns == 0 {
            return Err(SupervisorError::InvalidConfig(
                "maximum provider turns must not be zero",
            ));
        }
        if config.restart_policy.max_consecutive_failures == 0 {
            return Err(SupervisorError::InvalidConfig(
                "restart budget must not be zero",
            ));
        }
        Ok(Self {
            config,
            backend,
            handoff,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub async fn run(&self) -> Result<(), SupervisorError> {
        self.run_with_shell_broker(|| self.start_shell_broker())
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn run_with_shell_broker(
        &self,
        start_shell_broker: impl FnOnce() -> (ShellBrokerTask, Option<ShellBrokerReady>),
    ) -> Result<(), SupervisorError> {
        let mut budget = RestartBudget::new(self.config.restart_policy);
        let mut engine = None;
        let mut tui = None;
        let mut shell_broker = None;
        // Construct this future before the first spawn and poll it first in
        // every startup race. Tokio installs the Unix handlers on that first
        // poll, before any independently grouped child can exist.
        let shutdown_signal = self.backend.wait_shutdown_signal();
        tokio::pin!(shutdown_signal);
        let managed_result: Result<(), SupervisorError> = async {
            macro_rules! await_or_shutdown {
                ($future:expr) => {{
                    tokio::select! {
                        biased;
                        signal = &mut shutdown_signal => {
                            signal.map_err(SupervisorError::Signal)?;
                            None
                        }
                        output = $future => Some(output),
                    }
                }};
            }

            let Some(spawned) = await_or_shutdown!(self.spawn_engine(false)) else {
                return Ok(());
            };
            engine = Some(spawned?);
            let active_engine = engine.as_mut().ok_or(SupervisorError::InvalidConfig(
                "engine state missing after spawn",
            ))?;
            let Some(startup) = await_or_shutdown!(self.wait_for_engine_start(active_engine))
            else {
                return Ok(());
            };
            Self::resolve_engine_start(&mut engine, startup)?;
            let Some(spawned) = await_or_shutdown!(self.spawn_tui()) else {
                return Ok(());
            };
            tui = Some(spawned?);

            let (broker_task, mut broker_ready) = start_shell_broker();
            shell_broker = Some(broker_task);

            loop {
                enum RuntimeEvent {
                    Shutdown(io::Result<()>),
                    Engine(io::Result<ExitStatus>),
                    Tui(io::Result<ExitStatus>),
                    BrokerReady(Result<Result<(), String>, tokio::sync::oneshot::error::RecvError>),
                    Broker(Result<ShellBrokerResult, tokio::task::JoinError>),
                }

                let event = {
                    let engine_child = engine
                        .as_mut()
                        .ok_or(SupervisorError::InvalidConfig("engine child missing"))?;
                    let tui_child = tui
                        .as_mut()
                        .ok_or(SupervisorError::InvalidConfig("TUI child missing"))?;
                    let broker_task = shell_broker
                        .as_mut()
                        .ok_or(SupervisorError::InvalidConfig("shell broker missing"))?;
                    tokio::select! {
                        biased;
                        signal = &mut shutdown_signal => RuntimeEvent::Shutdown(signal),
                        status = engine_child.wait() => RuntimeEvent::Engine(status),
                        status = tui_child.wait() => RuntimeEvent::Tui(status),
                        readiness = async {
                            match broker_ready.as_mut() {
                                Some(receiver) => receiver.await,
                                None => std::future::pending().await,
                            }
                        } => RuntimeEvent::BrokerReady(readiness),
                        broker = broker_task => RuntimeEvent::Broker(broker),
                    }
                };

                match event {
                    RuntimeEvent::Shutdown(signal) => {
                        signal.map_err(SupervisorError::Signal)?;
                        return Ok(());
                    }
                    RuntimeEvent::Engine(status) => {
                        let status = status.map_err(|source| SupervisorError::Wait {
                            component: "engine",
                            source,
                        })?;
                        engine.take();
                        // The authenticated ShutdownHost path exits the engine
                        // successfully. Treat that as the user's one-app close,
                        // then let the common cleanup reap the still-rendering
                        // TUI instead of restarting both processes.
                        if status.success() {
                            return Ok(());
                        }
                        if let Some(mut child) = tui.take() {
                            terminate_and_reap(&mut child, "TUI").await?;
                        }
                        let delay = budget.failure_delay()?;
                        if await_or_shutdown!(self.backend.sleep(delay)).is_none() {
                            return Ok(());
                        }
                        eprintln!(
                            "Rottweiler is waiting briefly for the active session in this workspace to finish · Ctrl+C to cancel"
                        );
                        let Some(spawned) = await_or_shutdown!(self.spawn_engine(true)) else {
                            return Ok(());
                        };
                        engine = Some(spawned?);
                        let active_engine = engine.as_mut().ok_or(
                            SupervisorError::InvalidConfig("engine state missing after restart"),
                        )?;
                        let Some(startup) =
                            await_or_shutdown!(self.wait_for_engine_start(active_engine))
                        else {
                            return Ok(());
                        };
                        Self::resolve_engine_start(&mut engine, startup)?;
                        let Some(spawned) = await_or_shutdown!(self.spawn_tui()) else {
                            return Ok(());
                        };
                        tui = Some(spawned?);
                    }
                    RuntimeEvent::Tui(status) => {
                        let status = status.map_err(|source| SupervisorError::Wait {
                            component: "TUI",
                            source,
                        })?;
                        tui.take();
                        if tui_exit_is_user_close(status) {
                            if self.config.detach
                                && let Some(mut detached_engine) = engine.take()
                            {
                                tokio::spawn(async move {
                                    let _ = detached_engine.wait().await;
                                });
                            }
                            return Ok(());
                        }
                        // Exit 75 is a deliberate process-local memory recycle.
                        // Durable session state remains in the engine, so this
                        // is neither a crash nor a consecutive startup failure.
                        if !tui_exit_is_recycle(status) {
                            let delay = budget.failure_delay()?;
                            if await_or_shutdown!(self.backend.sleep(delay)).is_none() {
                                return Ok(());
                            }
                        }
                        let Some(spawned) = await_or_shutdown!(self.spawn_tui()) else {
                            return Ok(());
                        };
                        tui = Some(spawned?);
                    }
                    RuntimeEvent::BrokerReady(readiness) => match readiness {
                        Ok(Ok(())) => broker_ready = None,
                        Ok(Err(error)) => return Err(SupervisorError::ShellBroker(error)),
                        Err(error) => {
                            return Err(SupervisorError::ShellBroker(error.to_string()));
                        }
                    },
                    RuntimeEvent::Broker(broker) => {
                        shell_broker.take();
                        let message = match broker {
                            Ok(Ok(())) => "foreground-shell broker stopped unexpectedly".to_owned(),
                            Ok(Err(error)) => error.to_string(),
                            Err(error) => error.to_string(),
                        };
                        return Err(SupervisorError::ShellBroker(message));
                    }
                }
            }
        }
        .await;

        let cleanup_result =
            cleanup_managed_children(&mut engine, &mut tui, &mut shell_broker).await;
        match managed_result {
            Err(error) => Err(error),
            Ok(()) => cleanup_result,
        }
    }

    fn start_shell_broker(&self) -> (ShellBrokerTask, Option<ShellBrokerReady>) {
        let Some(target) = self.config.shell_target.clone() else {
            return (tokio::spawn(std::future::pending()), None);
        };
        let (ready, ready_rx) = tokio::sync::oneshot::channel();
        let config = crate::shell_broker::ShellBrokerConfig {
            socket: self.config.socket.clone(),
            token_file: self.config.token_file.clone(),
            session_id: rw_core::SessionId(self.config.session_id.clone()),
            target,
        };
        (
            tokio::spawn(crate::shell_broker::run(config, ready)),
            Some(ready_rx),
        )
    }

    async fn spawn_engine(
        &self,
        wait_for_execution_lease: bool,
    ) -> Result<B::Child, SupervisorError> {
        remove_stale_runtime_file(&self.config.socket, RuntimeFileKind::Socket)
            .map_err(SupervisorError::Readiness)?;
        remove_stale_runtime_file(&self.config.token_file, RuntimeFileKind::Regular)
            .map_err(SupervisorError::Readiness)?;
        let mut spec = engine_spec(&self.config);
        if wait_for_execution_lease {
            spec.args.push(OsString::from(WAIT_FOR_EXECUTION_LEASE_ARG));
        }
        self.backend
            .spawn(spec)
            .await
            .map_err(|source| SupervisorError::Spawn {
                component: "engine",
                source,
            })
    }

    async fn wait_for_engine_start(&self, engine: &mut B::Child) -> EngineStartOutcome {
        tokio::select! {
            biased;
            status = engine.wait() => {
                let stderr_tail = engine.stderr_tail().await.ok().flatten().map(|tail| {
                    sanitize_engine_stderr(&tail, &self.config.token_file)
                });
                EngineStartOutcome::Exited { status, stderr_tail }
            },
            readiness = self.backend.wait_ready(&self.config.socket, &self.config.token_file) => {
                EngineStartOutcome::Ready(readiness)
            }
        }
    }

    fn resolve_engine_start(
        engine: &mut Option<B::Child>,
        outcome: EngineStartOutcome,
    ) -> Result<(), SupervisorError> {
        match outcome {
            EngineStartOutcome::Ready(readiness) => readiness.map_err(SupervisorError::Readiness),
            EngineStartOutcome::Exited {
                status: Ok(status),
                stderr_tail,
            } => {
                // wait() consumed this process. Relinquish cleanup ownership so
                // a recycled PID/process-group id can never be signalled.
                engine.take();
                let detail = stderr_tail
                    .filter(|tail| !tail.trim().is_empty())
                    .map_or_else(
                        || "; another Rottweiler process may already own this session".to_owned(),
                        |tail| format!(": {}", tail.trim()),
                    );
                Err(SupervisorError::Readiness(io::Error::other(format!(
                    "engine exited before authenticated readiness ({status}){detail}",
                ))))
            }
            EngineStartOutcome::Exited {
                status: Err(source),
                ..
            } => Err(SupervisorError::Wait {
                component: "engine",
                source,
            }),
        }
    }

    async fn spawn_tui(&self) -> Result<B::Child, SupervisorError> {
        let persisted = read_resume_handoff(&self.config.last_seen_file);
        self.backend
            .spawn(tui_spec(
                &self.config,
                persisted.or_else(|| self.handoff.last_seen()),
            ))
            .await
            .map_err(|source| SupervisorError::Spawn {
                component: "TUI",
                source,
            })
    }
}

fn tui_exit_is_user_close(status: ExitStatus) -> bool {
    status.success()
        || status.code() == Some(130)
        || status.signal() == Some(rustix::process::Signal::INT.as_raw())
}

fn tui_exit_is_recycle(status: ExitStatus) -> bool {
    status.code() == Some(TUI_RECYCLE_EXIT_CODE)
}

#[derive(Clone, Copy)]
enum RuntimeFileKind {
    Socket,
    Regular,
}

fn remove_stale_runtime_file(path: &Path, expected: RuntimeFileKind) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            use std::os::unix::fs::FileTypeExt as _;
            let matches = match expected {
                RuntimeFileKind::Socket => metadata.file_type().is_socket(),
                RuntimeFileKind::Regular => metadata.is_file(),
            };
            if metadata.file_type().is_symlink() || !matches {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected runtime artifact type",
                ));
            }
            std::fs::remove_file(path)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn engine_spec(config: &SupervisorConfig) -> ChildSpec {
    let mut args = Vec::new();
    if let Some(script) = &config.in_memory_replay_script {
        args.extend([
            OsString::from("--in-memory-replay-script"),
            script.as_os_str().to_owned(),
        ]);
    }
    if config.record_script_delay_ms != 0 {
        args.extend([
            OsString::from("--record-script-delay-ms"),
            OsString::from(config.record_script_delay_ms.to_string()),
        ]);
    }
    args.extend([
        OsString::from("serve"),
        OsString::from("--max-turns"),
        OsString::from(config.max_turns.to_string()),
    ]);
    if let Some(mode) = config.permission_mode {
        args.extend([
            OsString::from("--permission-mode"),
            OsString::from(mode.as_str()),
        ]);
    }
    if let Some(model) = &config.model {
        args.extend([OsString::from("--model"), OsString::from(model)]);
    }
    for root in &config.additional_workspaces {
        args.extend([OsString::from("--add-dir"), root.as_os_str().to_owned()]);
    }
    if config.dangerously_trust {
        args.push(OsString::from("--dangerously-trust"));
    }
    let mut env = connection_env(config, None);
    if config.detach {
        env.remove(&OsString::from(SUPERVISOR_PID_ENV));
    }
    ChildSpec {
        program: config.rw_executable.clone(),
        args,
        env,
        stdio: if config.in_memory_replay_script.is_some() {
            // Hidden deterministic harnesses retain engine diagnostics in the
            // owning PTY. Production live-provider launches remain quiet.
            StdioMode::Inherit
        } else {
            StdioMode::CaptureStderr
        },
        new_process_group: true,
    }
}

fn sanitize_engine_stderr(value: &str, token_file: &Path) -> String {
    let token = std::fs::read_to_string(token_file).ok();
    let mut sanitized = token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map_or_else(
            || value.to_owned(),
            |token| value.replace(token, "[REDACTED]"),
        );
    sanitized.retain(|character| character == '\n' || character == '\t' || !character.is_control());
    if sanitized.len() > ENGINE_STDERR_TAIL_BYTES {
        let mut start = sanitized.len() - ENGINE_STDERR_TAIL_BYTES;
        while !sanitized.is_char_boundary(start) {
            start += 1;
        }
        sanitized = sanitized[start..].to_owned();
    }
    sanitized
}

fn tui_spec(config: &SupervisorConfig, last_seen: Option<SequenceId>) -> ChildSpec {
    let mut env = connection_env(config, last_seen);
    if let Some(keybindings) = &config.tui_keybindings {
        env.insert(
            OsString::from(TUI_KEYBINDINGS_ENV),
            OsString::from(keybindings),
        );
    }
    env.insert(
        OsString::from(TUI_THEME_ENV),
        OsString::from(&config.tui_theme),
    );
    ChildSpec {
        program: config.tui_executable.clone(),
        args: Vec::new(),
        env,
        stdio: StdioMode::Inherit,
        // The TUI must remain in `rw`'s foreground process group so it can
        // actually own the controlling terminal. Foreground shell handover
        // creates its own child group at the broker boundary.
        new_process_group: false,
    }
}

fn connection_env(
    config: &SupervisorConfig,
    last_seen: Option<SequenceId>,
) -> BTreeMap<OsString, OsString> {
    let mut env = BTreeMap::from([
        (
            OsString::from(SOCKET_ENV),
            config.socket.as_os_str().to_owned(),
        ),
        (
            OsString::from(TOKEN_FILE_ENV),
            config.token_file.as_os_str().to_owned(),
        ),
        (
            OsString::from(SESSION_ENV),
            OsString::from(&config.session_id),
        ),
        (
            OsString::from(LAST_SEEN_FILE_ENV),
            config.last_seen_file.as_os_str().to_owned(),
        ),
        (
            OsString::from(TUI_RECYCLE_STATE_FILE_ENV),
            config
                .last_seen_file
                .with_file_name("tui-recycle-state.json")
                .into_os_string(),
        ),
        (
            OsString::from(FORK_OPERATION_DIRECTORY_ENV),
            config.fork_operation_directory.as_os_str().to_owned(),
        ),
        (
            OsString::from(SUPERVISOR_PID_ENV),
            OsString::from(std::process::id().to_string()),
        ),
    ]);
    if let Some(last_seen) = last_seen {
        env.insert(
            OsString::from(LAST_SEEN_ENV),
            OsString::from(last_seen.0.to_string()),
        );
    }
    env
}

fn read_resume_handoff(path: &Path) -> Option<SequenceId> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 128 {
        return None;
    }
    let value = std::fs::read_to_string(path).ok()?;
    value.trim().parse::<u64>().ok().map(SequenceId)
}

async fn cleanup_managed_children<C: ManagedChild>(
    engine: &mut Option<C>,
    tui: &mut Option<C>,
    shell_broker: &mut Option<ShellBrokerTask>,
) -> Result<(), SupervisorError> {
    if let Some(task) = shell_broker.take() {
        task.abort();
        let _ = task.await;
    }
    let tui_result = if let Some(mut child) = tui.take() {
        terminate_and_reap(&mut child, "TUI").await
    } else {
        Ok(())
    };
    let engine_result = if let Some(mut child) = engine.take() {
        terminate_and_reap(&mut child, "engine").await
    } else {
        Ok(())
    };
    tui_result.and(engine_result)
}

async fn terminate_and_reap(
    child: &mut impl ManagedChild,
    component: &'static str,
) -> Result<(), SupervisorError> {
    terminate_and_reap_with_grace(child, component, Duration::from_secs(5)).await
}

async fn terminate_and_reap_with_grace(
    child: &mut impl ManagedChild,
    component: &'static str,
    grace: Duration,
) -> Result<(), SupervisorError> {
    let _ = child.signal_group(ProcessSignal::Terminate);
    if matches!(tokio::time::timeout(grace, child.wait()).await, Ok(Ok(_))) {
        return Ok(());
    }
    child
        .signal_group(ProcessSignal::Kill)
        .map_err(|source| SupervisorError::Wait { component, source })?;
    tokio::time::timeout(grace, child.wait())
        .await
        .map_err(|_| SupervisorError::Wait {
            component,
            source: io::Error::new(io::ErrorKind::TimedOut, "child did not exit after SIGKILL"),
        })?
        .map(|_| ())
        .map_err(|source| SupervisorError::Wait { component, source })
}

fn validate_private_path(path: &Path) -> Result<(), SupervisorError> {
    if !path.is_absolute() || path.as_os_str().is_empty() {
        return Err(SupervisorError::InvalidConfig(
            "engine socket and token file paths must be absolute",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
