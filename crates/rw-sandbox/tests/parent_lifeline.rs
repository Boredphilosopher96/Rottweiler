#![cfg(unix)]
#![allow(clippy::expect_used)]
//! The real helper keeps executing cleanup after its Rust owner receives `SIGKILL`.
mod common;
use rw_sandbox::{NetworkPolicy, PluginRendezvous, SandboxPolicy, shell_launch_plan};
use std::{
    fs,
    io::{BufRead as _, BufReader},
    os::unix::process::CommandExt as _,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn controller() {
    let Some(root) = std::env::var_os("RW_LIFELINE_FIXTURE_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let helper = common::helper();
    let policy = SandboxPolicy::new([root], NetworkPolicy::Deny)
        .expect("policy")
        .without_process_creation();
    let body = if std::env::var_os("RW_LIFELINE_NO_GRANT").is_some() {
        "printf forbidden; exit 9"
    } else {
        "printf '%s\\n' \"$$\"; while :; do :; done"
    };
    let mut plan = shell_launch_plan(
        &policy,
        &helper,
        Path::new("/bin/sh"),
        &["-c".into(), body.into()],
    )
    .expect("sandbox plan");
    let rendezvous = PluginRendezvous::bind().expect("private rendezvous");
    rendezvous.wrap(&mut plan).expect("supervised plan");
    let mut child = Command::new(&plan.program)
        .args(&plan.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .expect("helper");
    let mut control = rendezvous
        .accept(
            child.id(),
            std::time::Instant::now() + std::time::Duration::from_secs(5),
            &|| false,
        )
        .expect("verified connection");
    if std::env::var_os("RW_LIFELINE_NO_GRANT").is_some() {
        drop(control);
        let output = child.wait_with_output().expect("ungranted helper retires");
        assert!(output.stdout.is_empty(), "effect ran before grant");
        return;
    }
    control.grant().expect("grant");
    let mut pid = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut pid)
        .expect("effect identity");
    assert!(!pid.trim().is_empty(), "effect published readiness");
    fs::write(root.join("ready"), child.id().to_string()).expect("ready receipt");
    child.wait().expect("controller owns the supervisor wait");
    drop(control);
}

struct Controller(Child);
impl Drop for Controller {
    fn drop(&mut self) {
        use rustix::process::{Pid, WaitId, WaitIdOptions};
        let pid = Pid::from_child(&self.0);
        if rustix::process::waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .is_ok()
        {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
fn start(root: &Path, no_grant: bool) -> Controller {
    let mut command = Command::new(std::env::current_exe().expect("test binary"));
    command
        .args(["--exact", "controller", "--nocapture"])
        .env("RW_LIFELINE_FIXTURE_ROOT", root)
        .env("TMPDIR", root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if no_grant {
        command.env("RW_LIFELINE_NO_GRANT", "1");
    }
    Controller(command.spawn().expect("Rust owner controller"))
}

#[test]
fn parent_sigkill_retires_real_single_process_sandbox() {
    let root = tempfile::tempdir().expect("fixture root");
    let mut controller = start(root.path(), false);
    let deadline = Instant::now() + Duration::from_secs(15);
    let pids = loop {
        if let Ok(value) = fs::read_to_string(root.path().join("ready")) {
            break value;
        }
        assert!(
            Instant::now() < deadline,
            "controller failed to start supervised sandbox"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let pid = rustix::process::Pid::from_raw(pids.parse().expect("supervisor pid")).expect("pid");
    controller.0.kill().expect("SIGKILL only Rust owner");
    controller.0.wait().expect("reap Rust owner");
    let deadline = Instant::now() + Duration::from_secs(5);
    while rustix::process::test_kill_process_group(pid) != Err(rustix::io::Errno::SRCH) {
        assert!(
            Instant::now() < deadline,
            "supervisor group remained after parent loss: {pid}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn connection_loss_before_grant_does_not_execute_effect() {
    let root = tempfile::tempdir().expect("fixture root");
    let mut controller = start(root.path(), true);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = controller.0.try_wait().expect("controller status") {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ungranted controller did not retire"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn normal_child_exit_preserves_nonzero_status() {
    let helper = common::helper();
    let root = tempfile::tempdir().expect("scratch");
    let policy = SandboxPolicy::new([root.path()], NetworkPolicy::Deny)
        .expect("policy")
        .without_process_creation();
    let mut plan = shell_launch_plan(
        &policy,
        &helper,
        Path::new("/bin/sh"),
        &["-c".into(), "exit 23".into()],
    )
    .expect("plan");
    let rendezvous = PluginRendezvous::bind().expect("rendezvous");
    rendezvous.wrap(&mut plan).expect("supervisor");
    let child = Command::new(&plan.program)
        .args(&plan.args)
        .process_group(0)
        .spawn()
        .expect("helper");
    let mut owner = Controller(child);
    let mut control = rendezvous
        .accept(
            owner.0.id(),
            std::time::Instant::now() + std::time::Duration::from_secs(5),
            &|| false,
        )
        .expect("connection");
    control.grant().expect("grant");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = owner.0.try_wait().expect("child status") {
            assert_eq!(status.code(), Some(23));
            control
                .verify_settlement()
                .expect("effect retirement receipt");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "normal supervisor exit timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn long_temporary_directory_preserves_supervised_exit() {
    let root = tempfile::tempdir_in("/tmp").expect("fixture root");
    let temporary = root.path().join("x".repeat(120));
    fs::create_dir(&temporary).expect("valid long temporary directory");
    let child = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "normal_child_exit_preserves_nonzero_status",
            "--nocapture",
        ])
        .env("TMPDIR", &temporary)
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("isolated temporary-directory environment");
    let mut owner = Controller(child);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = owner.0.try_wait().expect("controller status") {
            assert!(status.success(), "supervised exit under long TMPDIR failed");
            break;
        }
        assert!(Instant::now() < deadline, "controller did not retire");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn supervisor_refuses_a_process_creating_policy() {
    let helper = common::helper();
    let root = tempfile::tempdir().expect("scratch");
    let policy = SandboxPolicy::new([root.path()], NetworkPolicy::Deny).expect("policy");
    let mut plan = shell_launch_plan(&policy, &helper, Path::new("/bin/sh"), &[]).expect("plan");
    assert!(
        PluginRendezvous::bind()
            .expect("rendezvous")
            .wrap(&mut plan)
            .is_err()
    );
}
