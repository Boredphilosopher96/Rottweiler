#![allow(clippy::expect_used)]
//! Exercise the actual native sandbox, including the Linux proxy worker bootstrap.
#[cfg(unix)]
mod common;
use rw_sandbox::{
    EgressPolicy, NetworkPolicy, SandboxPolicy, SandboxSupport, SupervisedEgressProxy,
    maybe_run_helper, probe, shell_launch_plan,
};
use std::{ffi::OsString, process::Command};

fn main() {
    if maybe_run_helper(std::env::args_os()).expect("internal helper") {
        return;
    }
    match std::env::args().nth(1).as_deref() {
        #[cfg(target_os = "macos")]
        Some("--replace-running-image") => {
            replaced_running_image();
            return;
        }
        Some("--probe-child") => {
            probe_child();
            return;
        }
        Some("--forbidden-child") => panic!("sandbox allowed a native child process"),
        _ => {}
    }
    let capability = probe();
    if capability.support != SandboxSupport::Enforced {
        assert!(
            std::env::var_os("ROTTWEILER_REQUIRE_LINUX_SANDBOX").is_none(),
            "required sandbox unavailable: {capability:?}"
        );
        eprintln!("sandbox unavailable: {capability:?}");
        return;
    }
    #[cfg(target_os = "macos")]
    verify_running_image_replacement();
    let workspace = tempfile::tempdir().expect("workspace");
    let proxy =
        SupervisedEgressProxy::start(EgressPolicy::new(std::iter::empty::<&str>())).expect("proxy");
    for network in [
        NetworkPolicy::Deny,
        NetworkPolicy::PolicyProxy {
            port: proxy.address().port(),
            relay_path: proxy.relay_path().map(std::path::Path::to_path_buf),
        },
    ] {
        let policy = SandboxPolicy::new([workspace.path()], network)
            .expect("policy")
            .without_process_creation();
        let executable = std::env::current_exe().expect("executable");
        let args = [OsString::from("--probe-child")];
        let plan =
            shell_launch_plan(&policy, &common::helper(), &executable, &args).expect("launch plan");
        #[cfg(target_os = "linux")]
        assert!(
            !plan
                .args
                .iter()
                .any(|arg| arg == "--rw-macos-worker" || arg == "-D"),
            "Linux worker arguments must remain Linux-native"
        );
        let output = Command::new(&plan.program)
            .args(&plan.args)
            .output()
            .expect("sandboxed probe");
        assert!(
            output.status.success(),
            "probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("thread allowed; process denied"));
    }
}

fn probe_child() {
    assert_eq!(std::thread::spawn(|| 42).join().expect("thread"), 42);
    let error = Command::new(std::env::current_exe().expect("executable"))
        .arg("--forbidden-child")
        .spawn()
        .expect_err("native process creation must be denied");
    assert_eq!(
        error.raw_os_error(),
        Some(1),
        "expected EPERM from sandbox: {error}"
    );
    println!("thread allowed; process denied");
}

#[cfg(target_os = "macos")]
fn verify_running_image_replacement() {
    let root = tempfile::tempdir().expect("owned executable directory");
    let executable = root.path().join("running-image");
    std::fs::copy(std::env::current_exe().expect("driver"), &executable).expect("copy driver");
    let result = Command::new(&executable)
        .arg("--replace-running-image")
        .output()
        .expect("running image probe");
    assert!(
        result.status.success(),
        "running image probe failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[cfg(target_os = "macos")]
fn replaced_running_image() {
    use std::os::unix::fs::PermissionsExt as _;
    let executable = std::env::current_exe().expect("driver path");
    let helper =
        rw_sandbox::SandboxHelper::from_running(&executable).expect("kernel image authority");
    std::fs::rename(&executable, executable.with_extension("retired")).expect("move running vnode");
    std::fs::write(&executable, b"#!/bin/sh\nexit 77\n").expect("replacement");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("replacement mode");
    assert!(
        rw_sandbox::SandboxHelper::from_running(&executable).is_err(),
        "installation path must not impersonate running image"
    );
    let policy = SandboxPolicy::new(
        [executable.parent().expect("workspace")],
        NetworkPolicy::Deny,
    )
    .expect("policy")
    .without_process_creation();
    let plan = shell_launch_plan(&policy, &helper, std::path::Path::new("/usr/bin/true"), &[])
        .expect("retained executable plan");
    assert!(
        Command::new(&plan.program)
            .args(&plan.args)
            .status()
            .expect("retained helper")
            .success(),
        "private helper snapshot must survive installation replacement"
    );
}
