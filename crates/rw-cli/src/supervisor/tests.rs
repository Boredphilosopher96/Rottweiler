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
        last_seen_file: PathBuf::from("/private/run/last-seen"),
        fork_operation_directory: PathBuf::from("/private/control/pending-forks"),
        session_id: "session-1".to_owned(),
        tui_keybindings: None,
        tui_theme: "kennel-dark".to_owned(),
        permission_mode: Some(crate::PermissionMode::Strict),
        max_turns: 32,
        model: None,
        additional_workspaces: Vec::new(),
        dangerously_trust: false,
        in_memory_replay_script: None,
        record_script_delay_ms: 0,
        shell_target: None,
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
    }
    assert!(engine_spec(&config).new_process_group);
    assert!(!tui_spec(&config, None).new_process_group);
    let tui = tui_spec(&config, Some(SequenceId(41)));
    assert_eq!(
        tui.env.get(&OsString::from(LAST_SEEN_ENV)),
        Some(&OsString::from("41"))
    );
    assert_eq!(
        tui.env.get(&OsString::from(LAST_SEEN_FILE_ENV)),
        Some(&OsString::from("/private/run/last-seen"))
    );
    assert_eq!(
        tui.env.get(&OsString::from(TUI_RECYCLE_STATE_FILE_ENV)),
        Some(&OsString::from("/private/run/tui-recycle-state.json"))
    );
    assert_eq!(
        tui.env.get(&OsString::from(FORK_OPERATION_DIRECTORY_ENV)),
        Some(&OsString::from("/private/control/pending-forks"))
    );
    assert_eq!(
        engine_spec(&config).args,
        ["serve", "--max-turns", "32", "--permission-mode", "strict",].map(OsString::from)
    );
    assert_eq!(engine_spec(&config).stdio, StdioMode::CaptureStderr);
}

#[test]
fn keybindings_and_theme_are_forwarded_only_to_the_tui() {
    let mut config = fixture_config();
    config.tui_keybindings = Some("preset = 'vim'".to_owned());
    config.tui_theme = "daylight".to_owned();

    let key = OsString::from(TUI_KEYBINDINGS_ENV);
    assert_eq!(
        tui_spec(&config, None).env.get(&key),
        Some(&OsString::from("preset = 'vim'"))
    );
    assert!(!engine_spec(&config).env.contains_key(&key));
    let theme_key = OsString::from(TUI_THEME_ENV);
    assert_eq!(
        tui_spec(&config, None).env.get(&theme_key),
        Some(&OsString::from("daylight"))
    );
    assert!(!engine_spec(&config).env.contains_key(&theme_key));

    let engine = command_from_spec(&engine_spec(&config));
    let engine_value = engine
        .as_std()
        .get_envs()
        .find(|(name, _)| *name == key.as_os_str())
        .map(|(_, value)| value);
    assert_eq!(engine_value, Some(None));
    let tui = command_from_spec(&tui_spec(&config, None));
    let tui_value = tui
        .as_std()
        .get_envs()
        .find(|(name, _)| *name == key.as_os_str())
        .map(|(_, value)| value);
    assert_eq!(
        tui_value,
        Some(Some(std::ffi::OsStr::new("preset = 'vim'")))
    );
}

#[test]
fn engine_restart_preserves_added_roots_and_explicit_trust_override() {
    let mut config = fixture_config();
    config.additional_workspaces = vec![PathBuf::from("/work/second")];
    config.dangerously_trust = true;
    let spec = engine_spec(&config);
    assert!(
        spec.args
            .windows(2)
            .any(|pair| { pair == [OsString::from("--add-dir"), OsString::from("/work/second")] })
    );
    assert!(
        spec.args
            .iter()
            .any(|argument| argument == "--dangerously-trust")
    );
}

