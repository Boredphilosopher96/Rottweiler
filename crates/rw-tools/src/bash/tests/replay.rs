use super::*;

#[test]
fn command_recordings_require_complete_explicit_request_contracts() {
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let occurrence = json!({
        "request": {
            "command": "printf explicit",
            "workspace_relative_cwd": ".",
            "env": {},
            "network_domains": [],
            "sandbox": "sandboxed"
        },
        "output": [],
        "terminal": { "type": "cancelled" }
    });
    let path = fixtures.path().join(COMMAND_REPLAY_FILE);
    let write = |value: &serde_json::Value| {
        std::fs::write(&path, serde_json::to_vec(&json!([value])).expect("encode"))
            .expect("write fixture");
    };
    write(&occurrence);
    ReplayCommandExecutor::load(fixtures.path(), workspace.path()).expect("complete contract");
    for field in [
        "command",
        "workspace_relative_cwd",
        "env",
        "network_domains",
        "sandbox",
    ] {
        let mut incomplete = occurrence.clone();
        incomplete["request"]
            .as_object_mut()
            .expect("request")
            .remove(field);
        write(&incomplete);
        assert!(
            ReplayCommandExecutor::load(fixtures.path(), workspace.path()).is_err(),
            "{field}"
        );
    }
    for scope in [None, Some("request"), Some("terminal")] {
        let mut unknown = occurrence.clone();
        let target = match scope {
            Some(key) => &mut unknown[key],
            None => &mut unknown,
        };
        target["extra"] = json!(false);
        write(&unknown);
        assert!(ReplayCommandExecutor::load(fixtures.path(), workspace.path()).is_err());
    }
}

#[tokio::test]
async fn command_recording_replays_exact_relative_occurrence_without_running_an_executor() {
    let record_root = tempdir().expect("record workspace");
    let replay_root = tempdir().expect("replay workspace");
    let fixtures = tempdir().expect("fixtures");
    let dangerous_command = "nc 127.0.0.1 9 < secret";
    let recorder = RecordingCommandExecutor::new(
        Arc::new(StreamingExecutor),
        fixtures.path(),
        record_root.path(),
    )
    .expect("recorder");
    let recorded_sink = Arc::new(RecordingSink::default());
    let recorded_env = BTreeMap::from([
        (
            "HOME".to_owned(),
            record_root.path().to_string_lossy().into_owned(),
        ),
        (
            "TMPDIR".to_owned(),
            record_root.path().to_string_lossy().into_owned(),
        ),
    ]);
    let expected_outcome = recorder
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Sandboxed,
                network_domains: Vec::new(),
                command: dangerous_command.to_owned(),
                cwd: record_root.path().to_path_buf(),
                env: recorded_env,
            },
            CancellationToken::default(),
            recorded_sink.clone(),
        )
        .await
        .expect("record command");

    let offline_executor =
        ReplayCommandExecutor::load(fixtures.path(), replay_root.path()).expect("replay executor");
    let replayed_sink = Arc::new(RecordingSink::default());
    let replayed_env = BTreeMap::from([
        (
            "HOME".to_owned(),
            replay_root.path().to_string_lossy().into_owned(),
        ),
        (
            "TMPDIR".to_owned(),
            replay_root.path().to_string_lossy().into_owned(),
        ),
    ]);
    let actual_outcome = offline_executor
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Sandboxed,
                network_domains: Vec::new(),
                command: dangerous_command.to_owned(),
                cwd: replay_root.path().to_path_buf(),
                env: replayed_env.clone(),
            },
            CancellationToken::default(),
            replayed_sink.clone(),
        )
        .await
        .expect("replay command");

    assert_eq!(actual_outcome, expected_outcome);
    assert_eq!(
        replayed_sink.0.lock().expect("replayed output").as_slice(),
        recorded_sink.0.lock().expect("recorded output").as_slice()
    );
    assert!(matches!(
        offline_executor
            .run(
                CommandRequest {
                    sandbox: BashSandboxMode::Sandboxed,
                    network_domains: Vec::new(),
                    command: dangerous_command.to_owned(),
                    cwd: replay_root.path().to_path_buf(),
                    env: replayed_env,
                },
                CancellationToken::default(),
                Arc::new(RecordingSink::default()),
            )
            .await,
        Err(ToolError::Command(message)) if message.contains("exhausted")
    ));
}

#[tokio::test]
async fn command_fixture_redactor_runs_before_any_fixture_bytes_reach_disk() {
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let recorder = RecordingCommandExecutor::new_with_redactor(
        Arc::new(StreamingExecutor),
        fixtures.path(),
        workspace.path(),
        Arc::new(SecretRedactor),
    )
    .expect("recorder");
    recorder
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Sandboxed,
                network_domains: Vec::new(),
                command: "printf secret-canary".to_owned(),
                cwd: workspace.path().to_path_buf(),
                env: BTreeMap::from([("TOKEN".to_owned(), "secret-canary".to_owned())]),
            },
            CancellationToken::default(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("record command");
    let fixture =
        std::fs::read_to_string(fixtures.path().join(COMMAND_REPLAY_FILE)).expect("fixture");
    assert!(!fixture.contains("secret-canary"));
    assert!(fixture.contains("[REDACTED]"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(fixtures.path().join(COMMAND_REPLAY_FILE))
            .expect("fixture metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn replay_recovers_a_complete_private_stale_temp_and_rejects_symlinks() {
    use std::os::unix::fs::symlink;

    let record_workspace = tempdir().expect("record workspace");
    let replay_workspace = tempdir().expect("replay workspace");
    let fixtures = tempdir().expect("fixtures");
    let recorder = RecordingCommandExecutor::new(
        Arc::new(StreamingExecutor),
        fixtures.path(),
        record_workspace.path(),
    )
    .expect("recorder");
    recorder
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Sandboxed,
                network_domains: Vec::new(),
                command: "printf recovered".to_owned(),
                cwd: record_workspace.path().to_path_buf(),
                env: BTreeMap::new(),
            },
            CancellationToken::default(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("record command");
    let installed = fixtures.path().join(COMMAND_REPLAY_FILE);
    let temporary = fixtures.path().join(COMMAND_REPLAY_TEMP_FILE);
    std::fs::rename(&installed, &temporary).expect("simulate pre-rename crash");
    let replay = ReplayCommandExecutor::load(fixtures.path(), replay_workspace.path())
        .expect("recover stale temp");
    assert!(installed.is_file());
    assert!(!temporary.exists());
    replay
        .run(
            CommandRequest {
                sandbox: BashSandboxMode::Sandboxed,
                network_domains: Vec::new(),
                command: "printf recovered".to_owned(),
                cwd: replay_workspace.path().to_path_buf(),
                env: BTreeMap::new(),
            },
            CancellationToken::default(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("replay recovered occurrence");

    std::fs::remove_file(&installed).expect("remove fixture");
    let target = fixtures.path().join("attacker.json");
    std::fs::write(&target, b"[]").expect("attacker file");
    symlink(&target, &installed).expect("fixture symlink");
    assert!(matches!(
        ReplayCommandExecutor::load(fixtures.path(), replay_workspace.path()),
        Err(ToolError::Command(message)) if message.contains("regular file")
    ));
}
