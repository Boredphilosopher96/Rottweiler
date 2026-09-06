use super::*;

#[test]
fn sessions_verify_checks_the_journal_and_rejects_unsupported_layout() {
    let root = tempdir().expect("root");
    let home = private_test_directory(&root.path().join("home"));
    let workspace = private_test_directory(&root.path().join("workspace"));
    let mut log = rw_store::session::SessionEventLog::open(&home, "verified").expect("journal");
    log.append(EngineEvent::UiNotification {
        meta: rw_core::EventMeta {
            protocol_version: rw_core::PROTOCOL_VERSION,
            session_id: rw_core::SessionId("verified".to_owned()),
            sequence_id: rw_core::SequenceId(0),
            emitted_at: "2026-09-04T00:00:00Z".to_owned(),
            caused_by: None,
        },
        plugin_id: "fixture".to_owned(),
        title: "verification".to_owned(),
        message: "complete".to_owned(),
    })
    .expect("event");
    let bytes = log.read_view().total_bytes();
    drop(log);
    let output = base_command(&workspace, &home)
        .args(["sessions", "--output-format", "json", "verify", "verified"])
        .output()
        .expect("verify command");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("verification JSON");
    assert_eq!(
        report,
        json!({"session_id": "verified", "events": 1, "bytes": bytes})
    );
    let unsupported = home.join("sessions/unsupported");
    fs::create_dir_all(&unsupported).expect("unsupported directory");
    fs::write(
        unsupported.join("events.jsonl"),
        b"nonempty unsupported journal\n",
    )
    .expect("unsupported fixture");
    let output = base_command(&workspace, &home)
        .args(["sessions", "verify", "unsupported"])
        .output()
        .expect("reject command");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unexpected events.jsonl file"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!unsupported.join("journal").exists());
}

#[cfg(unix)]
#[test]
fn m9_rw_replay_renders_a_persisted_envelope_log_through_production_tui() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::Builder::new()
        .prefix("rw-m9-")
        .tempdir_in("/tmp")
        .expect("short root");
    let home = private_test_directory(&root.path().join("home"));
    let workspace = private_test_directory(&root.path().join("workspace"));
    let session_id = "session-m9-replay-golden";
    let session = home.join("sessions").join(session_id);
    fs::create_dir_all(&session).expect("session directory");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/tui/test/fixtures/m9-replay-events.jsonl");
    let mut persisted =
        rw_store::session::SessionEventLog::open(&home, session_id).expect("event log");
    for (sequence, line) in fs::read_to_string(source)
        .expect("replay fixture")
        .lines()
        .enumerate()
    {
        let mut event: serde_json::Value = serde_json::from_str(line).expect("fixture event");
        event["meta"]["sequence_id"] = json!(sequence.to_string());
        persisted.append(event).expect("durable fixture event");
    }
    drop(persisted);

    let worker = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/tui/test/goldens/replay-cli-worker.ts")
        .canonicalize()
        .expect("replay worker");
    let wrapper = root.path().join("replay-tui");
    fs::write(
        &wrapper,
        format!("#!/bin/sh\nexec bun '{}'\n", worker.display()),
    )
    .expect("TUI wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).expect("wrapper mode");
    let report = root.path().join("replay-report.json");
    let output = base_command(&workspace, &home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("ROTTWEILER_TUI_BIN", &wrapper)
        .env("ROTTWEILER_TEST_REPORT_FILE", &report)
        .args(["replay", session_id])
        .output()
        .expect("rw replay process");
    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let actual: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(report).expect("replay report"))
            .expect("valid replay report");
    assert_eq!(actual["historyThrough"], "8", "available committed prefix");
    assert_eq!(actual["mountedItems"], 4, "semantic history rows");
    assert!(
        actual["completedThrough"].is_null(),
        "history is not raw replay"
    );
    assert!(
        actual["lastSequence"].is_null(),
        "availability cannot advance durable cursor"
    );
    assert_eq!(actual["invalidEvents"], 0, "replay protocol validation");

    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../packages/tui/test/goldens/fixtures/m9-replay-cli.golden.json"
    ))
    .expect("valid visual golden");
    for field in ["frame", "styledDigest", "styledSpanCount"] {
        assert_eq!(
            actual[field], expected[field],
            "replay visual contract: {field}; actual={actual}"
        );
    }
}

