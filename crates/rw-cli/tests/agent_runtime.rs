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
    runtime_support::{FinishReason, ProviderEvent, SandboxSupport, probe_sandbox},
};
use serde_json::json;
use tempfile::{TempDir, tempdir};

const PROMPT: &str = "create hello.py that prints hi, run it";
const STEERING: &str = "STEER_TOKEN_M2_CLI";

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
    let mut persisted = fs::File::create(session.join("events.jsonl")).expect("event log");
    for (sequence, line) in fs::read_to_string(source)
        .expect("replay fixture")
        .lines()
        .enumerate()
    {
        let mut event: serde_json::Value = serde_json::from_str(line).expect("fixture event");
        event["meta"]["sequence_id"] = json!(sequence.to_string());
        serde_json::to_writer(
            &mut persisted,
            &json!({
                "schema_version": 1,
                "sequence": sequence.to_string(),
                "event": event,
            }),
        )
        .expect("persisted envelope");
        persisted.write_all(b"\n").expect("event newline");
    }
    persisted.sync_all().expect("durable event fixture");

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
    let actual = fs::read_to_string(report).expect("replay report");
    let expected =
        include_str!("../../../packages/tui/test/goldens/fixtures/m9-replay-cli.golden.json");
    assert_eq!(actual, expected);
}

#[test]
fn m7_parent_spawns_three_parallel_worktree_children_and_keeps_main_clean() {
    let root = tempdir().expect("root");
    let run = TestRun::new(&root, "m7-parallel-worktrees");
    init_git_repository(&run.workspace);
    let script = root.path().join("m7-parallel.json");
    let mut first = Vec::new();
    for index in 0..3 {
        let id = format!("spawn-{index}");
        first.push(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: "spawn_agent".to_owned(),
        });
        first.push(ProviderEvent::ToolCallEnd {
            id,
            arguments: json!({
                "task": format!("inspect isolated branch {index}"),
                "agent": "explore",
                "isolation": "worktree"
            }),
        });
    }
    first.push(ProviderEvent::Finished {
        reason: FinishReason::ToolCalls,
    });
    write_script(
        &script,
        vec![
            first,
            text_events("explorer result one"),
            text_events("explorer result two"),
            text_events("explorer result three"),
            text_events("collated all three explorers"),
        ],
    );

    let output = base_command(&run.workspace, &run.home)
        .args([
            "-p",
            "run three isolated explorers and collate them",
            "--permission-mode",
            "yolo",
            "--output-format",
            "stream-json",
            "--in-memory-replay-script",
            script.to_str().expect("script"),
        ])
        .output()
        .expect("rw binary");
    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let events = parse_stream(&output.stdout);
    let spawned = events
        .iter()
        .filter(|event| matches!(event, EngineEvent::SubagentSpawned { .. }))
        .count();
    assert_eq!(
        spawned,
        3,
        "expected three real child spawns; stderr: {}\nevents: {events:#?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, EngineEvent::SubagentFinished { .. }))
            .count(),
        3
    );
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::TextDelta { text, .. } if text.contains("collated all three explorers")
    )));
    assert!(git_output(&run.workspace, &["diff", "--binary", "HEAD", "--"]).is_empty());
    assert!(git_output(&run.workspace, &["status", "--porcelain=v1"]).is_empty());
}

