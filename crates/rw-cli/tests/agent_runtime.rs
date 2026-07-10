#![allow(clippy::expect_used)]

use std::{
    fs,
    io::Write as _,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use rw_core::{
    EngineEvent, TurnStatus,
    runtime_support::{FinishReason, ProviderEvent},
};
use serde_json::json;
use tempfile::{TempDir, tempdir};

const PROMPT: &str = "create hello.py that prints hi, run it";
const STEERING: &str = "STEER_TOKEN_M2_CLI";

#[test]
fn binary_records_then_replays_a_complete_offline_tool_turn() {
    let fixture_root = tempdir().expect("fixture root");
    let fixture_dir = fixture_root.path().join("replay");
    let script_path = fixture_root.path().join("script.json");
    write_script(&script_path, hello_script());

    let record = TestRun::new(&fixture_root, "record");
    record.write_agents();
    let recorded = record.command(&fixture_dir, Some(&script_path), "stream-json");
    assert!(
        recorded.status.success(),
        "record stderr: {}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    assert_eq!(
        fs::read_to_string(record.workspace.join("hello.py")).expect("recorded hello.py"),
        "print(\"hi\")\n"
    );

    let replay = TestRun::new(&fixture_root, "replay");
    replay.write_agents();
    let replayed = replay.command(&fixture_dir, None, "stream-json");
    assert!(
        replayed.status.success(),
        "replay stderr: {}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    assert_eq!(
        fs::read_to_string(replay.workspace.join("hello.py")).expect("replayed hello.py"),
        "print(\"hi\")\n"
    );
    let events = parse_stream(&replayed.stdout);
    assert!(!events.is_empty());
    for (expected, event) in events.iter().filter_map(EngineEvent::meta).enumerate() {
        assert_eq!(event.sequence_id.0, expected as u64);
    }
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::ToolOutputDelta { chunk, .. } if chunk.contains("hi")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::TextDelta { text, .. } if text.contains(STEERING)
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::TurnFinished {
            status: TurnStatus::Completed,
            ..
        }
    )));

    let aggregate = TestRun::new(&fixture_root, "aggregate");
    aggregate.write_agents();
    let aggregated = aggregate.command(&fixture_dir, None, "json");
    assert!(
        aggregated.status.success(),
        "aggregate stderr: {}",
        String::from_utf8_lossy(&aggregated.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&aggregated.stdout).expect("aggregate JSON");
    assert_eq!(value["status"], "completed");
    assert!(
        value["text"]
            .as_str()
            .is_some_and(|text| text.contains(STEERING))
    );
    assert!(
        value["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty())
    );
}

#[test]
fn bash_replay_serves_recorded_output_without_spawning_or_opening_a_socket() {
    let root = tempdir().expect("root");
    let listener = TcpListener::bind("127.0.0.1:0").expect("canary listener");
    listener.set_nonblocking(true).expect("nonblocking canary");
    let address = listener.local_addr().expect("canary address");
    let accepts = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let accept_count = Arc::clone(&accepts);
    let accept_stop = Arc::clone(&stop);
    let acceptor = thread::spawn(move || {
        while !accept_stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((_socket, _)) => {
                    accept_count.fetch_add(1, Ordering::AcqRel);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("canary accept failed: {error}"),
            }
        }
    });
    let command = format!(
        "python3 -c 'import socket; s=socket.create_connection((\"127.0.0.1\", {})); s.sendall(b\"canary\"); print(\"recorded-output\")'; touch command-spawned",
        address.port()
    );
    let script = root.path().join("canary.json");
    write_script(&script, bash_script(&command));
    let fixtures = root.path().join("fixtures");

    let record = TestRun::new(&root, "canary-record");
    record.write_agents();
    let recorded = record.command(&fixtures, Some(&script), "stream-json");
    assert!(
        recorded.status.success(),
        "record stderr: {}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    wait_until(Duration::from_secs(3), || {
        accepts.load(Ordering::Acquire) == 1
    });
    assert!(record.workspace.join("command-spawned").is_file());

    let replay = TestRun::new(&root, "canary-replay");
    replay.write_agents();
    let replayed = replay.command(&fixtures, None, "stream-json");
    assert!(
        replayed.status.success(),
        "replay stderr: {}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    assert!(parse_stream(&replayed.stdout).iter().any(|event| matches!(
        event,
        EngineEvent::ToolOutputDelta { chunk, .. } if chunk.contains("recorded-output")
    )));
    thread::sleep(Duration::from_millis(200));
    assert_eq!(accepts.load(Ordering::Acquire), 1);
    assert!(!replay.workspace.join("command-spawned").exists());

    stop.store(true, Ordering::Release);
    acceptor.join().expect("canary acceptor");
}