#[test]
fn engine_restart_preserves_hidden_deterministic_replay_configuration() {
    let mut config = fixture_config();
    config.in_memory_replay_script = Some(PathBuf::from("/private/fixture/soak.json"));
    config.record_script_delay_ms = 17;

    let spec = engine_spec(&config);
    assert!(spec.args.windows(2).any(|pair| {
        pair == [
            OsString::from("--in-memory-replay-script"),
            OsString::from("/private/fixture/soak.json"),
        ]
    }));
    assert!(spec.args.windows(2).any(|pair| {
        pair == [
            OsString::from("--record-script-delay-ms"),
            OsString::from("17"),
        ]
    }));
    assert!(
        !tui_spec(&config, None)
            .args
            .iter()
            .any(|argument| argument == "--in-memory-replay-script")
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
    EngineCleanExit,
    TuiCrash,
    ShutdownSignal,
    StartupSignal,
    ReadinessFailure,
    EngineStartupFailure,
    EngineWaitError,
    TuiRestartBudget,
    TuiRecycle,
}

struct MockBackend {
    scenario: Scenario,
    spawned: Arc<Mutex<Vec<ChildSpec>>>,
    lifecycle: Arc<Mutex<Vec<String>>>,
    ready: Arc<AtomicBool>,
    count: AtomicUsize,
}

struct MockChild {
    name: &'static str,
    outcome: Option<ExitStatus>,
    exit_after_ready: bool,
    ready: Arc<AtomicBool>,
    wait_error_once: AtomicBool,
    terminated: AtomicBool,
    lifecycle: Arc<Mutex<Vec<String>>>,
    stderr_tail: Option<String>,
}

#[async_trait]
impl ManagedChild for MockChild {
    async fn wait(&mut self) -> io::Result<ExitStatus> {
        while self.exit_after_ready && !self.ready.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        if self.wait_error_once.swap(false, Ordering::AcqRel) {
            self.lifecycle
                .lock()
                .expect("lifecycle")
                .push(format!("wait-error:{}", self.name));
            return Err(io::Error::other("injected child wait failure"));
        }
        while self.outcome.is_none() && !self.terminated.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        self.lifecycle
            .lock()
            .expect("lifecycle")
            .push(format!("wait:{}", self.name));
        Ok(self.outcome.unwrap_or_else(|| ExitStatus::from_raw(0)))
    }

    async fn stderr_tail(&mut self) -> io::Result<Option<String>> {
        Ok(self.stderr_tail.take())
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

struct IgnoringTermChild {
    killed: AtomicBool,
    signals: Arc<Mutex<Vec<ProcessSignal>>>,
}

#[async_trait]
impl ManagedChild for IgnoringTermChild {
    async fn wait(&mut self) -> io::Result<ExitStatus> {
        while !self.killed.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        Ok(ExitStatus::from_raw(9))
    }

    fn signal_group(&self, signal: ProcessSignal) -> io::Result<()> {
        self.signals.lock().expect("signals").push(signal);
        if signal == ProcessSignal::Kill {
            self.killed.store(true, Ordering::Release);
        }
        Ok(())
    }
}

#[async_trait]
impl ProcessBackend for MockBackend {
    type Child = MockChild;

    async fn spawn(&self, spec: ChildSpec) -> io::Result<Self::Child> {
        let index = self.count.fetch_add(1, Ordering::Relaxed);
        let engine = spec.args.iter().any(|argument| argument == "serve");
        self.spawned.lock().expect("spawns").push(spec);
        self.lifecycle
            .lock()
            .expect("lifecycle")
            .push(if engine { "spawn:engine" } else { "spawn:tui" }.to_owned());
        let (name, outcome, wait_error_once) = match (self.scenario, index, engine) {
            (Scenario::EngineCleanExit, 0, true) => {
                ("engine-1", Some(ExitStatus::from_raw(0)), false)
            }
            (Scenario::EngineCrash | Scenario::EngineStartupFailure, 0, true) => {
                ("engine-1", Some(ExitStatus::from_raw(1 << 8)), false)
            }
            (
                Scenario::EngineCrash
                | Scenario::EngineCleanExit
                | Scenario::ShutdownSignal
                | Scenario::ReadinessFailure
                | Scenario::EngineWaitError,
                1,
                false,
            ) => ("tui-1", None, false),
            (Scenario::EngineCrash, 2, true) => ("engine-2", None, false),
            (Scenario::EngineCrash, 3, false) | (Scenario::TuiCrash, 2, false) => {
                ("tui-2", Some(ExitStatus::from_raw(0)), false)
            }
            (
                Scenario::TuiCrash
                | Scenario::ShutdownSignal
                | Scenario::ReadinessFailure
                | Scenario::TuiRestartBudget
                | Scenario::TuiRecycle,
                0,
                true,
            ) => ("engine-1", None, false),
            (Scenario::TuiCrash | Scenario::TuiRestartBudget, 1, false) => {
                ("tui-1", Some(ExitStatus::from_raw(1 << 8)), false)
            }
            (Scenario::EngineWaitError, 0, true) => ("engine-1", None, true),
            (Scenario::TuiRestartBudget, 2, false) => {
                ("tui-2", Some(ExitStatus::from_raw(1 << 8)), false)
            }
            (Scenario::TuiRecycle, 1..=3, false) => (
                match index {
                    1 => "tui-1",
                    2 => "tui-2",
                    _ => "tui-3",
                },
                Some(ExitStatus::from_raw(TUI_RECYCLE_EXIT_CODE << 8)),
                false,
            ),
            (Scenario::TuiRecycle, 4, false) => ("tui-4", Some(ExitStatus::from_raw(0)), false),
            _ => return Err(io::Error::other("unexpected mock spawn")),
        };
        Ok(MockChild {
            name,
            outcome,
            exit_after_ready: matches!(
                self.scenario,
                Scenario::EngineCrash | Scenario::EngineCleanExit | Scenario::EngineWaitError
            ) && index == 0
                && engine,
            ready: Arc::clone(&self.ready),
            wait_error_once: AtomicBool::new(wait_error_once),
            terminated: AtomicBool::new(false),
            lifecycle: Arc::clone(&self.lifecycle),
            stderr_tail: matches!(self.scenario, Scenario::EngineStartupFailure)
                .then(|| "workspace authorization failed".to_owned()),
        })
    }

    async fn sleep(&self, _duration: Duration) {
        self.lifecycle
            .lock()
            .expect("lifecycle")
            .push("backoff".to_owned());
    }

    async fn wait_ready(&self, _socket: &Path, _token_file: &Path) -> io::Result<()> {
        self.lifecycle
            .lock()
            .expect("lifecycle")
            .push("ready:engine".to_owned());
        if matches!(self.scenario, Scenario::ReadinessFailure) {
            return Err(io::Error::other("injected readiness failure"));
        }
        if matches!(self.scenario, Scenario::EngineStartupFailure) {
            std::future::pending::<()>().await;
        }
        self.ready.store(true, Ordering::Release);
        Ok(())
    }

    async fn wait_shutdown_signal(&self) -> io::Result<()> {
        if matches!(self.scenario, Scenario::StartupSignal) {
            return Ok(());
        }
        if matches!(self.scenario, Scenario::ShutdownSignal) {
            while self.count.load(Ordering::Acquire) < 2 {
                tokio::task::yield_now().await;
            }
            return Ok(());
        }
        std::future::pending::<()>().await;
        Ok(())
    }
}

async fn run_scenario_with_config(
    scenario: Scenario,
    config: SupervisorConfig,
) -> (Result<(), SupervisorError>, Vec<ChildSpec>, Vec<String>) {
    let spawned = Arc::new(Mutex::new(Vec::new()));
    let lifecycle = Arc::new(Mutex::new(Vec::new()));
    let backend = MockBackend {
        scenario,
        spawned: Arc::clone(&spawned),
        lifecycle: Arc::clone(&lifecycle),
        ready: Arc::new(AtomicBool::new(false)),
        count: AtomicUsize::new(0),
    };
    let result = Supervisor::new(config, backend, ResumeHandoff::default())
        .expect("supervisor")
        .run()
        .await;
    let specs = spawned.lock().expect("spawns").clone();
    let events = lifecycle.lock().expect("lifecycle").clone();
    (result, specs, events)
}

async fn run_scenario_with_pending_broker(
    scenario: Scenario,
) -> (Result<(), SupervisorError>, Vec<ChildSpec>, Vec<String>) {
    let spawned = Arc::new(Mutex::new(Vec::new()));
    let lifecycle = Arc::new(Mutex::new(Vec::new()));
    let backend = MockBackend {
        scenario,
        spawned: Arc::clone(&spawned),
        lifecycle: Arc::clone(&lifecycle),
        ready: Arc::new(AtomicBool::new(false)),
        count: AtomicUsize::new(0),
    };
    let supervisor =
        Supervisor::new(fixture_config(), backend, ResumeHandoff::default()).expect("supervisor");
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        supervisor.run_with_shell_broker(|| {
            let (ready, ready_rx) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                let _keep_ready_pending = ready;
                std::future::pending::<()>().await;
                Ok(())
            });
            (task, Some(ready_rx))
        }),
    )
    .await
    .expect("child lifecycle must not be hidden behind broker readiness");
    let specs = spawned.lock().expect("spawns").clone();
    let events = lifecycle.lock().expect("lifecycle").clone();
    (result, specs, events)
}

