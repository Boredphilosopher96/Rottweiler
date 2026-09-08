//! The actual print client resolves a native hook's Ask without terminal input.
use super::{TestRun, base_command, parse_stream, text_events, write_script};
use rw_core::{EngineEvent, TurnStatus};
use rw_providers::{FinishReason, ProviderEvent};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::tempdir;

#[path = "hook_ask/output.rs"]
mod output;
use output::bounded_output;

fn configure_hook(root: &Path, run: &TestRun) {
    let bun = std::env::split_paths(&std::env::var_os("PATH").expect("fixture PATH"))
        .map(|path| path.join("bun"))
        .find(|path| path.is_file())
        .expect("pinned Bun prerequisite")
        .canonicalize()
        .expect("Bun identity");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/plugin-sdk/fixtures/conformance/headless-hook-ask.ts");
    let package = run.workspace.join("hook-package");
    fs::create_dir(&package).expect("package");
    let bundle = package.join("plugin.js");
    bounded_output(
        Command::new(&bun)
            .args(["build", "--target=bun"])
            .arg(source)
            .arg("--outfile")
            .arg(&bundle),
    );
    let manifest_bytes = bounded_output(Command::new(&bun).arg(&bundle).arg("--manifest"));
    let manifest =
        rw_plugin_protocol::PluginManifest::from_slice(&manifest_bytes).expect("SDK manifest");
    let manifest_path = package.join("manifest.json");
    fs::write(&manifest_path, &manifest_bytes).expect("manifest");
    let settings = run.workspace.join(".rottweiler");
    fs::create_dir(&settings).expect("settings");
    let path = settings.join("plugins.toml");
    fs::write(&path, toml::to_string(&json!({"plugins":[{
        "name":manifest.name,"argv":[bun,bundle],"cwd":package,"manifest":manifest_path,"allowed_domains":[]
    }]})).expect("settings TOML")).expect("settings bytes");
    let discovered =
        rw_runtime::executable_config::discover_executable_configs(root, &run.workspace, true)
            .expect("actual executable discovery");
    let plugin = discovered.plugins.first().expect("configured hook");
    let store = rw_runtime::PrivatePluginApprovalStore::open(&run.home).expect("approval store");
    rw_ext::approve_plugin_launch(
        &store,
        &manifest,
        &plugin
            .executable_process_config()
            .expect("pinned launch config"),
        &format!("project:{}", path.display()),
    )
    .expect("exact hook approval");
}

#[test]
fn native_hook_ask_is_denied_by_actual_headless_print_without_mutation() {
    let root = tempdir().expect("fixture");
    let mut run = TestRun::new(&root, "headless-hook-ask");
    run.workspace = run.workspace.canonicalize().expect("canonical project");
    configure_hook(root.path(), &run);
    let script = root.path().join("provider.json");
    write_script(
        &script,
        vec![
            vec![
                ProviderEvent::ToolCallStart {
                    id: "write-attempt".into(),
                    name: "write".into(),
                },
                ProviderEvent::ToolCallEnd {
                    id: "write-attempt".into(),
                    arguments: json!({"path":"forbidden.txt","content":"unapproved mutation"}),
                },
                ProviderEvent::Finished {
                    reason: FinishReason::ToolCalls,
                },
            ],
            text_events("headless denial completed"),
        ],
    );
    let output = bounded_output(
        base_command(&run.workspace, &run.home)
            .args([
                "-p",
                "attempt the write",
                "--permission-mode",
                "yolo",
                "--dangerously-trust",
                "--perf-markers",
                "--output-format",
                "stream-json",
                "--in-memory-replay-script",
            ])
            .arg(&script),
    );
    let events = parse_stream(&output);
    let approval = events.iter().position(|event| matches!(event, EngineEvent::ToolApprovalNeeded { name, .. } if name == "write"))
        .expect("hook Ask overrides otherwise permissive headless policy");
    let denied = events
        .iter()
        .position(|event| matches!(event, EngineEvent::ToolCallFinished { is_error: true, .. }))
        .expect("headless fallback denies instead of waiting for input");
    assert!(approval < denied);
    assert!(!run.workspace.join("forbidden.txt").exists());
    assert!(events.iter().any(|event| matches!(event, EngineEvent::TextDelta { text, .. } if text.contains("headless denial completed"))));
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::TurnFinished {
            status: TurnStatus::Completed,
            ..
        }
    )));
}
