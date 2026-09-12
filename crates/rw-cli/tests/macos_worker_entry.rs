#![cfg(target_os = "macos")]
#![allow(clippy::expect_used)]
//! The installed CLI dispatches trusted worker setup before starting Tokio.
use std::process::Command;

#[test]
fn cli_worker_executes_before_runtime_threads_and_normal_entry_still_works() {
    let version = Command::new(env!("CARGO_BIN_EXE_rw"))
        .arg("--version")
        .output()
        .expect("normal CLI entry");
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("rw "));
    let worker = Command::new(env!("CARGO_BIN_EXE_rw"))
        .args(["--rw-macos-worker", "/usr/bin/true"])
        .output()
        .expect("actual CLI worker entry");
    assert!(
        worker.status.success(),
        "{}",
        String::from_utf8_lossy(&worker.stderr)
    );
}