#[test]
fn binary_records_then_replays_a_complete_offline_tool_turn() {
    if probe_sandbox().support != SandboxSupport::Enforced {
        eprintln!("skipping live record/replay acceptance: sandbox enforcement is unavailable");
        return;
    }
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
        "replay stderr: {}\nreplay stdout: {}",
        String::from_utf8_lossy(&replayed.stderr),
        String::from_utf8_lossy(&replayed.stdout)
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
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::ToolOutputDelta { chunk, .. } if chunk.contains("hi")
        )),
        "replayed events did not contain bash output: {events:#?}"
    );
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
    if probe_sandbox().support != SandboxSupport::Enforced {
        eprintln!("skipping live bash replay acceptance: sandbox enforcement is unavailable");
        return;
    }
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
        "python3 -c 'import socket; s=socket.create_connection((\"127.0.0.1\", {})); s.sendall(b\"canary\")'; test $? -ne 0; printf recorded-output; touch command-spawned",
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
    thread::sleep(Duration::from_millis(200));
    assert_eq!(accepts.load(Ordering::Acquire), 0);
    assert!(record.workspace.join("command-spawned").is_file());

    let replay = TestRun::new(&root, "canary-replay");
    replay.write_agents();
    let replayed = replay.command(&fixtures, None, "stream-json");
    assert!(
        replayed.status.success(),
        "replay stderr: {}\nreplay stdout: {}",
        String::from_utf8_lossy(&replayed.stderr),
        String::from_utf8_lossy(&replayed.stdout)
    );
    assert!(parse_stream(&replayed.stdout).iter().any(|event| matches!(
        event,
        EngineEvent::ToolOutputDelta { chunk, .. } if chunk.contains("recorded-output")
    )));
    thread::sleep(Duration::from_millis(200));
    assert_eq!(accepts.load(Ordering::Acquire), 0);
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
fn print_mode_slash_command_finishes_without_waiting_for_a_turn() {
    let root = tempdir().expect("root");
    let script = root.path().join("unused-provider.json");
    write_script(&script, Vec::new());
    let run = TestRun::new(&root, "headless-command");
    run.write_agents();

    let output = base_command(&run.workspace, &run.home)
        .args([
            "-p",
            "/status",
            "--output-format",
            "stream-json",
            "--in-memory-replay-script",
            script.to_str().expect("script path"),
        ])
        .output()
        .expect("headless slash command");
    assert!(
        output.status.success(),
        "headless command stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_stream(&output.stdout);
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::CommandFinished { name, .. } if name == "status"
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        EngineEvent::TurnStarted { .. } | EngineEvent::TurnFinished { .. }
    )));
}

#[cfg(unix)]
#[test]
fn local_tui_launch_anchors_relative_added_roots_before_repository_discovery() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::Builder::new()
        .prefix("rw-launch-roots-")
        .tempdir_in("/tmp")
        .expect("short root");
    let run = TestRun::new(&root, "repository");
    init_git_repository(&run.workspace);
    let nested = run.workspace.join("src/nested");
    let added = root.path().join("added-root");
    fs::create_dir_all(&nested).expect("nested launch directory");
    fs::create_dir(&added).expect("additional workspace root");
    let script = root.path().join("offline.json");
    write_script(&script, Vec::new());
    let report = root.path().join("launch-cwd");
    let wrapper = root.path().join("cwd-tui");
    fs::write(
        &wrapper,
        "#!/bin/sh\npwd > \"$ROTTWEILER_TEST_REPORT_FILE\"\nexit 0\n",
    )
    .expect("TUI wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).expect("wrapper mode");

    let output = base_command(&nested, &run.home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("ROTTWEILER_TUI_BIN", &wrapper)
        .env("ROTTWEILER_TEST_REPORT_FILE", &report)
        .args([
            "--add-dir",
            "../../../added-root",
            "--in-memory-replay-script",
            script.to_str().expect("script path"),
        ])
        .output()
        .expect("supervised TUI process");
    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    assert_eq!(
        fs::canonicalize(fs::read_to_string(&report).expect("TUI cwd report").trim())
            .expect("canonical reported cwd"),
        fs::canonicalize(&run.workspace).expect("canonical repository"),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&format!(
            "workspace: {}",
            fs::canonicalize(&added)
                .expect("canonical additional root")
                .display()
        )),
        "relative --add-dir must resolve from the nested invocation cwd"
    );
}

