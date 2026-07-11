use std::{fs, path::Path, process::Command};

use rw_core::{DEFAULT_INIT_FILE_BUDGET_BYTES, InitDepth, InitError, apply_init_plan, plan_init};
use tempfile::tempdir;

fn run_first_declared_test(workspace: &Path, instructions: &str) -> Result<String, String> {
    let command = instructions
        .split_once("## Test commands\n")
        .and_then(|(_, tests)| tests.lines().find_map(|line| line.strip_prefix("- `")))
        .and_then(|line| line.strip_suffix('`'))
        .ok_or_else(|| "generated instructions did not declare a test command".to_owned())?;
    let arguments = shell_words::split(command)
        .map_err(|error| format!("generated test command could not be parsed: {error}"))?;
    let (program, arguments) = arguments
        .split_first()
        .ok_or_else(|| "generated test command was empty".to_owned())?;
    #[cfg(target_os = "macos")]
    let mut child = {
        let mut child = Command::new("/usr/bin/sandbox-exec");
        child.args([
            "-p",
            "(version 1) (allow default) (deny network-outbound (require-not (remote ip \"localhost:*\")))",
            program,
        ]);
        child.args(arguments);
        child
    };
    #[cfg(not(target_os = "macos"))]
    let mut child = {
        let mut child = Command::new(program);
        child.args(arguments);
        child
    };
    let output = child
        .current_dir(workspace)
        .env("CARGO_NET_OFFLINE", "true")
        .env("npm_config_offline", "true")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("CI", "1")
        .output()
        .map_err(|error| format!("generated test command could not start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "generated test command failed ({command}):\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(command.to_owned())
}

fn must<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) -> T {
    result.unwrap_or_else(|error| panic!("{context}: {error}"))
}

fn copy_fixture(name: &str, destination: &Path) {
    fn copy_tree(source: &Path, destination: &Path) {
        must(fs::create_dir_all(destination), "create fixture directory");
        let mut entries = must(fs::read_dir(source), "read fixture directory");
        let mut entries = must(
            entries.by_ref().collect::<Result<Vec<_>, _>>(),
            "enumerate fixture directory",
        );
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let target = destination.join(entry.file_name());
            if must(entry.file_type(), "inspect fixture entry").is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                must(fs::copy(entry.path(), target), "copy fixture file");
            }
        }
    }
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/init")
            .join(name),
        destination,
    );
}

fn prepare_python_command_canary(workspace: &Path) {
    // Keep planning and generated-instruction assertions pinned to the exact
    // upstream snapshot. Its application test suite is not part of the init
    // contract, though, and imports stdlib modules removed by newer Python
    // releases (notably `cgi` in Python 3.13). Replace only the temporary
    // copy's upstream tests after inference so the exact generated command
    // still has to launch, discover, execute, and pass a real unittest using
    // the runner available on the host.
    must(
        fs::remove_dir_all(workspace.join("test")),
        "remove runtime-sensitive upstream Python tests from temporary fixture",
    );
    must(
        fs::write(
            workspace.join("test_inferred_command.py"),
            r#"import pathlib
import unittest


class GeneratedCommandCanary(unittest.TestCase):
    def test_unittest_discovery_runs_from_the_project_root(self):
        marker = pathlib.Path("tox.ini").read_text(encoding="utf-8")
        self.assertIn("-m unittest discover", marker)
"#,
        ),
        "write Python generated-command canary",
    );
}

#[test]
fn init_golden_fixtures_infer_first_try_test_commands() {
    for (fixture, expected) in [
        ("rust", "cargo test --workspace"),
        ("typescript", "npm test"),
        ("python", "python -m unittest discover"),
    ] {
        let root = must(tempdir(), "create tempdir");
        copy_fixture(fixture, root.path());
        let first = must(
            plan_init(root.path(), InitDepth::Root, DEFAULT_INIT_FILE_BUDGET_BYTES),
            "plan init",
        );
        let second = must(
            plan_init(root.path(), InitDepth::Root, DEFAULT_INIT_FILE_BUDGET_BYTES),
            "repeat init plan",
        );
        assert_eq!(first.files(), second.files(), "{fixture} must be stable");
        assert!(first.files()[Path::new("AGENTS.md")].contains(&format!("`{expected}`")));
        must(apply_init_plan(&first), "apply init plan");
        assert_eq!(
            must(
                fs::read_to_string(root.path().join("AGENTS.md")),
                "read generated instructions"
            ),
            first.files()[Path::new("AGENTS.md")]
        );
        let loaded = rw_core::load_root_project_instructions(root.path())
            .unwrap_or_else(|error| panic!("load generated instructions: {error}"))
            .unwrap_or_else(|| panic!("fresh session loads generated AGENTS.md"));
        assert!(loaded.content().contains(&format!("`{expected}`")));
        if fixture == "python" {
            prepare_python_command_canary(root.path());
        }
        let executed = must(
            run_first_declared_test(root.path(), loaded.content()),
            "run first generated test command",
        );
        assert_eq!(executed, expected);

        let provenance = must(
            fs::read_to_string(root.path().join("PROVENANCE.md")),
            "read snapshot provenance",
        );
        assert!(provenance.contains("Upstream: https://github.com/"));
        assert!(provenance.contains("Revision: `"));
        assert!(provenance.contains("Git tree: `"));
        assert!(
            root.path().join("LICENSE").is_file()
                || root.path().join("LICENSE.txt").is_file()
                || root.path().join("LICENSE-MIT").is_file()
        );
    }
}

