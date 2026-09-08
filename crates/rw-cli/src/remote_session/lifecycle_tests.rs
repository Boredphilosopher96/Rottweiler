#![allow(clippy::expect_used)]
use super::*;
use std::{process::Stdio, time::Duration};

#[tokio::test]
async fn watchdog_shutdown_reaps_its_tunnel_before_returning() {
    let root = tempfile::tempdir().expect("runtime");
    let paths = server::ServerRuntimePaths {
        directory: root.path().to_owned(),
        socket: root.path().join("engine.sock"),
        token: root.path().join("auth.token"),
        descriptor: root.path().join("runtime.json"),
    };
    let config = remote::RemoteConfig {
        ssh_executable: "/usr/bin/ssh".into(),
        host: "unused".into(),
        remote_rw_executable: "/unused/rw".into(),
        remote_socket: "/unused/engine.sock".into(),
        local_socket: paths.socket.clone(),
        session_id: "session".into(),
        remote_workspace: root.path().to_owned(),
        additional_workspaces: vec![],
        dangerously_trust: false,
        model: None,
        permission_mode: None,
    };
    let mut runtime = TokioRemoteRecoveryRuntime::new(config, paths);
    let child = tokio::process::Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("tunnel fixture");
    let pid = rustix::process::Pid::from_raw(
        i32::try_from(child.id().expect("child ID")).expect("pid width"),
    )
    .expect("pid");
    runtime.tunnel = Some(child);
    let (control, commands) = tokio::sync::mpsc::channel(2);
    control
        .send(remote::WatchdogCommand::Shutdown)
        .await
        .expect("queued stop before recovery");
    let mut watchdog = RemoteWatchdog::start(runtime, commands);
    tokio::time::timeout(Duration::from_secs(5), watchdog.wait())
        .await
        .expect("settlement deadline")
        .expect("settled watchdog");
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    watchdog.wait().await.expect("already physically settled");
}

#[tokio::test]
async fn oversized_startup_readiness_kills_and_reaps_before_returning_error() {
    let root = tempfile::tempdir().expect("fixture");
    let pid_path = root.path().join("pid");
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args([
            "-c",
            "printf %s \"$$\" > \"$1\"; head -c 65537 /dev/zero; exec sleep 30",
            "readiness",
        ])
        .arg(&pid_path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let error = tokio::time::timeout(Duration::from_secs(5), read_remote_readiness(&mut command))
        .await
        .expect("early byte rejection")
        .expect_err("oversized descriptor");
    assert!(error.contains("64KiB descriptor limit"));
    let pid = rustix::process::Pid::from_raw(
        fs::read_to_string(pid_path)
            .expect("started PID")
            .parse()
            .expect("numeric PID"),
    )
    .expect("pid");
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
}
