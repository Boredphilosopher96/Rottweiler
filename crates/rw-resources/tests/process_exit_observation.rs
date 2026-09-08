#![cfg(unix)]
#![allow(clippy::expect_used)]

use rustix::process::{Pid, WaitId, WaitIdOptions};
use rw_resources::process::BlockingProcess;
use std::{
    io::Read,
    os::unix::process::ExitStatusExt,
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

fn observe(process: &BlockingProcess) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = process.try_status().expect("observe child") {
            return status;
        }
        assert!(Instant::now() < deadline, "child did not exit");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn assert_waitable(process: &BlockingProcess) -> Pid {
    let pid = Pid::from_raw(i32::try_from(process.id().expect("owned ID")).expect("PID range"))
        .expect("PID");
    assert!(
        rustix::process::waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .expect("observation retained reaping authority")
        .is_some()
    );
    pid
}

#[test]
fn repeated_exit_observation_and_pipe_transfer_keep_child_waitable_until_settlement() {
    let mut process = BlockingProcess::spawn(
        Command::new("sh")
            .args(["-c", "printf captured; exit 23"])
            .stdout(Stdio::piped()),
    )
    .expect("child");
    let mut output = process.take_pipes().expect("pipes").stdout.expect("stdout");
    assert!(
        process
            .take_pipes()
            .expect("second transfer")
            .stdout
            .is_none()
    );
    for _ in 0..3 {
        assert_eq!(observe(&process).code(), Some(23));
    }
    let pid = assert_waitable(&process);
    process.settle();
    process.settle();
    assert!(process.id().is_err());
    assert!(process.try_status().is_err());
    assert!(matches!(
        rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG),
        Err(rustix::io::Errno::CHILD)
    ));
    let mut text = String::new();
    output.read_to_string(&mut text).expect("settled pipe");
    assert_eq!(text, "captured");
}

#[test]
fn signalled_exit_is_observed_without_reaping_and_drop_retires_it() {
    let process =
        BlockingProcess::spawn(Command::new("sh").args(["-c", "kill -TERM $$"])).expect("child");
    let status = observe(&process);
    assert_eq!(status.signal(), Some(15));
    let pid = assert_waitable(&process);
    drop(process);
    assert!(matches!(
        rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG),
        Err(rustix::io::Errno::CHILD)
    ));
}