async fn run_scenario(scenario: Scenario) -> (Vec<ChildSpec>, Vec<String>) {
    let (result, specs, events) = run_scenario_with_config(scenario, fixture_config()).await;
    result.expect("supervisor run");
    (specs, events)
}

#[test]
fn ctrl_c_exit_status_is_an_intentional_tui_close() {
    assert!(tui_exit_is_user_close(ExitStatus::from_raw(0)));
    assert!(tui_exit_is_user_close(ExitStatus::from_raw(130 << 8)));
    assert!(tui_exit_is_user_close(ExitStatus::from_raw(2)));
    assert!(!tui_exit_is_user_close(ExitStatus::from_raw(1 << 8)));
    assert!(tui_exit_is_recycle(ExitStatus::from_raw(
        TUI_RECYCLE_EXIT_CODE << 8
    )));
}

#[tokio::test]
async fn planned_tui_recycles_do_not_consume_the_crash_budget() {
    let mut config = fixture_config();
    config.restart_policy.max_consecutive_failures = 1;
    let (result, specs, lifecycle) = run_scenario_with_config(Scenario::TuiRecycle, config).await;
    result.expect("planned recycles must remain restartable");
    assert_eq!(specs.len(), 5);
    assert!(!lifecycle.iter().any(|event| event == "backoff"));
    assert!(lifecycle.contains(&"signal:engine-1:Terminate".to_owned()));
}

