#![cfg(target_os = "linux")]
#![allow(clippy::expect_used)]
//! An approved standalone artifact crosses the real namespace bootstrap boundary.
mod common;
use rw_sandbox::{NetworkPolicy, SandboxPolicy, shell_launch_plan};
use std::{ffi::OsString, path::Path, process::Command};

#[test]
fn approved_external_helper_executes_the_code_only_worker() {
    let directory = tempfile::tempdir().expect("scratch");
    let helper = common::helper();
    assert_ne!(
        helper.installation_path(),
        std::env::current_exe().expect("test executable")
    );
    let policy = SandboxPolicy::new([directory.path()], NetworkPolicy::Deny)
        .expect("policy")
        .without_process_creation();
    let plan = shell_launch_plan(
        &policy,
        &helper,
        Path::new("/bin/sh"),
        &[
            OsString::from("-c"),
            OsString::from("printf approved-artifact"),
        ],
    )
    .expect("approved plan");
    let output = Command::new(&plan.program)
        .args(&plan.args)
        .output()
        .expect("namespace launch");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"approved-artifact");
}
