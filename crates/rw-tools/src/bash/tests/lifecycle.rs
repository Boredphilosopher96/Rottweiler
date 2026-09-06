use super::*;

#[tokio::test]
async fn panicked_foreground_task_does_not_leave_an_immortal_registration() {
    let root = tempdir().expect("workspace");
    let context = ToolContext::new(root.path()).expect("context");
    let tool = BashTool::new(Arc::new(PanickingExecutor), ToolLimits::default());
    let result = tool
        .execute(&context, json!({"command":"printf test"}))
        .await;
    assert!(matches!(result, Err(ToolError::Command(_))));
    tokio::time::timeout(Duration::from_millis(100), tool.settle_effects())
        .await
        .expect("panic settlement must terminate")
        .expect("effects settled");
    assert!(tool.foreground.calls.lock().expect("calls").is_empty());
    tokio::time::timeout(
        Duration::from_millis(100),
        tool.execute(&context, json!({"command":"printf next"})),
    )
    .await
    .expect("next invocation must not hang")
    .expect_err("executor still panics");
}

#[cfg(unix)]
#[tokio::test]
async fn foreground_panic_waits_for_native_settlement_before_next_mutation() {
    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("workspace");
    let context = ToolContext::new(root.path()).expect("context");
    let executor = Arc::new(PanicDuringNative::default());
    let tool = Arc::new(BashTool::new(executor.clone(), ToolLimits::default()));
    let mut invocation = {
        let tool = Arc::clone(&tool);
        tokio::spawn(async move {
            tool.execute(&context, json!({"command":"(while :; do printf child >> child-writes; /bin/sleep 0.01; done) & while :; do printf parent >> parent-writes; /bin/sleep 0.01; done"})).await
        })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while !root.path().join("parent-writes").exists()
            || !root.path().join("child-writes").exists()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("native children started");
    executor.panic_now.notify_one();
    tokio::time::timeout(Duration::from_secs(3), executor.settling.notified())
        .await
        .expect("physical settlement entered");
    let premature = tokio::time::timeout(Duration::from_millis(50), &mut invocation)
        .await
        .is_ok();
    invocation.abort();
    let _ = invocation.await;
    let premature_barrier = tokio::time::timeout(Duration::from_millis(50), tool.settle_effects())
        .await
        .is_ok();
    executor.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), tool.settle_effects())
        .await
        .expect("native cleanup settles after caller drop")
        .expect("effects settled");
    assert!(!premature, "task panic was treated as physical completion");
    assert!(
        !premature_barrier,
        "caller drop bypassed executor settlement"
    );
    assert!(tool.foreground.calls.lock().expect("calls").is_empty());
    for file in ["parent-writes", "child-writes"] {
        std::fs::write(root.path().join(file), b"next mutation").expect("write after settlement");
    }
    tokio::time::sleep(Duration::from_millis(80)).await;
    for file in ["parent-writes", "child-writes"] {
        assert_eq!(
            std::fs::read(root.path().join(file)).expect("read after settlement"),
            b"next mutation"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn foreground_cleanup_and_recording_survive_caller_drop() {
    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("workspace");
    let fixtures = tempdir().expect("recordings");
    let writes = root.path().join("writes");
    let cancellation = CancellationToken::default();
    let context = ToolContext::new(root.path())
        .expect("context")
        .with_cancellation(cancellation.clone());
    let cleanup = Arc::new(DelayedNativeCleanup::default());
    let recording = RecordingCommandExecutor::new(cleanup.clone(), fixtures.path(), root.path())
        .expect("recording executor");
    let tool = Arc::new(BashTool::new(Arc::new(recording), ToolLimits::default()));
    let mut task = {
        let tool = Arc::clone(&tool);
        tokio::spawn(async move {
            tool.execute(
                &context,
                json!({"command": "while :; do printf running >> writes; /bin/sleep 0.01; done"}),
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while !writes.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("native command started");
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(3), cleanup.started.notified())
        .await
        .expect("cleanup started");
    assert!(
        tokio::time::timeout(Duration::from_secs(2), &mut task)
            .await
            .is_err()
    );
    task.abort();
    assert!(task.await.expect_err("caller dropped").is_cancelled());
    let premature = tokio::time::timeout(Duration::from_millis(50), tool.settle_effects())
        .await
        .is_ok();
    cleanup.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), tool.settle_effects())
        .await
        .expect("settlement")
        .expect("effects settled");
    assert!(
        !premature,
        "Bash released its settlement barrier while native cleanup was still pending"
    );
    assert!(cleanup.finished.load(std::sync::atomic::Ordering::Acquire));
    let occurrences =
        load_command_occurrences(&fixtures.path().join(COMMAND_REPLAY_FILE)).expect("recording");
    assert_eq!(occurrences.len(), 1);
    assert!(matches!(
        occurrences[0].terminal,
        RecordedCommandTerminal::Cancelled {}
    ));
    std::fs::write(&writes, b"next mutation").expect("conflicting write");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        std::fs::read(writes).expect("settled file"),
        b"next mutation"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dropped_foreground_call_cancels_native_parent_and_descendant_before_settlement() {
    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("workspace");
    let lease_path = root.path().join("execution.lock");
    let executor = TokioCommandExecutor::with_execution_lease(Arc::new(
        ExecutionLease::acquire(&lease_path).expect("execution lease"),
    ));
    let tool = Arc::new(BashTool::new(Arc::new(executor), ToolLimits::default()));
    let context = ToolContext::new(root.path()).expect("context");
    let task = {
        let tool = Arc::clone(&tool);
        tokio::spawn(async move {
            tool.execute(&context, json!({"command": "(while :; do printf child >> child-writes; /bin/sleep 0.01; done) & while :; do printf parent >> parent-writes; /bin/sleep 0.01; done"})).await
        })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while !root.path().join("parent-writes").exists()
            || !root.path().join("child-writes").exists()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("native parent and child started");
    task.abort();
    assert!(task.await.expect_err("caller dropped").is_cancelled());
    tokio::time::timeout(Duration::from_secs(3), tool.settle_effects())
        .await
        .expect("native settlement")
        .expect("effects settled");
    for file in ["parent-writes", "child-writes"] {
        std::fs::write(root.path().join(file), b"next mutation").expect("conflicting write");
    }
    tokio::time::sleep(Duration::from_millis(80)).await;
    for file in ["parent-writes", "child-writes"] {
        assert_eq!(
            std::fs::read(root.path().join(file)).expect("settled file"),
            b"next mutation"
        );
    }
    drop(tool);
    let _recovered = ExecutionLease::acquire_for(&lease_path, Duration::from_secs(1))
        .expect("watchdog released execution lease after settlement");
}

#[tokio::test]
async fn injected_commands_observe_cancellation() {
    let root = tempdir().expect("temp directory");
    let cancellation = CancellationToken::default();
    let context = ToolContext::new(root.path())
        .expect("context")
        .with_cancellation(cancellation.clone());
    let tool = BashTool::new(Arc::new(BlockingExecutor), ToolLimits::default());
    let task =
        tokio::spawn(async move { tool.execute(&context, json!({"command": "ignored"})).await });
    tokio::task::yield_now().await;
    cancellation.cancel();
    assert!(matches!(
        task.await.expect("join"),
        Err(ToolError::Cancelled)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_before_launch_gate_never_releases_the_command() {
    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("temp directory");
    let sentinel = root.path().join("must-not-run");
    let command = format!(
        "printf launched > {}",
        shell_words::quote(sentinel.to_string_lossy().as_ref())
    );
    let cancellation = CancellationToken::default();
    let run_cancellation = cancellation.clone();
    let hook = Arc::new(LaunchGateTestHook::default());
    let run_hook = hook.clone();
    let executor = TokioCommandExecutor::default().with_launch_gate_hook(run_hook);
    let run = tokio::spawn(async move {
        executor
            .run(
                CommandRequest {
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
                    command,
                    cwd: root.path().to_path_buf(),
                    env: BTreeMap::new(),
                },
                run_cancellation,
                Arc::new(crate::NoopOutputSink),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), hook.wait_until_reached())
        .await
        .expect("launch-gate barrier timeout");
    let child = hook.child_id().expect("guarded command pid");
    cancellation.cancel();
    hook.release();
    let outcome = tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .expect("bounded pre-launch cancellation")
        .expect("executor join");
    assert!(
        matches!(outcome, Err(ToolError::Cancelled)),
        "unexpected pre-launch cancellation outcome: {outcome:?}"
    );
    assert!(!sentinel.exists(), "cancelled command was released");
    assert!(
        matches!(
            rustix::process::test_kill_process_group(child),
            Err(rustix::io::Errno::SRCH)
        ),
        "cancelled guarded command group survived"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn process_group_cancellation_kills_a_descendant_holding_the_pipes() {
    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("temp directory");
    let descendant_pid_file = root.path().join("descendant.pid");
    let command = format!(
        concat!(
            "sleep 30 & descendant=$!; ",
            "printf '%s\\n' \"$descendant\" > {}; ",
            "printf 'descendant-ready\\n'; wait"
        ),
        shell_words::quote(descendant_pid_file.to_string_lossy().as_ref())
    );
    let cancellation = CancellationToken::default();
    let run_cancellation = cancellation.clone();
    let executor = TokioCommandExecutor::default();
    let sink = Arc::new(RecordingSink::default());
    let run_sink = sink.clone();
    let run = tokio::spawn(async move {
        executor
            .run(
                CommandRequest {
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
                    command,
                    cwd: root.path().to_path_buf(),
                    env: BTreeMap::new(),
                },
                run_cancellation,
                run_sink,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let ready = sink
                .0
                .lock()
                .expect("recorded output")
                .iter()
                .map(|chunk| chunk.content.as_str())
                .collect::<String>()
                .contains("descendant-ready");
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("descendant readiness timeout");
    let descendant = std::fs::read_to_string(&descendant_pid_file)
        .expect("descendant pid file")
        .trim()
        .parse::<i32>()
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .expect("descendant pid");
    cancellation.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .expect("bounded cancellation")
        .expect("executor join");
    assert!(
        matches!(outcome, Err(ToolError::Cancelled)),
        "unexpected cancellation outcome: {outcome:?}"
    );
    assert!(
        matches!(
            rustix::process::test_kill_process(descendant),
            Err(rustix::io::Errno::SRCH)
        ),
        "cancelled descendant survived process-group teardown"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn real_executor_disarms_and_reaps_watchdog_on_normal_completion() {
    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("temp directory");
    let sink = Arc::new(RecordingSink::default());
    let outcome = TokioCommandExecutor::default()
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Sandboxed,
                network_domains: Vec::new(),
                command: "printf normal".to_owned(),
                cwd: root.path().to_path_buf(),
                env: BTreeMap::new(),
            },
            CancellationToken::default(),
            sink.clone(),
        )
        .await
        .expect("normal command");
    assert_eq!(outcome.exit_code, 0);
    assert!(
        sink.0
            .lock()
            .expect("recording")
            .iter()
            .any(|chunk| chunk.content.contains("normal"))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn executor_waits_for_background_group_members_before_returning() {
    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("temp directory");
    let pid_file = root.path().join("background.pid");
    let outcome = TokioCommandExecutor::default()
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Sandboxed,
                network_domains: Vec::new(),
                command: "sleep 30 & printf '%s\\n' \"$!\" > background.pid".to_owned(),
                cwd: root.path().to_path_buf(),
                env: BTreeMap::new(),
            },
            CancellationToken::default(),
            Arc::new(crate::NoopOutputSink),
        )
        .await
        .expect("command outcome");
    assert_eq!(outcome.exit_code, 0);
    let background = read_test_pid(&pid_file).await;
    assert!(
        rustix::process::test_kill_process(background).is_err(),
        "background same-group process survived executor return"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn lease_descriptor_is_not_inherited_by_user_or_unrelated_commands() {
    use std::os::unix::fs::MetadataExt as _;

    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("temp directory");
    let lease = Arc::new(
        ExecutionLease::acquire(root.path().join("execution.lock")).expect("execution lease"),
    );
    let descriptor = lease.test_watchdog_raw_fd().to_string();
    let metadata = lease.file.metadata().expect("lease metadata");
    let device = metadata.dev().to_string();
    let inode = metadata.ino().to_string();
    let executable = std::env::current_exe().expect("current test executable");
    let unrelated = std::process::Command::new(&executable)
        .arg("--exact")
        .arg("bash::tests::lease_descriptor_probe_subprocess_helper")
        .arg("--nocapture")
        .env("ROTTWEILER_LEASE_PROBE_FD", &descriptor)
        .env("ROTTWEILER_LEASE_PROBE_DEV", &device)
        .env("ROTTWEILER_LEASE_PROBE_INO", &inode)
        .status()
        .expect("unrelated descriptor probe");
    assert!(unrelated.success(), "unrelated child inherited lease fd");

    let user_probe = format!(
        "{} --exact bash::tests::lease_descriptor_probe_subprocess_helper --nocapture",
        shell_words::quote(executable.to_string_lossy().as_ref())
    );
    let outcome = TokioCommandExecutor::with_execution_lease(lease)
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Sandboxed,
                network_domains: Vec::new(),
                command: user_probe,
                cwd: root.path().to_path_buf(),
                env: BTreeMap::from([
                    ("ROTTWEILER_LEASE_PROBE_FD".to_owned(), descriptor),
                    ("ROTTWEILER_LEASE_PROBE_DEV".to_owned(), device),
                    ("ROTTWEILER_LEASE_PROBE_INO".to_owned(), inode),
                ]),
            },
            CancellationToken::default(),
            Arc::new(crate::NoopOutputSink),
        )
        .await
        .expect("user command descriptor probe");
    assert_eq!(outcome.exit_code, 0, "user command inherited lease fd");
}

#[cfg(unix)]
#[tokio::test]
async fn sigkill_of_executor_parent_kills_group_and_prevents_delayed_side_effects() {
    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("temp directory");
    let ready = root.path().join("command.pid");
    let watchdog_pid_file = root.path().join("watchdog.pid");
    let sentinel = root.path().join("sentinel");
    let executable = std::env::current_exe().expect("current test executable");
    let mut helper_command = Command::new(executable);
    helper_command
        .arg("--exact")
        .arg("bash::tests::watchdog_subprocess_helper")
        .arg("--nocapture")
        .env("ROTTWEILER_WATCHDOG_HELPER", "1")
        .env("ROTTWEILER_WATCHDOG_READY", &ready)
        .env("ROTTWEILER_WATCHDOG_SENTINEL", &sentinel)
        .env("ROTTWEILER_WATCHDOG_TEST_PID_FILE", &watchdog_pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut helper = helper_command.spawn().expect("spawn helper");
    wait_for_test_file(&mut helper, &ready).await;
    wait_for_test_file(&mut helper, &watchdog_pid_file).await;
    let command_pid = read_test_pid(&ready).await;
    let watchdog_pid = read_test_pid(&watchdog_pid_file).await;
    let helper_pid = helper
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
        .expect("helper pid");
    rustix::process::kill_process(helper_pid, rustix::process::Signal::KILL).expect("kill helper");
    tokio::time::timeout(Duration::from_secs(3), helper.wait())
        .await
        .expect("helper exit timeout")
        .expect("helper wait");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let group_gone = rustix::process::test_kill_process_group(command_pid).is_err();
        let watchdog_gone = !test_process_is_running(watchdog_pid);
        if group_gone && watchdog_gone {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "orphan process survived"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert!(!sentinel.exists(), "orphan command wrote delayed sentinel");
}

#[cfg(unix)]
#[tokio::test]
async fn watchdog_lease_blocks_resumer_until_killed_group_is_absent() {
    let _lifecycle = crate::acquire_process_lifecycle_test_gate().await;
    let root = tempdir().expect("temp directory");
    let lease_path = root.path().join("execution.lock");
    let pause = root.path().join("pause-watchdog");
    std::fs::write(&pause, b"pause").expect("watchdog pause marker");
    let ready = root.path().join("command.pid");
    let watchdog_pid_file = root.path().join("watchdog.pid");
    let sentinel = root.path().join("sentinel");
    let executable = std::env::current_exe().expect("current test executable");
    let mut helper_command = Command::new(executable);
    helper_command
        .arg("--exact")
        .arg("bash::tests::watchdog_subprocess_helper")
        .arg("--nocapture")
        .env("ROTTWEILER_WATCHDOG_HELPER", "1")
        .env("ROTTWEILER_WATCHDOG_READY", &ready)
        .env("ROTTWEILER_WATCHDOG_SENTINEL", &sentinel)
        .env("ROTTWEILER_WATCHDOG_TEST_PID_FILE", &watchdog_pid_file)
        .env("ROTTWEILER_WATCHDOG_PAUSE_FILE", &pause)
        .env("ROTTWEILER_WATCHDOG_LEASE", &lease_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut helper = helper_command.spawn().expect("spawn helper");
    wait_for_test_file(&mut helper, &ready).await;
    wait_for_test_file(&mut helper, &watchdog_pid_file).await;
    let command_pid = read_test_pid(&ready).await;
    let watchdog_pid = read_test_pid(&watchdog_pid_file).await;
    let helper_pid = helper
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
        .expect("helper pid");
    rustix::process::kill_process(helper_pid, rustix::process::Signal::KILL).expect("kill helper");
    helper.wait().await.expect("helper wait");

    let (acquired_tx, mut acquired_rx) = tokio::sync::mpsc::unbounded_channel();
    let resumer_path = lease_path.clone();
    let resumer = tokio::task::spawn_blocking(move || {
        let lease = ExecutionLease::acquire(resumer_path).expect("resumer lease");
        acquired_tx.send(lease).expect("report acquired lease");
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(200), acquired_rx.recv())
            .await
            .is_err(),
        "resumer acquired while watchdog was deliberately paused"
    );
    assert!(
        rustix::process::test_kill_process_group(command_pid).is_ok(),
        "paused watchdog killed command group too early"
    );

    std::fs::remove_file(&pause).expect("release watchdog");
    let resumed_lease = tokio::time::timeout(Duration::from_secs(3), acquired_rx.recv())
        .await
        .expect("resumer barrier timeout")
        .expect("resumer lease channel");
    assert!(
        rustix::process::test_kill_process_group(command_pid).is_err(),
        "lease released before command group disappearance"
    );
    assert!(
        !test_process_is_running(watchdog_pid),
        "lease released before watchdog exit"
    );
    drop(resumed_lease);
    resumer.await.expect("resumer task");
    assert!(!sentinel.exists(), "orphan command wrote delayed sentinel");
}

#[tokio::test]
async fn any_failed_settlement_keeps_foreground_admission_closed() {
    #[derive(Default)]
    struct FailedSettlement(std::sync::atomic::AtomicUsize);
    #[async_trait]
    impl CommandExecutor for FailedSettlement {
        async fn settle_effects(&self) -> Result<(), ToolError> {
            Err(ToolError::Command("cleanup failed".into()))
        }
        async fn run(
            &self,
            _: CommandRequest,
            _: CancellationToken,
            _: Arc<dyn ToolOutputSink>,
        ) -> Result<CommandOutcome, ToolError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(CommandOutcome { exit_code: 0 })
        }
    }
    let root = tempdir().expect("workspace");
    let context = ToolContext::new(root.path()).expect("context");
    let executor = Arc::new(FailedSettlement::default());
    let tool = BashTool::new(executor.clone(), ToolLimits::default());
    assert!(matches!(
        tool.execute(&context, json!({"command":"printf test"}))
            .await,
        Err(ToolError::EffectsUnsettled(_))
    ));
    assert!(matches!(
        tokio::time::timeout(Duration::from_millis(100), tool.settle_effects())
            .await
            .expect("failed owner proof returns promptly"),
        Err(ToolError::EffectsUnsettled(_))
    ));
    let next_context = ToolContext::new(root.path()).expect("independent next invocation");
    assert!(matches!(
        tool.execute(&next_context, json!({"command":"printf next"}))
            .await,
        Err(ToolError::EffectsUnsettled(_))
    ));
    assert_eq!(executor.0.load(std::sync::atomic::Ordering::Relaxed), 1);
}
