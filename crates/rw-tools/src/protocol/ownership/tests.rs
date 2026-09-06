#![allow(clippy::expect_used)]
use super::{ProcessOwner, SupervisedEgressProxy};
use std::{io, process::Stdio, time::Duration};

fn physical(proxy: Option<SupervisedEgressProxy>) -> (ProcessOwner, u32) {
    let credit = rw_resources::try_acquire(rw_resources::ResourceClass::Process).expect("credit");
    let helper =
        rw_sandbox::SandboxHelper::from_running(&std::env::current_exe().expect("test image"))
            .expect("running image");
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(["-c", "while :; do :; done"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let child = command.spawn().expect("physical child");
    let pid = child.id().expect("pid");
    (ProcessOwner::new(child, helper, credit, proxy), pid)
}
fn pid(value: u32) -> rustix::process::Pid {
    rustix::process::Pid::from_raw(i32::try_from(value).expect("pid range")).expect("pid")
}

#[tokio::test]
async fn dropped_handle_retires_the_actual_child_and_proxy() {
    let proxy =
        SupervisedEgressProxy::start(rw_sandbox::EgressPolicy::new(std::iter::empty::<&str>()))
            .expect("proxy");
    let proof = proxy.lifecycle();
    let (owner, child) = physical(Some(proxy));
    drop(owner);
    tokio::time::timeout(Duration::from_secs(2), async {
        while rustix::process::test_kill_process(pid(child)).is_ok() || !proof.is_stopped() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("physical cleanup");
}

#[tokio::test]
async fn proven_settlement_consumes_identity_and_is_idempotent() {
    let (mut owner, _) = physical(None);
    owner.settle(Duration::from_secs(2)).await.expect("settled");
    assert!(
        owner.0.is_none(),
        "old group, helper and credit are retired together"
    );
    owner
        .settle(Duration::ZERO)
        .await
        .expect("does not revisit an old PID");
}

#[tokio::test]
async fn failed_reap_retains_every_physical_owner() {
    let (mut owner, child) = physical(None);
    owner
        .0
        .as_mut()
        .expect("physical owner")
        .signal()
        .expect("signal fixture");
    rustix::process::waitpid(Some(pid(child)), rustix::process::WaitOptions::empty())
        .expect("external wait")
        .expect("reaped");
    let failure = owner
        .settle(Duration::from_secs(2))
        .await
        .expect_err("lost proof");
    assert_eq!(
        failure.raw_os_error(),
        Some(rustix::io::Errno::CHILD.raw_os_error())
    );
    assert!(
        owner.0.is_some(),
        "child, helper and resource credit remain owned"
    );
    assert!(owner.child().is_ok());
}

#[tokio::test]
async fn missing_stdio_still_has_an_owned_retirement_path() {
    let (mut owner, child) = physical(None);
    owner.child().expect("owner").stdin.take();
    let absent = owner
        .child()
        .expect("owner")
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("stdin unavailable"));
    assert!(absent.is_err());
    drop(owner);
    tokio::time::timeout(Duration::from_secs(2), async {
        while rustix::process::test_kill_process(pid(child)).is_ok() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("failed handoff retires actual child");
}

#[tokio::test]
async fn signal_failure_never_replaces_actual_reap_and_absence_proof() {
    let (mut owner, child) = physical(None);
    owner.0.as_mut().expect("physical owner").signal_result =
        Some(Err("signal delivery: operation not permitted".into()));
    let result = owner
        .settle(Duration::ZERO)
        .await
        .expect_err("live child is unproven");
    assert_eq!(result.kind(), io::ErrorKind::TimedOut);
    assert!(owner.0.is_some(), "live physical owner remains charged");
    rustix::process::kill_process_group(pid(child), rustix::process::Signal::KILL)
        .expect("terminate physical fixture");
    owner
        .settle(Duration::from_secs(2))
        .await
        .expect("actual reap and group absence");
    assert!(
        owner.0.is_none(),
        "proven retirement releases actual ownership"
    );
}

#[tokio::test]
async fn synchronous_termination_signals_without_polling_async_cleanup() {
    let (mut owner, _) = physical(None);
    owner.request_termination().expect("synchronous signal");
    assert!(
        owner.0.is_some(),
        "signal does not release physical authority"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if owner
            .child()
            .expect("child")
            .try_wait()
            .expect("try reap")
            .is_some()
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "signal was deferred");
        // No async cleanup task can progress on this current-thread runtime.
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        owner.0.is_some(),
        "reaping alone does not consume group authority"
    );
    owner
        .settle(Duration::from_secs(2))
        .await
        .expect("group proof");
    assert!(owner.0.is_none());
}