#[cfg(unix)]
#[test]
fn supervised_tui_crosses_the_real_host_for_commands_and_tool_approval() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::Builder::new()
        .prefix("rw-host-ui-")
        .tempdir_in("/tmp")
        .expect("short root");
    let run = TestRun::new(&root, "full-host-tui-roundtrip");
    let script = root.path().join("tool-roundtrip.json");
    write_script(
        &script,
        vec![
            vec![
                ProviderEvent::ToolCallStart {
                    id: "write-canary".to_owned(),
                    name: "write".to_owned(),
                },
                ProviderEvent::ToolCallEnd {
                    id: "write-canary".to_owned(),
                    arguments: json!({
                        "path": "approval.txt",
                        "content": "ROTTWEILER_FULL_HOST_CANARY\n",
                    }),
                },
                ProviderEvent::Finished {
                    reason: FinishReason::ToolCalls,
                },
            ],
            text_events("The approved write completed."),
        ],
    );
    let worker = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/tui/test/full-host-roundtrip-worker.ts")
        .canonicalize()
        .expect("roundtrip worker");
    let wrapper = root.path().join("roundtrip-tui");
    fs::write(
        &wrapper,
        format!("#!/bin/sh\nexec bun '{}'\n", worker.display()),
    )
    .expect("TUI wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).expect("wrapper mode");
    let report = root.path().join("roundtrip-report.json");

    let output = base_command(&run.workspace, &run.home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("ROTTWEILER_TUI_BIN", &wrapper)
        .env("ROTTWEILER_TEST_REPORT_FILE", &report)
        .args([
            "--dangerously-trust",
            "--in-memory-replay-script",
            script.to_str().expect("script path"),
        ])
        .output()
        .expect("supervised TUI process");
    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&report).expect("roundtrip report"))
            .expect("valid roundtrip report");
    assert!(
        report["commandResult"]
            .as_str()
            .is_some_and(|value| value.contains("Agent: idle"))
    );
    assert!(report["approvalBanner"].as_str().is_some_and(|value| {
        value.contains("Waiting for approval") && value.contains("Write file")
    }));
    assert!(
        report["approvalPanel"]
            .as_str()
            .is_some_and(|value| value.contains("approval.txt"))
    );
    assert_eq!(report["toolStatus"], "finished");
    assert_eq!(report["errors"], json!([]));
    assert_eq!(
        fs::read_to_string(run.workspace.join("approval.txt")).expect("approved write output"),
        "ROTTWEILER_FULL_HOST_CANARY\n",
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn m3_context_cost_compaction_and_prompt_dump_use_the_headless_protocol() {
    let root = tempdir().expect("root");
    let run = TestRun::new(&root, "m3-headless");
    run.write_agents();
    let first_script = root.path().join("first-turn.json");
    write_script(&first_script, text_script("FIRST_CONTEXT_TOKEN"));
    let first = base_command(&run.workspace, &run.home)
        .args([
            "-p",
            "FIRST_USER_PROMPT_TOKEN",
            "--output-format",
            "stream-json",
            "--in-memory-replay-script",
            first_script.to_str().expect("first script"),
        ])
        .output()
        .expect("first turn");
    assert!(
        first.status.success(),
        "first stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let session_id = only_session_id(&run.home);

    let second_script = root.path().join("second-turn.json");
    write_script(&second_script, text_script("SECOND_CONTEXT_TOKEN"));
    let second = base_command(&run.workspace, &run.home)
        .args([
            "-p",
            "SECOND_USER_PROMPT_TOKEN",
            "--resume",
            &session_id,
            "--output-format",
            "stream-json",
            "--in-memory-replay-script",
            second_script.to_str().expect("second script"),
        ])
        .output()
        .expect("second turn");
    assert!(
        second.status.success(),
        "second stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    let historical = base_command(&run.workspace, &run.home)
        .args(["prompt", "dump", "--turn", "1", "--resume", &session_id])
        .output()
        .expect("historical prompt dump");
    assert!(
        historical.status.success(),
        "historical dump stderr: {}",
        String::from_utf8_lossy(&historical.stderr)
    );
    let historical_text = String::from_utf8(historical.stdout).expect("historical UTF-8");
    assert!(historical_text.contains("FIRST_USER_PROMPT_TOKEN"));
    assert!(!historical_text.contains("SECOND_USER_PROMPT_TOKEN"));
    let prompt_shapes = run
        .home
        .join("sessions")
        .join(&session_id)
        .join("prompt-shapes.json");
    let prompt_shapes_backup = prompt_shapes.with_extension("json.backup");
    fs::rename(&prompt_shapes, &prompt_shapes_backup).expect("hide prompt-shape metadata");
    let missing_shape = base_command(&run.workspace, &run.home)
        .args(["prompt", "dump", "--turn", "1", "--resume", &session_id])
        .output()
        .expect("missing-shape prompt dump");
    assert!(!missing_shape.status.success());
    assert!(
        String::from_utf8_lossy(&missing_shape.stderr)
            .contains("exact request shape is unavailable for historical turn 1")
    );
    fs::rename(&prompt_shapes_backup, &prompt_shapes).expect("restore prompt-shape metadata");
    let latest = base_command(&run.workspace, &run.home)
        .args(["prompt", "dump"])
        .output()
        .expect("latest prompt dump");
    assert!(
        latest.status.success(),
        "latest dump stderr: {}",
        String::from_utf8_lossy(&latest.stderr)
    );
    assert!(
        String::from_utf8(latest.stdout)
            .expect("latest UTF-8")
            .contains("SECOND_USER_PROMPT_TOKEN")
    );

    let empty_script = root.path().join("no-provider.json");
    write_script(&empty_script, Vec::new());
    let context = run_m3_command(&run, &session_id, "/context", &empty_script);
    let context_events = parse_stream(&context.stdout);
    let snapshot = context_events
        .iter()
        .find_map(|event| match event {
            EngineEvent::ContextSnapshotReady { snapshot, .. } => Some(snapshot.clone()),
            _ => None,
        })
        .expect("context snapshot");
    let user_item = snapshot
        .items
        .iter()
        .find(|item| item.label.starts_with("User") && !item.state.evicted)
        .expect("user context item")
        .item_id
        .0
        .clone();
    let assistant_item = snapshot
        .items
        .iter()
        .find(|item| item.label.starts_with("Assistant") && !item.state.evicted)
        .expect("assistant context item")
        .item_id
        .0
        .clone();

    let evict = run_m3_command(
        &run,
        &session_id,
        &format!("/context evict {user_item}"),
        &empty_script,
    );
    assert!(
        evict.status.success(),
        "evict stderr: {}",
        String::from_utf8_lossy(&evict.stderr)
    );
    assert_ack_precedes_cause(&parse_stream(&evict.stdout), |event| {
        matches!(event, EngineEvent::ContextItemEvicted { .. })
    });
    let pin = run_m3_command(
        &run,
        &session_id,
        &format!("/context pin {assistant_item}"),
        &empty_script,
    );
    assert!(
        pin.status.success(),
        "pin stderr: {}",
        String::from_utf8_lossy(&pin.stderr)
    );
    assert_ack_precedes_cause(&parse_stream(&pin.stdout), |event| {
        matches!(event, EngineEvent::ContextItemPinned { .. })
    });

    let surgically_changed = base_command(&run.workspace, &run.home)
        .args(["prompt", "dump", "--resume", &session_id])
        .output()
        .expect("prompt dump after surgery");
    assert!(surgically_changed.status.success());
    let changed_text = String::from_utf8(surgically_changed.stdout).expect("changed UTF-8");
    assert!(!changed_text.contains("FIRST_USER_PROMPT_TOKEN"));
    assert!(changed_text.contains("FIRST_CONTEXT_TOKEN"));

    let cost = run_m3_command(&run, &session_id, "/cost", &empty_script);
    assert!(
        cost.status.success(),
        "cost stderr: {}",
        String::from_utf8_lossy(&cost.stderr)
    );
    let cost_events = parse_stream(&cost.stdout);
    let costs = cost_events
        .iter()
        .find_map(|event| match event {
            EngineEvent::CostSnapshotReady { snapshot, .. } => Some(snapshot.clone()),
            _ => None,
        })
        .expect("cost snapshot");
    assert_eq!(costs.turns.len(), 2);
    assert!(
        costs
            .turns
            .iter()
            .all(|turn| matches!(&turn.cost, rw_core::Cost::Unavailable { .. }))
    );
    assert!(!costs.session_monetary_accounting_complete);
    assert!(!costs.daily_monetary_accounting_complete);
    assert_eq!(costs.session_cost_unavailable_entries, 2);
    assert_eq!(costs.daily_cost_unavailable_entries, 2);

    let rewind = run_m3_command(&run, &session_id, "/rewind 2", &empty_script);
    assert!(
        rewind.status.success(),
        "rewind stderr: {}",
        String::from_utf8_lossy(&rewind.stderr)
    );
    let rewound_dump = base_command(&run.workspace, &run.home)
        .args(["prompt", "dump", "--resume", &session_id])
        .output()
        .expect("rewound prompt dump");
    assert!(rewound_dump.status.success());
    assert!(
        String::from_utf8(rewound_dump.stdout)
            .expect("rewound UTF-8")
            .contains("FIRST_USER_PROMPT_TOKEN")
    );

    let compact_script = root.path().join("compact.json");
    write_script(
        &compact_script,
        text_script("Goal\nInstructions\nDiscoveries\nAccomplished\nRelevant files"),
    );
    let compact = run_m3_command(
        &run,
        &session_id,
        "/compact retain the test intent",
        &compact_script,
    );
    assert!(
        compact.status.success(),
        "compact stderr: {}",
        String::from_utf8_lossy(&compact.stderr)
    );
    let compact_events = parse_stream(&compact.stdout);
    assert_ack_precedes_cause(&compact_events, |event| {
        matches!(event, EngineEvent::CompactionStarted { .. })
    });
    assert!(compact_events.iter().any(|event| matches!(
        event,
        EngineEvent::CompactionFinished {
            usage: Some(_),
            cost: Some(rw_core::Cost::Unavailable { .. }),
            ..
        }
    )));
}

#[test]
fn zero_turn_anthropic_prompt_dump_uses_static_cache_shape_without_auth() {
    let root = tempdir().expect("root");
    let run = TestRun::new(&root, "anthropic-prompt-dump");
    run.write_agents();
    fs::write(
        run.home.join("config.toml"),
        "[models]\n\
         default = \"fast\"\n\
         [models.aliases]\n\
         fast = [\"anthropic/claude-fixture\"]\n\
         [providers.anthropic]\n\
         kind = \"anthropic\"\n",
    )
    .expect("Anthropic inspection config");
    let output = base_command(&run.workspace, &run.home)
        .args(["prompt", "dump"])
        .output()
        .expect("zero-turn Anthropic prompt dump");
    assert!(
        output.status.success(),
        "prompt dump stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dump: rw_core::PromptDump =
        serde_json::from_slice(&output.stdout).expect("prompt dump JSON");
    assert_eq!(dump.model_alias.0, "fast");
    assert_eq!(dump.cache_breakpoints.len(), 1);
    assert!(
        dump.tools
            .iter()
            .any(|tool| { tool.name == "read" && tool.input_schema.get("properties").is_some() })
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn budget_warning_rate_alarm_and_hard_cap_are_enforced_before_provider_dispatch() {
    let root = tempdir().expect("root");
    let run = TestRun::new(&root, "m3-budget");
    run.write_agents();
    let initial_script = root.path().join("budget-initial.json");
    write_script(&initial_script, text_script("initial billed turn"));
    let initial = base_command(&run.workspace, &run.home)
        .args([
            "-p",
            "create a billed history entry",
            "--in-memory-replay-script",
            initial_script.to_str().expect("initial script"),
        ])
        .output()
        .expect("initial budget turn");
    assert!(initial.status.success());
    let session_id = only_session_id(&run.home);
    rewrite_turn_cost(&run.home, &session_id, 100);
    remove_derived_index(&run.home);

    fs::write(
        run.home.join("config.toml"),
        "[budget]\n\
         session_cost_cap_micros_usd = 125\n\
         spend_rate_alarm_micros_usd_per_minute = 50\n\
         warn_at_percent = 80\n",
    )
    .expect("warning budget config");
    let warning_script = root.path().join("budget-warning.json");
    write_script(&warning_script, text_script("PROVIDER_RAN_AFTER_WARNING"));
    let warning = base_command(&run.workspace, &run.home)
        .args([
            "-p",
            "continue below the cap",
            "--resume",
            &session_id,
            "--output-format",
            "stream-json",
            "--in-memory-replay-script",
            warning_script.to_str().expect("warning script"),
        ])
        .output()
        .expect("warning turn");
    assert!(
        !warning.status.success(),
        "unpriced current-turn usage must fail closed under a dollar cap"
    );
    assert!(
        String::from_utf8_lossy(&warning.stderr).contains("BudgetExceeded"),
        "warning stderr: {}",
        String::from_utf8_lossy(&warning.stderr)
    );
    let warning_events = parse_stream(&warning.stdout);
    assert!(warning_events.iter().any(|event| matches!(
        event,
        EngineEvent::BudgetStatusChanged { level, .. }
            if format!("{level:?}") == "Warning"
    )));
    assert!(warning_events.iter().any(|event| matches!(
        event,
        EngineEvent::BudgetStatusChanged { level, .. }
            if format!("{level:?}") == "SpendRateAlarm"
    )));
    assert!(warning_events.iter().any(|event| matches!(
        event,
        EngineEvent::TextDelta { text, .. } if text.contains("PROVIDER_RAN_AFTER_WARNING")
    )));

    fs::write(
        run.home.join("config.toml"),
        "[budget]\n\
         session_cost_cap_micros_usd = 100\n\
         spend_rate_alarm_micros_usd_per_minute = 50\n\
         warn_at_percent = 80\n",
    )
    .expect("hard-cap budget config");
    let blocked_script = root.path().join("budget-blocked.json");
    write_script(&blocked_script, text_script("PROVIDER_MUST_NOT_RUN_AT_CAP"));
    let blocked = base_command(&run.workspace, &run.home)
        .args([
            "-p",
            "must stop before provider dispatch",
            "--resume",
            &session_id,
            "--output-format",
            "stream-json",
            "--in-memory-replay-script",
            blocked_script.to_str().expect("blocked script"),
        ])
        .output()
        .expect("hard-cap turn");
    assert!(!blocked.status.success(), "hard cap must fail print mode");
    let blocked_events = parse_stream(&blocked.stdout);
    assert!(blocked_events.iter().any(|event| matches!(
        event,
        EngineEvent::BudgetStatusChanged { level, .. }
            if format!("{level:?}") == "HardCap"
    )));
    assert!(blocked_events.iter().any(|event| matches!(
        event,
        EngineEvent::TurnFinished {
            status: TurnStatus::BudgetExceeded,
            ..
        }
    )));
    assert!(!blocked_events.iter().any(|event| matches!(
        event,
        EngineEvent::TextDelta { text, .. } if text.contains("PROVIDER_MUST_NOT_RUN_AT_CAP")
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
            .args(["-9", &first.id().to_string()])
            .status()
            .expect("kill first writer")
            .success()
    );
    assert!(!first.wait().expect("first writer wait").success());
}

#[test]
fn real_process_executes_glob_and_read_without_a_stalled_approval_channel() {
    let root = tempdir().expect("root");
    let script = root.path().join("read-tools.json");
    write_script(
        &script,
        vec![
            vec![
                ProviderEvent::ToolCallStart {
                    id: "glob-rust".to_owned(),
                    name: "glob".to_owned(),
                },
                ProviderEvent::ToolCallEnd {
                    id: "glob-rust".to_owned(),
                    arguments: json!({"pattern": "**/*.rs", "path": "."}),
                },
                ProviderEvent::ToolCallStart {
                    id: "read-rust".to_owned(),
                    name: "read".to_owned(),
                },
                ProviderEvent::ToolCallEnd {
                    id: "read-rust".to_owned(),
                    arguments: json!({"path": "src/lib.rs"}),
                },
                ProviderEvent::Finished {
                    reason: FinishReason::ToolCalls,
                },
            ],
            text_events("read tools completed"),
        ],
    );

    for (name, permission_args) in [
        (
            "auto-safe-read-tools",
            vec!["--permission-mode", "auto-safe"],
        ),
        ("yolo-read-tools", vec!["--permission-mode", "yolo"]),
        ("trusted-read-tools", vec!["--dangerously-trust"]),
    ] {
        let run = TestRun::new(&root, name);
        fs::create_dir_all(run.workspace.join("src")).expect("source directory");
        fs::write(
            run.workspace.join("src/lib.rs"),
            "pub const TOOL_EXECUTION_CANARY: &str = \"visible\";\n",
        )
        .expect("source fixture");
        let mut command = base_command(&run.workspace, &run.home);
        command.args([
            "-p",
            "inspect the Rust source with glob and read",
            "--output-format",
            "stream-json",
            "--in-memory-replay-script",
            script.to_str().expect("script path"),
        ]);
        command.args(permission_args);
        let output = command.output().expect("rw process");
        assert!(
            output.status.success(),
            "{name} stderr: {}\nstdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout),
        );
        let events = parse_stream(&output.stdout);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EngineEvent::ToolCallStarted { .. }))
                .count(),
            2,
            "{name}: missing visible tool starts: {events:#?}",
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    EngineEvent::ToolCallFinished {
                        is_error: false,
                        ..
                    }
                ))
                .count(),
            2,
            "{name}: tools did not finish successfully: {events:#?}",
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("TOOL_EXECUTION_CANARY"),
            "{name}: read output was not projected",
        );
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, EngineEvent::ToolApprovalNeeded { .. }))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::TurnFinished {
                status: TurnStatus::Completed,
                ..
            }
        )));
    }
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
    #[cfg(not(target_os = "linux"))]
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
    #[cfg(not(target_os = "linux"))]
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
        fs::create_dir_all(&workspace).expect("workspace");
        let home = private_test_directory(&root.path().join(format!("{name}-home")));
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
    let home = private_test_directory(home);
    let mut command = Command::new(env!("CARGO_BIN_EXE_rw"));
    command
        .env_clear()
        .current_dir(workspace)
        .env("HOME", &home)
        .env("ROTTWEILER_HOME", &home)
        .env("ROTTWEILER_CREDENTIAL_BACKEND", "file");
    command
}

