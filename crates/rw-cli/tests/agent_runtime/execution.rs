use super::*;

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
                "action": "spawn",
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
    let children = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::SubagentFinished { result, .. } => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(children.len(), 3);
    assert!(
        children
            .iter()
            .all(|child| child.status == rw_types::SubagentStatus::Completed),
        "child execution failed before collation; events: {events:#?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::TextDelta { text, .. } if text.contains("collated all three explorers")
        )),
        "parent did not collate the completed children; events: {events:#?}"
    );
    assert!(git_output(&run.workspace, &["diff", "--binary", "HEAD", "--"]).is_empty());
    let status = git_output(&run.workspace, &["status", "--porcelain=v1"]);
    assert!(status.is_empty(), "parent status was not clean: {status:?}");
}

#[test]
fn subagent_control_plane_never_requests_permission_under_strict_policy() {
    let root = tempdir().expect("root");
    let run = TestRun::new(&root, "strict-subagent-control");
    let script = root.path().join("strict-subagent-control.json");
    write_script(
        &script,
        vec![
            vec![
                ProviderEvent::ToolCallStart {
                    id: "spawn-child".to_owned(),
                    name: "spawn_agent".to_owned(),
                },
                ProviderEvent::ToolCallEnd {
                    id: "spawn-child".to_owned(),
                    arguments: json!({
                        "action": "spawn",
                        "task": "inspect without invoking tools",
                        "agent": "explore",
                        "isolation": "shared"
                    }),
                },
                ProviderEvent::Finished {
                    reason: FinishReason::ToolCalls,
                },
            ],
            text_events("child inspection complete"),
            text_events("parent received the child result"),
        ],
    );

    let output = base_command(&run.workspace, &run.home)
        .args([
            "-p",
            "start one child and report its result",
            "--permission-mode",
            "strict",
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
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, EngineEvent::SubagentSpawned { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, EngineEvent::SubagentFinished { .. }))
            .count(),
        1
    );
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(!rendered.contains("approval needed"));
    assert!(!rendered.contains("permission denied"));
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
        EngineEvent::QuestionAnswered { answer, .. }
            if answer.value == "first"
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
    for entry in fs::read_dir(run.home.join("tmp")).expect("CLI temporary directory") {
        let entry = entry.expect("temporary entry");
        assert!(
            !entry.path().join("approved-executable").exists(),
            "normal CLI exit retained an executable snapshot: {}",
            entry.path().display()
        );
    }
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
    assert!(
        !run.workspace.join("../../../added-root").exists(),
        "fixture must distinguish invocation-relative from repository-relative resolution"
    );
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
    // The command can succeed only if `--add-dir` was canonicalized while the
    // nested invocation directory was still current. The same relative path
    // is deliberately absent from the repository-root interpretation above.
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
            .is_some_and(|value| value.contains("**Idle**"))
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
    assert!(
        report["toolDisplay"]["details"]
            .as_str()
            .is_some_and(|text| { text.contains("approval.txt") && text.len() <= 4096 })
    );
    assert_eq!(report["toolSource"]["selector"]["type"], "tool_output");
    assert!(
        report["toolSource"]["sequence"]
            .as_str()
            .is_some_and(|value| value.parse::<u64>().is_ok())
    );
    assert_eq!(report["errors"], json!([]), "{}", report["errorDetails"]);
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
        .join("prompt-shapes.sqlite3");
    let prompt_shapes_backup = prompt_shapes.with_extension("sqlite3.backup");
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
    assert!(costs.subscription_quota.is_none());
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