#[test]
fn resume_repairs_a_killed_tail_and_reuses_the_original_agents_prefix() {
    let root = tempdir().expect("root");
    let fixtures = root.path().join("fixtures");
    let first_script = root.path().join("first.json");
    write_script(&first_script, hello_script());
    let run = TestRun::new(&root, "workspace");
    run.write_agents();
    let first = run.command(&fixtures, Some(&first_script), "text");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    let session_id = only_session_id(&run.home);
    fs::write(
        run.workspace.join("AGENTS.md"),
        "NEW_TOKEN_MUST_NOT_REPLACE_PREFIX\n",
    )
    .expect("changed agents");
    let log_path = run
        .home
        .join("sessions")
        .join(&session_id)
        .join("journal")
        .join("active.jsonl");
    let mut tail = fs::OpenOptions::new()
        .append(true)
        .open(&log_path)
        .expect("event tail");
    tail.write_all(b"{\"schema_version\":1,\"sequence\":999,\"event\":")
        .expect("partial killed tail");
    tail.sync_all().expect("sync killed tail");
    drop(tail);

    let second_script = root.path().join("second.json");
    write_script(
        &second_script,
        vec![vec![
            ProviderEvent::TextDelta {
                text: "resumed".to_owned(),
            },
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]],
    );
    let resumed = run.resume_command(&fixtures, &second_script, &session_id);
    assert!(
        resumed.status.success(),
        "resume stderr: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    for line in fs::read_to_string(&log_path)
        .expect("well-formed event log")
        .lines()
    {
        let _: serde_json::Value = serde_json::from_str(line).expect("complete JSONL record");
    }
    let fixture_bytes = fixture_files(&fixtures)
        .into_iter()
        .flat_map(|path| fs::read(path).expect("fixture bytes"))
        .collect::<Vec<_>>();
    let fixtures_text = String::from_utf8_lossy(&fixture_bytes);
    assert!(fixtures_text.contains(STEERING));
    assert!(!fixtures_text.contains("NEW_TOKEN_MUST_NOT_REPLACE_PREFIX"));
}

#[test]
fn kill_nine_mid_provider_is_closed_as_interrupted_on_resume() {
    let root = tempdir().expect("root");
    let fixtures = root.path().join("fixtures");
    let delayed_script = root.path().join("delayed.json");
    write_script(
        &delayed_script,
        vec![vec![
            ProviderEvent::TextDelta {
                text: "too late".to_owned(),
            },
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]],
    );
    let run = TestRun::new(&root, "kill-workspace");
    run.write_agents();
    let mut child = base_command(&run.workspace, &run.home)
        .args([
            "-p",
            "wait for the delayed provider",
            "--permission-mode",
            "yolo",
            "--replay-dir",
            fixtures.to_str().expect("fixture path"),
            "--record-replay-script",
            delayed_script.to_str().expect("script path"),
            "--record-script-delay-ms",
            "30000",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("delayed child");
    // This acceptance test waits for a semantic crash point; cold-start latency
    // is enforced by the dedicated release performance gate. Debug binaries
    // include the WASM compiler and can take longer to validate after relink on
    // macOS, especially under the full parallel workspace test load.
    wait_until(Duration::from_secs(15), || {
        event_log(&run.home)
            .and_then(|path| fs::read_to_string(path).ok())
            .is_some_and(|log| log.contains("user_message_accepted"))
    });
    let killed = Command::new("kill")
        .args(["-9", &child.id().to_string()])
        .status()
        .expect("kill -9");
    assert!(killed.success());
    assert!(!child.wait().expect("killed child").success());

    let session_id = only_session_id(&run.home);
    let resume_script = root.path().join("resume.json");
    write_script(
        &resume_script,
        vec![vec![
            ProviderEvent::TextDelta {
                text: "recovered".to_owned(),
            },
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]],
    );
    let resumed = run.resume_command(&fixtures, &resume_script, &session_id);
    assert!(
        resumed.status.success(),
        "resume stderr: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let log = fs::read_to_string(event_log(&run.home).expect("event log")).expect("event log");
    for line in log.lines() {
        let _: serde_json::Value = serde_json::from_str(line).expect("well-formed JSONL");
    }
    assert!(log.contains("\"status\":\"interrupted\""));
    assert!(log.contains("\"status\":\"completed\""));
}

#[test]
#[allow(clippy::too_many_lines)]
fn simultaneous_resume_rejects_the_second_writer_before_startup_mutation() {
    let root = tempdir().expect("root");
    let fixtures = root.path().join("fixtures");
    let initial_script = root.path().join("initial.json");
    write_script(
        &initial_script,
        vec![vec![
            ProviderEvent::TextDelta {
                text: "initial".to_owned(),
            },
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]],
    );
    let run = TestRun::new(&root, "writer-race");
    run.write_agents();
    let initial = run.command(&fixtures, Some(&initial_script), "text");
    assert!(
        initial.status.success(),
        "initial stderr: {}",
        String::from_utf8_lossy(&initial.stderr)
    );
    let session_id = only_session_id(&run.home);

    let delayed_script = root.path().join("delayed-resume.json");
    write_script(
        &delayed_script,
        vec![vec![
            ProviderEvent::TextDelta {
                text: "too late".to_owned(),
            },
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]],
    );
    let unique_prompt = "FIRST_RESUME_WRITER_OWNS_SESSION";
    let mut first = TestProcess::spawn(
        base_command(&run.workspace, &run.home)
            .args([
                "-p",
                unique_prompt,
                "--resume",
                &session_id,
                "--permission-mode",
                "yolo",
                "--replay-dir",
                fixtures.to_str().expect("fixture path"),
                "--record-replay-script",
                delayed_script.to_str().expect("script path"),
                "--record-script-delay-ms",
                "30000",
            ])
            .stdout(Stdio::null()),
    );
    let log_path = event_log(&run.home).expect("event log");
    first.wait_ready(|| {
        fs::read_to_string(&log_path)
            .ok()
            .is_some_and(|log| log.contains(unique_prompt))
    });

    let started = Instant::now();
    let second = base_command(&run.workspace, &run.home)
        .args([
            "-p",
            "SECOND_WRITER_MUST_NOT_PERSIST",
            "--resume",
            &session_id,
            "--permission-mode",
            "yolo",
            "--replay-dir",
            fixtures.to_str().expect("fixture path"),
            "--record-replay-script",
            delayed_script.to_str().expect("script path"),
        ])
        .output()
        .expect("second resume writer");
    assert!(
        !second.status.success(),
        "second writer unexpectedly started"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "second writer did not fail fast"
    );
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("session log could not open"),
        "unexpected second-writer stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let after = fs::read_to_string(&log_path).expect("log after second writer");
    assert!(
        !after.contains("SECOND_WRITER_MUST_NOT_PERSIST"),
        "losing writer persisted its prompt in the authoritative log"
    );
    for line in after.lines().filter(|line| !line.trim().is_empty()) {
        serde_json::from_str::<serde_json::Value>(line)
            .expect("authoritative log remains valid JSONL while the winner appends");
    }

    assert!(
        Command::new("kill")
            .args(["-9", &first.child.id().to_string()])
            .status()
            .expect("kill first writer")
            .success()
    );
    assert!(!first.child.wait().expect("first writer wait").success());
}

#[test]
#[allow(clippy::too_many_lines)]
fn sigkill_mid_bash_waits_for_watchdog_then_recovers_opaque_checkpoint() {
    if probe_sandbox().support != SandboxSupport::Enforced {
        eprintln!("skipping live SIGKILL acceptance: sandbox enforcement is unavailable");
        return;
    }
    let root = tempdir().expect("root");
    let fixtures = root.path().join("fixtures");
    let script = root.path().join("opaque-bash.json");
    write_script(
        &script,
        vec![vec![
            ProviderEvent::ToolCallStart {
                id: "opaque-bash".to_owned(),
                name: "bash".to_owned(),
            },
            ProviderEvent::ToolCallEnd {
                id: "opaque-bash".to_owned(),
                arguments: json!({
                    "command": "echo $$ > child.pid; printf mutated > mutated.txt; sleep 30; printf late > late.txt"
                }),
            },
            ProviderEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ]],
    );
    let run = TestRun::new(&root, "opaque-kill");
    run.write_agents();
    let mut child = TestProcess::spawn(
        base_command(&run.workspace, &run.home)
            .args([
                "-p",
                "run the opaque command",
                "--permission-mode",
                "yolo",
                "--replay-dir",
                fixtures.to_str().expect("fixture path"),
                "--record-replay-script",
                script.to_str().expect("script path"),
            ])
            .stdout(Stdio::null()),
    );
    child.wait_ready(|| {
        run.workspace.join("mutated.txt").is_file()
            && read_pid(&run.workspace.join("child.pid")).is_some()
    });
    #[cfg(not(target_os = "linux"))]
    let shell_pid = read_pid(&run.workspace.join("child.pid")).expect("shell pid");
    assert!(
        Command::new("kill")
            .args(["-9", &child.child.id().to_string()])
            .status()
            .expect("kill CLI")
            .success()
    );
    assert!(!child.child.wait().expect("killed CLI wait").success());

    let session_id = only_session_id(&run.home);
    let resume_script = root.path().join("opaque-resume.json");
    write_script(
        &resume_script,
        vec![vec![
            ProviderEvent::TextDelta {
                text: "opaque recovery complete".to_owned(),
            },
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]],
    );
    let resumed = run.resume_command(&fixtures, &resume_script, &session_id);
    assert!(
        resumed.status.success(),
        "resume stderr: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    // Linux commands run in a PID namespace, so the PID written by the shell
    // is namespace-local and must not be queried in the host PID table. The
    // privileged linux_egress gate separately proves descendant termination.
    #[cfg(not(target_os = "linux"))]
    assert!(
        !Command::new("kill")
            .args(["-0", &shell_pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success()),
        "command process survived recovery barrier"
    );
    // Resume completes the killed command's post-scan; it does not silently
    // rewind user-visible files. The resulting manifest makes an explicit
    // later `/rewind` able to remove these newly-created paths.
    assert!(run.workspace.join("child.pid").exists());
    assert!(run.workspace.join("mutated.txt").exists());
    assert!(!run.workspace.join("late.txt").exists());
    let checkpoint_files = fixture_files(&run.home.join("workspaces"));
    assert!(
        checkpoint_files.iter().all(|path| {
            !path
                .components()
                .any(|component| component.as_os_str() == "pending")
        }),
        "opaque pending marker survived resume"
    );
    let manifests = checkpoint_files
        .into_iter()
        .filter(|path| {
            path.components()
                .any(|component| component.as_os_str() == "manifests")
                && path
                    .extension()
                    .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    assert_eq!(manifests.len(), 1, "expected one recovered manifest");
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifests[0]).expect("recovered opaque manifest bytes"))
            .expect("recovered opaque manifest JSON");
    assert_eq!(manifest["files"]["child.pid"]["state"], "absent");
    assert_eq!(manifest["files"]["mutated.txt"]["state"], "absent");
    let log = fs::read_to_string(event_log(&run.home).expect("event log")).expect("event log");
    for line in log.lines() {
        let _: serde_json::Value = serde_json::from_str(line).expect("well-formed JSONL");
    }
    assert!(log.contains("\"status\":\"interrupted\""));
    assert!(log.contains("\"status\":\"completed\""));
}

#[test]
fn sigint_mid_bash_closes_the_log_and_kills_the_process_group() {
    if probe_sandbox().support != SandboxSupport::Enforced {
        eprintln!("skipping live SIGINT acceptance: sandbox enforcement is unavailable");
        return;
    }
    let root = tempdir().expect("root");
    let fixtures = root.path().join("fixtures");
    let script = root.path().join("bash.json");
    write_script(
        &script,
        vec![vec![
            ProviderEvent::ToolCallStart {
                id: "long-bash".to_owned(),
                name: "bash".to_owned(),
            },
            ProviderEvent::ToolCallEnd {
                id: "long-bash".to_owned(),
                arguments: json!({
                    "command": "echo $$ > child.pid; exec sleep 30"
                }),
            },
            ProviderEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ]],
    );
    let run = TestRun::new(&root, "interrupt-workspace");
    run.write_agents();
    let mut child = TestProcess::spawn(
        base_command(&run.workspace, &run.home)
            .args([
                "-p",
                "run a long command",
                "--permission-mode",
                "yolo",
                "--replay-dir",
                fixtures.to_str().expect("fixture path"),
                "--record-replay-script",
                script.to_str().expect("script path"),
            ])
            .stdout(Stdio::null()),
    );
    child.wait_ready(|| read_pid(&run.workspace.join("child.pid")).is_some());
    #[cfg(not(target_os = "linux"))]
    let shell_pid = read_pid(&run.workspace.join("child.pid"))
        .expect("numeric child pid")
        .to_string();
    assert!(
        Command::new("kill")
            .args(["-INT", &child.child.id().to_string()])
            .status()
            .expect("send SIGINT")
            .success()
    );
    let status = child.child.wait().expect("interrupted child");
    assert!(!status.success());
    #[cfg(not(target_os = "linux"))]
    wait_until(Duration::from_secs(5), || {
        !Command::new("kill")
            .args(["-0", &shell_pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    });
    let log = fs::read_to_string(event_log(&run.home).expect("event log")).expect("event log");
    for line in log.lines() {
        let _: serde_json::Value = serde_json::from_str(line).expect("well-formed JSONL");
    }
    assert!(log.contains("\"status\":\"interrupted\""));
}

#[test]
fn continue_filters_by_canonical_workspace_and_resume_rejects_cwd_mismatch() {
    let root = tempdir().expect("root");
    let home = root.path().join("shared-home");
    let workspace_a = root.path().join("workspace-a");
    let workspace_b = root.path().join("workspace-b");
    fs::create_dir_all(&home).expect("home");
    fs::create_dir_all(&workspace_a).expect("workspace a");
    fs::create_dir_all(&workspace_b).expect("workspace b");
    for workspace in [&workspace_a, &workspace_b] {
        fs::write(workspace.join("AGENTS.md"), format!("Use {STEERING}.\n")).expect("AGENTS.md");
    }
    let fixtures = root.path().join("fixtures");
    let hello = root.path().join("hello.json");
    write_script(&hello, hello_script());
    for workspace in [&workspace_a, &workspace_b] {
        let output = base_command(workspace, &home)
            .args([
                "-p",
                PROMPT,
                "--permission-mode",
                "yolo",
                "--replay-dir",
                fixtures.to_str().expect("fixtures"),
                "--record-replay-script",
                hello.to_str().expect("script"),
            ])
            .output()
            .expect("create session");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let sessions = session_ids_by_workspace(&home);
    let session_a = sessions
        .get(&fs::canonicalize(&workspace_a).expect("canonical a"))
        .expect("session a")
        .clone();
    let session_b = sessions
        .get(&fs::canonicalize(&workspace_b).expect("canonical b"))
        .expect("session b")
        .clone();

    let stop = root.path().join("stop.json");
    write_script(
        &stop,
        vec![vec![
            ProviderEvent::TextDelta {
                text: "continued-a".to_owned(),
            },
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]],
    );
    let continued = base_command(&workspace_a, &home)
        .args([
            "-p",
            "CONTINUE_ONLY_WORKSPACE_A",
            "--continue",
            "--permission-mode",
            "yolo",
            "--replay-dir",
            fixtures.to_str().expect("fixtures"),
            "--record-replay-script",
            stop.to_str().expect("stop script"),
        ])
        .output()
        .expect("continue a");
    assert!(
        continued.status.success(),
        "continue stderr: {}",
        String::from_utf8_lossy(&continued.stderr)
    );
    let log_a = read_session_journal(&home, &session_a);
    let log_b = read_session_journal(&home, &session_b);
    assert!(log_a.contains("CONTINUE_ONLY_WORKSPACE_A"));
    assert!(!log_b.contains("CONTINUE_ONLY_WORKSPACE_A"));

    let mismatch = base_command(&workspace_b, &home)
        .args([
            "-p",
            "must not run",
            "--resume",
            &session_a,
            "--permission-mode",
            "yolo",
            "--replay-dir",
            fixtures.to_str().expect("fixtures"),
            "--record-replay-script",
            stop.to_str().expect("stop script"),
        ])
        .output()
        .expect("cwd mismatch");
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("metadata identity does not match"));
}