fn private_test_directory(path: &Path) -> PathBuf {
    fs::create_dir_all(path).expect("private test directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("private test directory permissions");
    }
    fs::canonicalize(path).expect("canonical private test directory")
}

fn run_m3_command(
    run: &TestRun,
    session_id: &str,
    prompt: &str,
    script: &Path,
) -> std::process::Output {
    base_command(&run.workspace, &run.home)
        .args([
            "-p",
            prompt,
            "--resume",
            session_id,
            "--output-format",
            "stream-json",
            "--in-memory-replay-script",
            script.to_str().expect("M3 script path"),
        ])
        .output()
        .expect("M3 command")
}

fn assert_ack_precedes_cause(events: &[EngineEvent], is_target: impl Fn(&EngineEvent) -> bool) {
    let (target_index, cause) = events
        .iter()
        .enumerate()
        .find_map(|(index, event)| {
            is_target(event).then(|| {
                (
                    index,
                    event
                        .meta()
                        .and_then(|meta| meta.caused_by.clone())
                        .expect("durable command event cause"),
                )
            })
        })
        .expect("target command event");
    let ack_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                EngineEvent::CommandAcknowledged { meta, .. } if meta.request_id == cause
            )
        })
        .unwrap_or_else(|| panic!("matching command acknowledgement in {events:#?}"));
    assert!(
        ack_index < target_index,
        "acknowledgement must precede cause"
    );
}

