#![allow(clippy::expect_used)]

#[cfg(target_os = "linux")]
fn main() {
    use std::ffi::OsString;
    use std::fs::File;
    use std::path::Path;
    use std::process::Command;

    use rw_sandbox::{
        NetworkPolicy, SandboxPolicy, SandboxSupport, maybe_run_helper, probe, shell_launch_plan,
    };

    if maybe_run_helper(std::env::args_os()).expect("sandbox helper dispatch") {
        unreachable!("sandbox helper replaces the process")
    }

    if std::env::args().nth(1).as_deref() == Some("--replace-running-path") {
        replaced_running_path();
        return;
    }

    let capability = probe();
    if capability.support != SandboxSupport::Enforced {
        assert!(
            std::env::var_os("ROTTWEILER_REQUIRE_LINUX_SANDBOX").is_none(),
            "privileged Linux gate requires sandbox enforcement: {capability:?}"
        );
        eprintln!("skipping helper-driver acceptance: {capability:?}");
        return;
    }

    let held_descriptors = (0..16)
        .map(|_| File::open("/dev/null").expect("held descriptor"))
        .collect::<Vec<_>>();
    assert_eq!(held_descriptors.len(), 16);
    let workspace = tempfile::tempdir().expect("workspace");
    let policy =
        SandboxPolicy::new([workspace.path()], NetworkPolicy::Deny).expect("sandbox policy");
    let executable = std::env::current_exe().expect("current executable");
    let shell_args = [OsString::from("-c"), OsString::from("placeholder")];
    let mut plan = shell_launch_plan(
        &policy,
        &rw_sandbox::SandboxHelper::from_running(&executable).expect("running helper"),
        Path::new("/bin/sh"),
        &shell_args,
    )
    .expect("self-hosted helper launch plan");
    let helper_path = plan
        .args
        .iter()
        .find_map(|argument| {
            argument
                .to_str()
                .and_then(|argument| argument.strip_prefix("/proc/self/fd/"))
        })
        .expect("helper descriptor path");
    let helper_descriptor = helper_path.parse::<u32>().expect("helper descriptor");
    assert!(
        helper_descriptor >= 10,
        "regression requires a multi-digit helper descriptor"
    );
    *plan.args.last_mut().expect("shell script argument") =
        OsString::from(format!("test ! -e /proc/self/fd/{helper_descriptor}"));
    let status = Command::new(&plan.program)
        .args(&plan.args)
        .status()
        .expect("self-hosted helper status");
    assert!(status.success(), "self-hosted helper failed: {status}");
    let copy_root = tempfile::tempdir().expect("owned driver root");
    let copied = copy_root.path().join("rw");
    std::fs::copy(&executable, &copied).expect("owned driver copy");
    let status = Command::new(copied)
        .arg("--replace-running-path")
        .status()
        .expect("identity fixture");
    assert!(
        status.success(),
        "running helper replacement regression: {status}"
    );
}

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
fn replaced_running_path() {
    use rw_sandbox::{NetworkPolicy, SandboxHelper, SandboxPolicy, shell_launch_plan};
    use std::{fs, os::unix::fs::PermissionsExt as _, path::Path, process::Command};
    let running = std::env::current_exe().expect("owned running path");
    let helper = SandboxHelper::from_running(&running).expect("capture running inode");
    fs::rename(&running, running.with_extension("retired")).expect("rename actual running inode");
    fs::write(&running, b"#!/bin/sh\nexit 77\n").expect("replace installation path");
    fs::set_permissions(&running, fs::Permissions::from_mode(0o700))
        .expect("valid replacement mode");
    assert!(
        SandboxHelper::from_running(&running).is_err(),
        "a path lookup must not impersonate the running inode"
    );
    let policy = SandboxPolicy::new([running.parent().expect("owned root")], NetworkPolicy::Deny)
        .expect("policy");
    let plan = shell_launch_plan(&policy, &helper, Path::new("/usr/bin/true"), &[])
        .expect("retained helper plan");
    let status = Command::new(&plan.program)
        .args(&plan.args)
        .status()
        .expect("retained helper launch");
    assert!(
        status.success(),
        "retained running inode must survive path replacement"
    );
}