#[tokio::test]
async fn engine_crash_reaps_tui_restarts_both_and_normal_tui_exit_reaps_engine() {
    let (specs, lifecycle) = run_scenario(Scenario::EngineCrash).await;
    assert_eq!(specs.len(), 4);
    assert!(
        !specs[0]
            .args
            .iter()
            .any(|argument| argument == WAIT_FOR_EXECUTION_LEASE_ARG)
    );
    assert!(
        specs[2]
            .args
            .iter()
            .any(|argument| argument == WAIT_FOR_EXECUTION_LEASE_ARG)
    );
    assert_eq!(
        &lifecycle[..3],
        ["spawn:engine", "ready:engine", "spawn:tui"]
    );
    assert!(lifecycle.contains(&"signal:tui-1:Terminate".to_owned()));
    assert!(lifecycle.contains(&"wait:tui-1".to_owned()));
    assert!(lifecycle.contains(&"signal:engine-2:Terminate".to_owned()));
    assert!(lifecycle.contains(&"wait:engine-2".to_owned()));
}

#[tokio::test]
async fn engine_exit_during_broker_readiness_is_reaped_and_restarted() {
    let (result, specs, lifecycle) = run_scenario_with_pending_broker(Scenario::EngineCrash).await;
    result.expect("engine crash must recover while broker readiness is pending");
    assert_eq!(specs.len(), 4);
    assert!(lifecycle.contains(&"wait:engine-1".to_owned()));
    assert!(lifecycle.contains(&"signal:tui-1:Terminate".to_owned()));
    assert!(lifecycle.contains(&"wait:tui-1".to_owned()));
    assert!(lifecycle.contains(&"spawn:engine".to_owned()));
    assert!(lifecycle.contains(&"spawn:tui".to_owned()));
}

#[tokio::test]
async fn clean_engine_exit_reaps_tui_without_restarting_the_app() {
    let (specs, lifecycle) = run_scenario(Scenario::EngineCleanExit).await;
    assert_eq!(specs.len(), 2);
    assert_eq!(
        &lifecycle[..3],
        ["spawn:engine", "ready:engine", "spawn:tui"]
    );
    assert!(lifecycle.contains(&"wait:engine-1".to_owned()));
    assert!(lifecycle.contains(&"signal:tui-1:Terminate".to_owned()));
    assert!(lifecycle.contains(&"wait:tui-1".to_owned()));
    assert!(!lifecycle.contains(&"backoff".to_owned()));
}

