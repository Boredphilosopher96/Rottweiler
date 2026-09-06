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

use rw_core::{EngineEvent, TurnStatus};
use rw_providers::{FinishReason, ProviderEvent};
use rw_tools::{SandboxSupport, probe_sandbox};
use serde_json::json;
use tempfile::{TempDir, tempdir};

const PROMPT: &str = "create hello.py that prints hi, run it";
const STEERING: &str = "STEER_TOKEN_M2_CLI";

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
        .env("ROTTWEILER_HOME", &home);
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
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
    let path = home
        .join("sessions")
        .join(session_id)
        .join("journal")
        .join("active.jsonl");
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

fn read_session_journal(home: &Path, session: &str) -> String {
    let envelopes =
        rw_store::session::SessionEventLog::load_existing::<serde_json::Value>(home, session)
            .expect("offline journal");
    serde_json::to_string(&envelopes).expect("journal JSON")
}

fn event_log(home: &Path) -> Option<PathBuf> {
    let session = fs::read_dir(home.join("sessions"))
        .ok()?
        .find_map(Result::ok)?;
    Some(session.path().join("journal").join("active.jsonl"))
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

#[path = "agent_runtime/execution.rs"]
mod execution;
#[path = "agent_runtime/sessions.rs"]
mod sessions;

#[path = "agent_runtime/process.rs"]
mod process;
use process::TestProcess;
