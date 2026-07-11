#![allow(clippy::expect_used)]

#[cfg(target_os = "linux")]
fn main() {
    use std::ffi::OsString;
    use std::path::Path;
    use std::process::Command;

    use rw_sandbox::{NetworkPolicy, SandboxPolicy, maybe_run_helper, shell_launch_plan};

    if maybe_run_helper(std::env::args_os()).expect("sandbox helper dispatch") {
        unreachable!("sandbox helper replaces the process")
    }

    let workspace = tempfile::tempdir().expect("workspace");
    let policy =
        SandboxPolicy::new([workspace.path()], NetworkPolicy::Deny).expect("sandbox policy");
    let executable = std::env::current_exe().expect("current executable");
    let shell_args = [OsString::from("-c"), OsString::from("placeholder")];
    let mut plan = shell_launch_plan(&policy, &executable, Path::new("/bin/sh"), &shell_args)
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
    *plan.args.last_mut().expect("shell script argument") =
        OsString::from(format!("test ! -e /proc/self/fd/{helper_descriptor}"));
    let status = Command::new(&plan.program)
        .args(&plan.args)
        .status()
        .expect("self-hosted helper status");
    assert!(status.success(), "self-hosted helper failed: {status}");
}

#[cfg(not(target_os = "linux"))]
fn main() {}
