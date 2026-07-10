//! Process supervision for the local engine and compiled TUI.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io,
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use rw_core::SequenceId;
use tokio::process::{Child, Command};

const SOCKET_ENV: &str = "ROTTWEILER_ENGINE_SOCKET";
const TOKEN_FILE_ENV: &str = "ROTTWEILER_ENGINE_TOKEN_FILE";
const SESSION_ENV: &str = "ROTTWEILER_SESSION_ID";
const LAST_SEEN_ENV: &str = "ROTTWEILER_LAST_SEEN_SEQUENCE";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StdioMode {
    Inherit,
    Null,
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
    pub session_id: String,
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
    WindowChanged,
}

#[async_trait]
pub trait ManagedChild: Send + 'static {
    async fn wait(&mut self) -> io::Result<ExitStatus>;
    fn signal_group(&self, signal: ProcessSignal) -> io::Result<()>;
}

#[async_trait]
pub trait ProcessBackend: Send + Sync {
    type Child: ManagedChild;

    async fn spawn(&self, spec: ChildSpec) -> io::Result<Self::Child>;

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[derive(Debug, Default)]
pub struct TokioProcessBackend;

pub struct TokioManagedChild {
    child: Child,
    process_group: Option<rustix::process::Pid>,
}

#[async_trait]
impl ManagedChild for TokioManagedChild {
    async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    fn signal_group(&self, signal: ProcessSignal) -> io::Result<()> {
        let Some(group) = self.process_group else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child has no process group",
            ));
        };
        let signal = match signal {
            ProcessSignal::Interrupt => rustix::process::Signal::INT,
            ProcessSignal::Terminate => rustix::process::Signal::TERM,
            ProcessSignal::WindowChanged => rustix::process::Signal::WINCH,
        };
        Ok(rustix::process::kill_process_group(group, signal)?)
    }
}

#[async_trait]
impl ProcessBackend for TokioProcessBackend {
    type Child = TokioManagedChild;

