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