#[test]
fn print_mode_auto_answers_structured_ask_user_without_a_third_channel() {
    let root = tempdir().expect("root");
    let fixtures = root.path().join("fixtures");
    let script = root.path().join("ask-user.json");
    write_script(
        &script,
        vec![
            vec![
                ProviderEvent::ToolCallStart {
                    id: "ask-one".to_owned(),
                    name: "ask_user".to_owned(),
                },
                ProviderEvent::ToolCallEnd {
                    id: "ask-one".to_owned(),
                    arguments: json!({
                        "question": "Which deterministic option should headless mode use?",
                        "options": ["first", "second"],
                        "allow_free_text": false
                    }),
                },
                ProviderEvent::Finished {
                    reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                ProviderEvent::TextDelta {
                    text: "structured question completed".to_owned(),
                },
                ProviderEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ],
        ],
    );
    let run = TestRun::new(&root, "headless-question");
    run.write_agents();
    let output = base_command(&run.workspace, &run.home)
        .args([
            "-p",
            "ask the structured question",
            "--permission-mode",
            "yolo",
            "--output-format",
            "stream-json",
            "--replay-dir",
            fixtures.to_str().expect("fixture path"),
            "--record-replay-script",
            script.to_str().expect("script path"),
        ])
        .output()
        .expect("headless question child");
    assert!(
        output.status.success(),
        "headless question stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_stream(&output.stdout);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::QuestionAsked { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::QuestionAnswered { answers, .. }
            if answers.iter().any(|answer| answer.values == ["first"])
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::TurnFinished {
            status: TurnStatus::Completed,
            ..
        }
    )));
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
        .join("events.jsonl");
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
    wait_until(Duration::from_secs(5), || {
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
    let mut first = base_command(&run.workspace, &run.home)
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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("first resume writer");
    let log_path = event_log(&run.home).expect("event log");
    wait_until(Duration::from_secs(5), || {
        fs::read_to_string(&log_path)
            .ok()
            .is_some_and(|log| log.contains(unique_prompt))
    });

    let before = fs::read(&log_path).expect("log before second writer");
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
    assert_eq!(
        fs::read(&log_path).expect("log after second writer"),
        before,
        "losing writer mutated the authoritative log"
    );

    assert!(
        Command::new("kill")
            .args(["-9", &first.id().to_string()])
            .status()
            .expect("kill first writer")
            .success()
    );
    assert!(!first.wait().expect("first writer wait").success());
}

#[test]
#[allow(clippy::too_many_lines)]
fn sigkill_mid_bash_waits_for_watchdog_then_recovers_opaque_checkpoint() {
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
    let mut child = base_command(&run.workspace, &run.home)
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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("opaque bash child");
    wait_until(Duration::from_secs(5), || {
        run.workspace.join("mutated.txt").is_file()
            && read_pid(&run.workspace.join("child.pid")).is_some()
    });
    let shell_pid = read_pid(&run.workspace.join("child.pid")).expect("shell pid");
    assert!(
        Command::new("kill")
            .args(["-9", &child.id().to_string()])
            .status()
            .expect("kill CLI")
            .success()
    );
    assert!(!child.wait().expect("killed CLI wait").success());

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
    let mut child = base_command(&run.workspace, &run.home)
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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("bash child");
    wait_until(Duration::from_secs(5), || {
        read_pid(&run.workspace.join("child.pid")).is_some()
    });
    let shell_pid = read_pid(&run.workspace.join("child.pid"))
        .expect("numeric child pid")
        .to_string();
    assert!(
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .expect("send SIGINT")
            .success()
    );
    let status = child.wait().expect("interrupted child");
    assert!(!status.success());
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
    let log_a = fs::read_to_string(home.join("sessions").join(&session_a).join("events.jsonl"))
        .expect("a log");
    let log_b = fs::read_to_string(home.join("sessions").join(&session_b).join("events.jsonl"))
        .expect("b log");
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

struct TestRun {
    workspace: PathBuf,
    home: PathBuf,
}

impl TestRun {
    fn new(root: &TempDir, name: &str) -> Self {
        let workspace = root.path().join(name);
        let home = root.path().join(format!("{name}-home"));
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&home).expect("home");
        Self { workspace, home }
    }

    fn write_agents(&self) {
        fs::write(
            self.workspace.join("AGENTS.md"),
            format!("Always include the literal token {STEERING} in the final answer.\n"),
        )
        .expect("AGENTS.md");
    }

    fn command(
        &self,
        fixtures: &Path,
        record_script: Option<&Path>,
        output_format: &str,
    ) -> std::process::Output {
        let mut command = base_command(&self.workspace, &self.home);
        command.args([
            "-p",
            PROMPT,
            "--permission-mode",
            "yolo",
            "--output-format",
            output_format,
            "--replay-dir",
            fixtures.to_str().expect("fixture path"),
            "--replay-provider",
            "cli-replay",
        ]);
        if let Some(script) = record_script {
            command.args([
                "--record-replay-script",
                script.to_str().expect("script path"),
            ]);
        }
        command.output().expect("rw binary")
    }

    fn resume_command(
        &self,
        fixtures: &Path,
        script: &Path,
        session_id: &str,
    ) -> std::process::Output {
        let mut command = base_command(&self.workspace, &self.home);
        command.args([
            "-p",
            "continue after the crash",
            "--resume",
            session_id,
            "--permission-mode",
            "yolo",
            "--replay-dir",
            fixtures.to_str().expect("fixture path"),
            "--replay-provider",
            "cli-replay",
            "--record-replay-script",
            script.to_str().expect("script path"),
        ]);
        command.output().expect("rw resume binary")
    }
}

fn base_command(workspace: &Path, home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rw"));
    command
        .env_clear()
        .current_dir(workspace)
        .env("HOME", home)
        .env("ROTTWEILER_HOME", home)
        .env("ROTTWEILER_CREDENTIAL_BACKEND", "file");
    command
}

fn hello_script() -> Vec<Vec<ProviderEvent>> {
    vec![
        vec![
            ProviderEvent::ToolCallStart {
                id: "write-hello".to_owned(),
                name: "write".to_owned(),
            },
            ProviderEvent::ToolCallEnd {
                id: "write-hello".to_owned(),
                arguments: json!({"path": "hello.py", "content": "print(\"hi\")\n"}),
            },
            ProviderEvent::ToolCallStart {
                id: "run-hello".to_owned(),
                name: "bash".to_owned(),
            },
            ProviderEvent::ToolCallEnd {
                id: "run-hello".to_owned(),
                arguments: json!({"command": "python3 hello.py"}),
            },
            ProviderEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ],
        vec![
            ProviderEvent::TextDelta {
                text: format!("{STEERING}: created and ran hello.py"),
            },
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        ],
    ]
}

fn bash_script(command: &str) -> Vec<Vec<ProviderEvent>> {
    vec![
        vec![
            ProviderEvent::ToolCallStart {
                id: "canary-bash".to_owned(),
                name: "bash".to_owned(),
            },
            ProviderEvent::ToolCallEnd {
                id: "canary-bash".to_owned(),
                arguments: json!({"command": command}),
            },
            ProviderEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ],
        vec![
            ProviderEvent::TextDelta {
                text: "canary complete".to_owned(),
            },
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        ],
    ]
}

#[allow(clippy::needless_pass_by_value)]
fn write_script(path: &Path, script: Vec<Vec<ProviderEvent>>) {
    fs::write(path, serde_json::to_vec(&script).expect("script JSON")).expect("script file");
}

fn parse_stream(bytes: &[u8]) -> Vec<EngineEvent> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| serde_json::from_str(line).expect("stream-json event"))
        .collect()
}

