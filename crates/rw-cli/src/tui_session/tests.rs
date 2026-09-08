#![allow(clippy::expect_used)]
use super::*;
use crate::{
    runtime_paths::RuntimeDirectoryGuard,
    tui_launch::{TuiLaunch, TuiProcess},
};
use std::{fs, os::unix::fs::PermissionsExt as _, time::Duration};

#[tokio::test]
async fn lost_waiter_retains_directory_until_client_is_physically_reaped() {
    let root = tempfile::tempdir().expect("fixture");
    let directory = root.path().join("runtime");
    fs::create_dir(&directory).expect("runtime");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).expect("private runtime");
    let guard = RuntimeDirectoryGuard::capture(&directory).expect("directory owner");
    let executable = root.path().join("host");
    fs::write(&executable, "#!/bin/sh\nprintf %s \"$$\" > \"$ROTTWEILER_FORK_OPERATION_DIRECTORY/pid\"\nexec sleep 30\n").expect("host");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("executable");
    let fork_directory = root.path().to_owned();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (cancelled, observed_cancel) = tokio::sync::oneshot::channel();
    let (continue_cleanup, cleanup_gate) = tokio::sync::oneshot::channel();
    let (settled, settlement) = tokio::sync::oneshot::channel();
    let caller = tokio::spawn(run(move |mut stop| {
        Box::pin(async move {
            let directory_owner = guard;
            let directory = &directory_owner.path;
            let mut client = TuiProcess::start(&TuiLaunch {
                executable: &executable,
                socket: &directory.join("engine.sock"),
                token_file: &directory.join("auth.token"),
                session_id: "owned-client",
                last_seen_file: &directory.join("last-seen"),
                fork_operation_directory: &fork_directory,
                keybindings: None,
                theme: "",
                replay: false,
            });
            started.send(()).expect("caller ready");
            stop.cancelled().await;
            cancelled.send(()).expect("cancellation observed");
            cleanup_gate.await.expect("cleanup gate");
            client.shutdown().await.into_diagnostic()?;
            drop(directory_owner);
            settled.send(()).expect("settlement receipt");
            Ok(())
        })
    }));
    ready.await.expect("session started");
    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(value) = fs::read_to_string(root.path().join("pid"))
                && let Ok(value) = value.parse::<i32>()
            {
                break rustix::process::Pid::from_raw(value).expect("pid");
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("physical child");
    caller.abort();
    assert!(caller.await.expect_err("caller cancelled").is_cancelled());
    tokio::time::timeout(Duration::from_secs(5), observed_cancel)
        .await
        .expect("cancel deadline")
        .expect("owner retained");
    assert!(directory.is_dir());
    assert!(rustix::process::test_kill_process(pid).is_ok());
    continue_cleanup.send(()).expect("release cleanup");
    tokio::time::timeout(Duration::from_secs(5), settlement)
        .await
        .expect("settle deadline")
        .expect("settled");
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    assert!(!directory.exists());
}