#[tokio::test]
async fn tui_crash_restarts_with_same_connection_environment_and_reaps_engine_on_exit() {
    let (specs, lifecycle) = run_scenario(Scenario::TuiCrash).await;
    assert_eq!(specs.len(), 3);
    assert_eq!(
        &lifecycle[..3],
        ["spawn:engine", "ready:engine", "spawn:tui"]
    );
    assert_eq!(specs[1].env, specs[2].env);
    assert!(lifecycle.contains(&"signal:engine-1:Terminate".to_owned()));
    assert!(lifecycle.contains(&"wait:engine-1".to_owned()));
}

#[tokio::test]
async fn tui_exit_during_broker_readiness_is_reaped_and_restarted() {
    let (result, specs, lifecycle) = run_scenario_with_pending_broker(Scenario::TuiCrash).await;
    result.expect("TUI crash must recover while broker readiness is pending");
    assert_eq!(specs.len(), 3);
    assert!(lifecycle.contains(&"wait:tui-1".to_owned()));
    assert_eq!(specs[1].env, specs[2].env);
    assert!(lifecycle.contains(&"signal:engine-1:Terminate".to_owned()));
    assert!(lifecycle.contains(&"wait:engine-1".to_owned()));
}

#[tokio::test]
async fn detach_keeps_the_engine_group_alive_after_normal_tui_exit() {
    let mut config = fixture_config();
    config.detach = true;
    let (result, specs, lifecycle) = run_scenario_with_config(Scenario::TuiCrash, config).await;
    result.expect("detached supervisor run");
    assert_eq!(specs.len(), 3);
    assert!(
        !lifecycle
            .iter()
            .any(|event| event == "signal:engine-1:Terminate")
    );
}

#[tokio::test]
async fn shutdown_signal_reaps_tui_and_independent_engine_group() {
    let (specs, lifecycle) = run_scenario(Scenario::ShutdownSignal).await;
    assert_eq!(specs.len(), 2);
    assert!(lifecycle.contains(&"signal:tui-1:Terminate".to_owned()));
    assert!(lifecycle.contains(&"wait:tui-1".to_owned()));
    assert!(lifecycle.contains(&"signal:engine-1:Terminate".to_owned()));
    assert!(lifecycle.contains(&"wait:engine-1".to_owned()));
}

#[tokio::test]
async fn shutdown_during_broker_readiness_reaps_both_process_groups() {
    let (result, specs, lifecycle) =
        run_scenario_with_pending_broker(Scenario::ShutdownSignal).await;
    result.expect("shutdown must remain responsive while broker readiness is pending");
    assert_eq!(specs.len(), 2);
    assert!(lifecycle.contains(&"signal:tui-1:Terminate".to_owned()));
    assert!(lifecycle.contains(&"wait:tui-1".to_owned()));
    assert!(lifecycle.contains(&"signal:engine-1:Terminate".to_owned()));
    assert!(lifecycle.contains(&"wait:engine-1".to_owned()));
}

#[tokio::test]
async fn startup_signal_is_armed_before_the_first_child_spawn() {
    let (specs, lifecycle) = run_scenario(Scenario::StartupSignal).await;
    assert!(specs.is_empty());
    assert!(lifecycle.is_empty());
}

#[tokio::test]
async fn readiness_failure_cleans_up_every_started_child() {
    let (result, specs, lifecycle) =
        run_scenario_with_config(Scenario::ReadinessFailure, fixture_config()).await;
    assert!(matches!(result, Err(SupervisorError::Readiness(_))));
    assert_eq!(specs.len(), 1);
    assert!(!lifecycle.iter().any(|event| event.contains("tui")));
    assert!(lifecycle.contains(&"signal:engine-1:Terminate".to_owned()));
    assert!(lifecycle.contains(&"wait:engine-1".to_owned()));
}