fn only_session_id(home: &Path) -> String {
    let sessions = fs::read_dir(home.join("sessions"))
        .expect("sessions")
        .collect::<Result<Vec<_>, _>>()
        .expect("session entries");
    assert_eq!(sessions.len(), 1);
    sessions[0]
        .file_name()
        .into_string()
        .expect("UTF-8 session id")
}

fn fixture_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).expect("fixture directory") {
            let entry = entry.expect("fixture entry");
            if entry.file_type().expect("fixture type").is_dir() {
                pending.push(entry.path());
            } else {
                files.push(entry.path());
            }
        }
    }
    files
}

fn event_log(home: &Path) -> Option<PathBuf> {
    let session = fs::read_dir(home.join("sessions"))
        .ok()?
        .find_map(Result::ok)?;
    Some(session.path().join("events.jsonl"))
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("condition was not met within {timeout:?}");
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn session_ids_by_workspace(home: &Path) -> std::collections::BTreeMap<PathBuf, String> {
    fs::read_dir(home.join("sessions"))
        .expect("sessions")
        .map(|entry| {
            let entry = entry.expect("session entry");
            let metadata: serde_json::Value = serde_json::from_slice(
                &fs::read(entry.path().join("metadata.json")).expect("metadata"),
            )
            .expect("metadata JSON");
            (
                PathBuf::from(metadata["workspace"].as_str().expect("workspace path")),
                entry.file_name().into_string().expect("session id"),
            )
        })
        .collect()
}