fn text_script(text: &str) -> Vec<Vec<ProviderEvent>> {
    vec![text_events(text)]
}

fn text_events(text: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta {
            text: text.to_owned(),
        },
        ProviderEvent::Finished {
            reason: FinishReason::Stop,
        },
    ]
}

fn init_git_repository(workspace: &Path) {
    git_output(workspace, &["init", "--quiet"]);
    fs::write(workspace.join("tracked.txt"), "base\n").expect("tracked file");
    git_output(workspace, &["add", "tracked.txt"]);
    git_output(workspace, &["commit", "--quiet", "-m", "base"]);
}

fn git_output(workspace: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Rottweiler Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Rottweiler Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git UTF-8")
        .trim()
        .to_owned()
}

fn rewrite_turn_cost(home: &Path, session_id: &str, amount_micros: u64) {
    let path = home.join("sessions").join(session_id).join("events.jsonl");
    let source = fs::read_to_string(&path).expect("event log before cost rewrite");
    let mut rewritten = String::new();
    let mut found = false;
    for line in source.lines() {
        let mut envelope: serde_json::Value = serde_json::from_str(line).expect("event envelope");
        if envelope["event"]["type"] == "turn_finished" {
            envelope["event"]["cost"] = json!({
                "kind": "monetary",
                "amount_micros": amount_micros.to_string(),
                "currency": "USD"
            });
            found = true;
        }
        rewritten.push_str(&serde_json::to_string(&envelope).expect("rewritten envelope"));
        rewritten.push('\n');
    }
    assert!(found, "fixture must contain a completed turn");
    fs::write(path, rewritten).expect("rewritten event log");
}

fn remove_derived_index(home: &Path) {
    for name in ["index.sqlite", "index.sqlite-wal", "index.sqlite-shm"] {
        match fs::remove_file(home.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("derived index removal failed: {error}"),
        }
    }
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
