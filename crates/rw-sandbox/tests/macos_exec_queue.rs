#![cfg(target_os = "macos")]
#![allow(clippy::expect_used)]
//! Parent-held queue identity proves exec discards pending transferred authority.
mod common;
use rw_sandbox::{NetworkPolicy, SandboxPolicy, shell_launch_plan};
use std::{path::Path, process::Command};

#[test]
fn actual_exec_discards_receive_queue_with_pending_effect_right() {
    let resolved = Command::new("python3")
        .args([
            "-c",
            "import os,sys; print(os.path.realpath(sys.executable))",
        ])
        .output()
        .expect("resolve interpreter");
    assert!(resolved.status.success());
    let interpreter = String::from_utf8(resolved.stdout).expect("interpreter path");
    let interpreter = interpreter.trim();
    let directory = tempfile::tempdir().expect("scratch");
    let policy = SandboxPolicy::new([directory.path()], NetworkPolicy::Deny)
        .expect("policy")
        .without_process_creation();
    let plan = shell_launch_plan(&policy, &common::helper(), Path::new(interpreter), &[])
        .expect("worker plan");
    let mut command = vec![plan.program.to_string_lossy().into_owned()];
    command.extend(
        plan.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned()),
    );
    let encoded = serde_json::to_string(&command).expect("launch plan");
    let output = Command::new(interpreter)
        .args(["-c", include_str!("fixtures/macos_exec_queue.py"), &encoded])
        .output()
        .expect("Mach queue probe");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{}", String::from_utf8_lossy(&output.stdout));
}