#[test]
fn deep_init_indexes_packages_skips_generated_and_respects_each_budget() {
    let root = must(tempdir(), "create tempdir");
    copy_fixture("typescript", root.path());
    must(
        fs::create_dir_all(root.path().join("node_modules/ignored")),
        "create generated fixture directory",
    );
    must(
        fs::write(
            root.path().join("node_modules/ignored/package.json"),
            r#"{"name":"must-not-be-discovered"}"#,
        ),
        "write generated fixture marker",
    );
    let plan = must(
        plan_init(root.path(), InitDepth::Deep, DEFAULT_INIT_FILE_BUDGET_BYTES),
        "plan deep init",
    );
    assert!(
        plan.files()
            .contains_key(Path::new("apps/browser/AGENTS.md"))
    );
    assert!(
        plan.files()
            .contains_key(Path::new("packages/engine/AGENTS.md"))
    );
    assert!(
        !plan
            .files()
            .contains_key(Path::new("node_modules/ignored/AGENTS.md"))
    );
    assert!(
        plan.skipped_directories()
            .contains(&Path::new("node_modules").to_path_buf())
    );
    for content in plan.files().values() {
        assert!(content.len() <= DEFAULT_INIT_FILE_BUDGET_BYTES);
    }
    let root_instructions = &plan.files()[Path::new("AGENTS.md")];
    assert!(root_instructions.contains("apps/browser/AGENTS.md"));
    assert!(root_instructions.contains("packages/engine/AGENTS.md"));
}

#[test]
fn init_never_overwrites_human_owned_agents_file() {
    let root = must(tempdir(), "create tempdir");
    copy_fixture("python", root.path());
    must(
        fs::write(root.path().join("AGENTS.md"), "human guidance"),
        "write human instructions",
    );
    let plan = must(
        plan_init(root.path(), InitDepth::Root, DEFAULT_INIT_FILE_BUDGET_BYTES),
        "plan init",
    );
    assert!(matches!(
        apply_init_plan(&plan),
        Err(InitError::ExistingInstructions { .. })
    ));
    assert_eq!(
        must(
            fs::read_to_string(root.path().join("AGENTS.md")),
            "read human instructions"
        ),
        "human guidance"
    );
}

#[test]
fn generated_file_budget_is_enforced_before_writes() {
    let root = must(tempdir(), "create tempdir");
    copy_fixture("rust", root.path());
    assert!(matches!(
        plan_init(root.path(), InitDepth::Root, 32),
        Err(InitError::GeneratedFileTooLarge { limit: 32, .. })
    ));
    assert!(!root.path().join("AGENTS.md").exists());
}

#[test]
fn generated_files_are_byte_rewindable_from_the_planned_checkpoint_scope() {
    let root = must(tempdir(), "create tempdir");
    let storage = must(tempdir(), "create checkpoint storage");
    copy_fixture("typescript", root.path());
    let plan = must(
        plan_init(root.path(), InitDepth::Deep, DEFAULT_INIT_FILE_BUDGET_BYTES),
        "plan deep init",
    );
    let store = must(
        rw_store::checkpoint::CheckpointStore::open(storage.path(), root.path()),
        "open checkpoint store",
    );
    must(
        store.checkpoint_known("init-session", 1, plan.files().keys().cloned()),
        "checkpoint generated paths",
    );
    let created = must(apply_init_plan(&plan), "apply init plan");
    assert!(created.iter().all(|path| root.path().join(path).is_file()));
    let handle = must(
        store.prepare_rewind("init-session", 0, "rewind-init"),
        "prepare init rewind",
    );
    must(store.apply_rewind(&handle), "apply init rewind");
    must(store.acknowledge_rewind(&handle), "ack init rewind");
    assert!(created.iter().all(|path| !root.path().join(path).exists()));
}