#[tokio::test]
async fn engine_startup_exit_surfaces_immediately_before_tui_spawn() {
    let (result, specs, lifecycle) =
        run_scenario_with_config(Scenario::EngineStartupFailure, fixture_config()).await;
    let error = result.expect_err("startup must fail").to_string();
    assert!(error.contains("engine exited before authenticated readiness"));
    assert!(error.contains("workspace authorization failed"));
    assert_eq!(specs.len(), 1);
    assert!(!lifecycle.iter().any(|event| event.contains("tui")));
    assert!(lifecycle.contains(&"wait:engine-1".to_owned()));
    assert!(!lifecycle.contains(&"signal:engine-1:Terminate".to_owned()));
}

#[tokio::test]
async fn captured_stderr_is_drained_concurrently_bounded_and_tail_biased() {
    let spec = ChildSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec![
            OsString::from("-c"),
            OsString::from(
                "i=0; while [ $i -lt 20000 ]; do printf x >&2; i=$((i+1)); done; printf TAIL-SENTINEL >&2",
            ),
        ],
        env: BTreeMap::new(),
        stdio: StdioMode::CaptureStderr,
        new_process_group: true,
    };
    let mut child = TokioProcessBackend.spawn(spec).await.expect("spawn");
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("stderr pipe must not deadlock")
        .expect("wait");
    assert!(status.success());
    let tail = child
        .stderr_tail()
        .await
        .expect("read stderr")
        .expect("captured stderr");
    assert!(tail.len() <= ENGINE_STDERR_TAIL_BYTES);
    assert!(tail.ends_with("TAIL-SENTINEL"));
}

#[test]
fn engine_stderr_diagnostics_strip_control_bytes_and_bootstrap_token() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let token_file = directory.path().join("auth.token");
    std::fs::write(&token_file, "top-secret-token\n").expect("token fixture");
    let output = sanitize_engine_stderr(
        "failure \u{1b}[31mtop-secret-token\u{7} details\n",
        &token_file,
    );
    assert!(!output.contains("top-secret-token"));
    assert!(!output.contains('\u{1b}'));
    assert!(!output.contains('\u{7}'));
    assert!(output.contains("[REDACTED]"));
}

#[tokio::test]
async fn child_wait_error_still_cleans_up_both_process_groups() {
    let (result, specs, lifecycle) =
        run_scenario_with_config(Scenario::EngineWaitError, fixture_config()).await;
    assert!(matches!(
        result,
        Err(SupervisorError::Wait {
            component: "engine",
            ..
        })
    ));
    assert_eq!(specs.len(), 2);
    assert!(lifecycle.contains(&"wait-error:engine-1".to_owned()));
    assert!(lifecycle.contains(&"signal:tui-1:Terminate".to_owned()));
    assert!(lifecycle.contains(&"signal:engine-1:Terminate".to_owned()));
    assert!(lifecycle.contains(&"wait:engine-1".to_owned()));
}

#[tokio::test]
async fn restart_budget_exhaustion_reaps_the_surviving_engine() {
    let mut config = fixture_config();
    config.restart_policy.max_consecutive_failures = 1;
    let (result, specs, lifecycle) =
        run_scenario_with_config(Scenario::TuiRestartBudget, config).await;
    assert!(matches!(
        result,
        Err(SupervisorError::RestartBudgetExhausted)
    ));
    assert_eq!(specs.len(), 3);
    assert!(lifecycle.contains(&"wait:tui-1".to_owned()));
    assert!(lifecycle.contains(&"wait:tui-2".to_owned()));
    assert!(lifecycle.contains(&"signal:engine-1:Terminate".to_owned()));
    assert!(lifecycle.contains(&"wait:engine-1".to_owned()));
}

#[tokio::test]
async fn wedged_child_is_killed_after_bounded_shutdown_grace() {
    let signals = Arc::new(Mutex::new(Vec::new()));
    let mut child = IgnoringTermChild {
        killed: AtomicBool::new(false),
        signals: Arc::clone(&signals),
    };

    terminate_and_reap_with_grace(&mut child, "fixture", Duration::from_millis(1))
        .await
        .expect("bounded reap");
    assert_eq!(
        *signals.lock().expect("signals"),
        [ProcessSignal::Terminate, ProcessSignal::Kill]
    );
}
