#![allow(clippy::expect_used)]
use super::*;
use rw_tools::EgressPolicy;
use std::os::unix::process::CommandExt as _;

fn incomplete_child() -> (Child, PluginProcessConfig, u32) {
    let config = PluginProcessConfig::new("/bin/sh").expect("fixture config");
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(["-c", "while :; do :; done"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let child = command.spawn().expect("fixture child");
    let pid = child.id().expect("pid");
    (child, config, pid)
}

#[tokio::test]
async fn incomplete_stdio_is_rejected_only_after_actual_child_cleanup() {
    let (child, config, pid) = incomplete_child();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        attach_supervisor(
            child,
            None,
            &config,
            running_helper(),
            process_fixture_lease(),
        ),
    )
    .await
    .expect("bounded handoff cleanup");
    assert!(matches!(result, Err(PluginLaunchError::Rejected(_))));
    let alive = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("probe");
    assert!(!alive.success());
}

#[tokio::test]
async fn lost_wait_result_is_typed_as_unsettled_launch() {
    let (child, config, pid) = incomplete_child();
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).expect("pid range")).expect("pid");
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).expect("kill fixture");
    rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty())
        .expect("consume OS wait result")
        .expect("reaped");
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        attach_supervisor(
            child,
            None,
            &config,
            running_helper(),
            process_fixture_lease(),
        ),
    )
    .await
    .expect("failed proof returns");
    assert!(matches!(
        result,
        Err(PluginLaunchError::EffectsUnsettled { .. })
    ));
}

#[tokio::test]
async fn successful_process_settlement_stops_proxy_while_process_owner_stays_alive() {
    let config = PluginProcessConfig::new("/bin/sh").expect("fixture config");
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(["-c", "while :; do :; done"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let child = command.spawn().expect("child");
    let proxy =
        SupervisedEgressProxy::start(EgressPolicy::new(std::iter::empty::<&str>())).expect("proxy");
    let lifecycle = proxy.lifecycle();
    let launched = attach_supervisor(
        child,
        Some(proxy),
        &config,
        running_helper(),
        process_fixture_lease(),
    )
    .await
    .expect("handoff");
    assert!(!lifecycle.is_stopped());
    launched.process.kill_tree().expect("kill owned process");
    tokio::time::timeout(Duration::from_secs(2), launched.process.settle_effects())
        .await
        .expect("bounded proof")
        .expect("settled");
    assert!(
        lifecycle.is_stopped(),
        "the retained process Arc cannot keep proxy workers live after settlement"
    );
    launched
        .process
        .settle_effects()
        .await
        .expect("idempotent settlement");
}

fn running_helper() -> rw_tools::SandboxHelper {
    rw_tools::SandboxHelper::from_running(&std::env::current_exe().expect("executable"))
        .expect("helper")
}

#[tokio::test]
async fn dropping_bare_launched_process_settles_actual_child_and_proxy() {
    let config = PluginProcessConfig::new("/bin/sh").expect("fixture config");
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(["-c", "while :; do :; done"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let child = command.spawn().expect("child");
    let pid = child.id().expect("pid");
    let proxy =
        SupervisedEgressProxy::start(EgressPolicy::new(std::iter::empty::<&str>())).expect("proxy");
    let lifecycle = proxy.lifecycle();
    let launched = attach_supervisor(
        child,
        Some(proxy),
        &config,
        running_helper(),
        process_fixture_lease(),
    )
    .await
    .expect("handoff");
    drop(launched);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let alive = rustix::process::Pid::from_raw(i32::try_from(pid).expect("pid range"))
                .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok());
            if !alive && lifecycle.is_stopped() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("destructor transfers actual effect retirement");
}

#[tokio::test]
async fn dropped_launch_waiter_retires_the_child_returned_by_its_blocking_worker() {
    let config = PluginProcessConfig::new("/bin/sh").expect("fixture config");
    let helper = running_helper();
    let admission = process_fixture_lease();
    let (started, entered) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (spawned, child_pid) = tokio::sync::oneshot::channel();
    let waiting = tokio::spawn(handoff_in_worker(config, helper, admission, move || {
        let _ = started.send(());
        released
            .recv_timeout(Duration::from_secs(2))
            .expect("release launch worker");
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "while :; do :; done"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let child = command.spawn().expect("physical child");
        let _ = spawned.send(child.id().expect("child pid"));
        Ok((child, None))
    }));
    entered.await.expect("worker owns launch");
    waiting.abort();
    assert!(matches!(waiting.await, Err(error) if error.is_cancelled()));
    release.send(()).expect("finish admitted worker");
    let pid = child_pid.await.expect("worker completed actual spawn");
    let group = rustix::process::Pid::from_raw(i32::try_from(pid).expect("pid range"))
        .expect("group identity");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match rustix::process::test_kill_process_group(group) {
                Err(rustix::io::Errno::SRCH) => return,
                Ok(()) => tokio::time::sleep(Duration::from_millis(5)).await,
                other => panic!("unexpected group proof: {other:?}"),
            }
        }
    })
    .await
    .expect("dropped result retains group ownership through cleanup");
}
