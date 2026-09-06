#![cfg(unix)]
#![allow(clippy::expect_used)]
use std::process::Command;

use rw_resources::{ResourceClass, process::BlockingProcess, try_acquire};

#[test]
fn process_admission_is_retained_until_reaping_and_drop_retires_the_child() {
    let mut process = BlockingProcess::spawn(Command::new("sh").args(["-c", "exec sleep 30"]))
        .expect("owned process");
    let id = process.child_mut().expect("child").id();
    let leases = (0..63)
        .map(|_| try_acquire(ResourceClass::Process).expect("remaining group"))
        .collect::<Vec<_>>();
    assert!(try_acquire(ResourceClass::Process).is_err());
    drop(process);
    let returned = try_acquire(ResourceClass::Process).expect("reaped capacity returned");
    let pid = rustix::process::Pid::from_raw(i32::try_from(id).expect("pid range")).expect("pid");
    assert!(matches!(
        rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG),
        Err(rustix::io::Errno::CHILD)
    ));
    assert!(matches!(
        rustix::process::test_kill_process_group(pid),
        Err(rustix::io::Errno::SRCH)
    ));
    drop(returned);
    assert!(BlockingProcess::spawn(&mut Command::new("/nonexistent/rottweiler-command")).is_err());
    let mut process = BlockingProcess::spawn(Command::new("sh").args(["-c", "exec sleep 30"]))
        .expect("failed spawn returned capacity");
    process.settle();
    process.settle();
    assert!(process.child_mut().is_err());
    drop(leases);
}
