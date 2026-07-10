#![allow(clippy::expect_used)]

use std::fs;
use std::process::Command;

use tempfile::tempdir;

#[test]
fn config_check_prints_values_and_leaf_provenance() {
    let root = tempdir().expect("temporary directory should be created");
    let output = Command::new(env!("CARGO_BIN_EXE_rw"))
        .current_dir(root.path())
        .env("ROTTWEILER_HOME", root.path().join("user"))
        .args([
            "config",
            "check",
            "--set",
            "models.default=test-fast",
            "--set",
            "compaction.reserved=4096",
            "--set",
            "budget.session_cost_cap_micros_usd=250000",
        ])
        .output()
        .expect("rw config check should run");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("models.default = \"test-fast\" [cli]"));
    assert!(stdout.contains("compaction.reserved = 4096 [cli]"));
    assert!(stdout.contains("budget.session_cost_cap_micros_usd = 250000 [cli]"));
    assert!(stdout.contains("permissions.default = ask [built-in]"));
}

#[test]
fn invalid_toml_exits_nonzero_with_actionable_diagnostic() {
    let root = tempdir().expect("temporary directory should be created");
    let user_root = root.path().join("user");
    fs::create_dir_all(&user_root).expect("user config directory should be created");
    fs::write(user_root.join("config.toml"), "unknown = true")
        .expect("invalid config should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_rw"))
        .current_dir(root.path())
        .env("ROTTWEILER_HOME", &user_root)
        .args(["config", "check"])
        .output()
        .expect("rw config check should run");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("invalid configuration"));
    assert!(stderr.contains("unknown field"));
}