    async fn spawn(&self, spec: ChildSpec) -> io::Result<Self::Child> {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args).envs(&spec.env);
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
        }
        if spec.new_process_group {
            command.as_std_mut().process_group(0);
        }
        let child = command.spawn()?;
        let process_group = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw);
        Ok(TokioManagedChild {
            child,
            process_group,
        })
    }
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
        if config.session_id.is_empty() {
            return Err(SupervisorError::InvalidConfig(
                "session id must not be empty",
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

    pub async fn run(&self) -> Result<(), SupervisorError> {
        let mut budget = RestartBudget::new(self.config.restart_policy);
        let mut engine = self.spawn_engine().await?;
        let mut tui = self.spawn_tui_guarded(&mut engine).await?;
        loop {
            tokio::select! {
                status = engine.wait() => {
                    status.map_err(|source| SupervisorError::Wait { component: "engine", source })?;
                    terminate_and_reap(&mut tui, "TUI").await?;
                    self.backend.sleep(budget.failure_delay()?).await;
                    engine = self.spawn_engine().await?;
                    tui = self.spawn_tui_guarded(&mut engine).await?;
                }
                status = tui.wait() => {
                    let status = status.map_err(|source| SupervisorError::Wait { component: "TUI", source })?;
                    if status.success() {
                        if self.config.detach {
                            tokio::spawn(async move { let _ = engine.wait().await; });
                        } else {
                            terminate_and_reap(&mut engine, "engine").await?;
                        }
                        return Ok(());
                    }
                    self.backend.sleep(budget.failure_delay()?).await;
                    tui = self.spawn_tui_guarded(&mut engine).await?;
                }
            }
        }
    }

    async fn spawn_engine(&self) -> Result<B::Child, SupervisorError> {
        self.backend
            .spawn(engine_spec(&self.config))
            .await
            .map_err(|source| SupervisorError::Spawn {
                component: "engine",
                source,
            })
    }

    async fn spawn_tui(&self) -> Result<B::Child, SupervisorError> {
        self.backend
            .spawn(tui_spec(&self.config, self.handoff.last_seen()))
            .await
            .map_err(|source| SupervisorError::Spawn {
                component: "TUI",
                source,
            })
    }

    async fn spawn_tui_guarded(&self, engine: &mut B::Child) -> Result<B::Child, SupervisorError> {
        match self.spawn_tui().await {
            Ok(tui) => Ok(tui),
            Err(error) => {
                terminate_and_reap(engine, "engine").await?;
                Err(error)
            }
        }
    }
}

fn engine_spec(config: &SupervisorConfig) -> ChildSpec {
    ChildSpec {
        program: config.rw_executable.clone(),
        args: vec![OsString::from("serve")],
        env: connection_env(config, None),
        stdio: StdioMode::Null,
        new_process_group: true,
    }
}

fn tui_spec(config: &SupervisorConfig, last_seen: Option<SequenceId>) -> ChildSpec {
    ChildSpec {
        program: config.tui_executable.clone(),
        args: Vec::new(),
        env: connection_env(config, last_seen),
        stdio: StdioMode::Inherit,
        new_process_group: true,
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
    ]);
    if let Some(last_seen) = last_seen {
        env.insert(
            OsString::from(LAST_SEEN_ENV),
            OsString::from(last_seen.0.to_string()),
        );
    }
    env
}

async fn terminate_and_reap(
    child: &mut impl ManagedChild,
    component: &'static str,
) -> Result<(), SupervisorError> {
    child
        .signal_group(ProcessSignal::Terminate)
        .map_err(|source| SupervisorError::Wait { component, source })?;
    child
        .wait()
        .await
        .map_err(|source| SupervisorError::Wait { component, source })?;
    Ok(())
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
mod tests {
    #![allow(clippy::expect_used)]

    use std::{
        os::unix::process::ExitStatusExt as _,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use super::*;

    fn fixture_config() -> SupervisorConfig {
        SupervisorConfig {
            rw_executable: PathBuf::from("/bin/rw"),
            tui_executable: PathBuf::from("/bin/rottweiler-tui"),
            socket: PathBuf::from("/private/run/engine.sock"),
            token_file: PathBuf::from("/private/run/auth.token"),
            session_id: "session-1".to_owned(),
            detach: false,
            restart_policy: RestartPolicy::default(),
        }
    }

    #[test]
    fn token_is_passed_by_private_file_environment_never_argv() {
        let config = fixture_config();
        for spec in [
            engine_spec(&config),
            tui_spec(&config, Some(SequenceId(41))),
        ] {
            assert!(!spec.args.iter().any(|argument| argument == "auth.token"));
            assert_eq!(
                spec.env.get(&OsString::from(TOKEN_FILE_ENV)),
                Some(&OsString::from("/private/run/auth.token"))
            );
            assert!(spec.new_process_group);
        }
        let tui = tui_spec(&config, Some(SequenceId(41)));
        assert_eq!(
            tui.env.get(&OsString::from(LAST_SEEN_ENV)),
            Some(&OsString::from("41"))
        );
    }

    #[test]
    fn restart_budget_is_bounded_exponential() {
        let mut budget = RestartBudget::new(RestartPolicy {
            max_consecutive_failures: 3,
            initial_delay: Duration::from_millis(10),
            maximum_delay: Duration::from_millis(25),
        });
        assert_eq!(
            budget.failure_delay().expect("first"),
            Duration::from_millis(10)
        );
        assert_eq!(
            budget.failure_delay().expect("second"),
            Duration::from_millis(20)
        );
        assert_eq!(
            budget.failure_delay().expect("third"),
            Duration::from_millis(25)
        );
        assert!(matches!(
            budget.failure_delay(),
            Err(SupervisorError::RestartBudgetExhausted)
        ));
    }

    #[derive(Clone, Copy)]
    enum Scenario {
        EngineCrash,
        TuiCrash,
    }

    struct MockBackend {
        scenario: Scenario,
        spawned: Arc<Mutex<Vec<ChildSpec>>>,
        lifecycle: Arc<Mutex<Vec<String>>>,
        count: AtomicUsize,
    }

    struct MockChild {
        name: &'static str,
        outcome: Option<ExitStatus>,
        terminated: AtomicBool,
        lifecycle: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl ManagedChild for MockChild {
        async fn wait(&mut self) -> io::Result<ExitStatus> {
            while self.outcome.is_none() && !self.terminated.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            self.lifecycle
                .lock()
                .expect("lifecycle")
                .push(format!("wait:{}", self.name));
            Ok(self.outcome.unwrap_or_else(|| ExitStatus::from_raw(0)))
        }

        fn signal_group(&self, signal: ProcessSignal) -> io::Result<()> {
            self.lifecycle
                .lock()
                .expect("lifecycle")
                .push(format!("signal:{}:{signal:?}", self.name));
            self.terminated.store(true, Ordering::Release);
            Ok(())
        }
    }

    #[async_trait]
    impl ProcessBackend for MockBackend {
        type Child = MockChild;

        async fn spawn(&self, spec: ChildSpec) -> io::Result<Self::Child> {
            let index = self.count.fetch_add(1, Ordering::Relaxed);
            let engine = spec.stdio == StdioMode::Null;
            self.spawned.lock().expect("spawns").push(spec);
            let (name, outcome) = match (self.scenario, index, engine) {
                (Scenario::EngineCrash, 0, true) => {
                    ("engine-1", Some(ExitStatus::from_raw(1 << 8)))
                }
                (Scenario::EngineCrash, 1, false) => ("tui-1", None),
                (Scenario::EngineCrash, 2, true) => ("engine-2", None),
                (Scenario::EngineCrash, 3, false) | (Scenario::TuiCrash, 2, false) => {
                    ("tui-2", Some(ExitStatus::from_raw(0)))
                }
                (Scenario::TuiCrash, 0, true) => ("engine-1", None),
                (Scenario::TuiCrash, 1, false) => ("tui-1", Some(ExitStatus::from_raw(1 << 8))),
                _ => return Err(io::Error::other("unexpected mock spawn")),
            };
            Ok(MockChild {
                name,
                outcome,
                terminated: AtomicBool::new(false),
                lifecycle: Arc::clone(&self.lifecycle),
            })
        }

        async fn sleep(&self, _duration: Duration) {
            self.lifecycle
                .lock()
                .expect("lifecycle")
                .push("backoff".to_owned());
        }
    }

    async fn run_scenario(scenario: Scenario) -> (Vec<ChildSpec>, Vec<String>) {
        let spawned = Arc::new(Mutex::new(Vec::new()));
        let lifecycle = Arc::new(Mutex::new(Vec::new()));
        let backend = MockBackend {
            scenario,
            spawned: Arc::clone(&spawned),
            lifecycle: Arc::clone(&lifecycle),
            count: AtomicUsize::new(0),
        };
        Supervisor::new(fixture_config(), backend, ResumeHandoff::default())
            .expect("supervisor")
            .run()
            .await
            .expect("supervisor run");
        let specs = spawned.lock().expect("spawns").clone();
        let events = lifecycle.lock().expect("lifecycle").clone();
        (specs, events)
    }

    #[tokio::test]
    async fn engine_crash_reaps_tui_restarts_both_and_normal_tui_exit_reaps_engine() {
        let (specs, lifecycle) = run_scenario(Scenario::EngineCrash).await;
        assert_eq!(specs.len(), 4);
        assert!(lifecycle.contains(&"signal:tui-1:Terminate".to_owned()));
        assert!(lifecycle.contains(&"wait:tui-1".to_owned()));
        assert!(lifecycle.contains(&"signal:engine-2:Terminate".to_owned()));
        assert!(lifecycle.contains(&"wait:engine-2".to_owned()));
    }

    #[tokio::test]
    async fn tui_crash_restarts_with_same_connection_environment_and_reaps_engine_on_exit() {
        let (specs, lifecycle) = run_scenario(Scenario::TuiCrash).await;
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[1].env, specs[2].env);
        assert!(lifecycle.contains(&"signal:engine-1:Terminate".to_owned()));
        assert!(lifecycle.contains(&"wait:engine-1".to_owned()));
    }
}
