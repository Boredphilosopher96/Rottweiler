#![allow(clippy::expect_used)]
use super::*;
use std::{fs, os::unix::fs::PermissionsExt as _, time::Duration};

#[tokio::test]
async fn replay_server_exit_reaps_a_held_client_and_removes_runtime_files() {
    let root = tempfile::tempdir().expect("fixture");
    drop(rw_store::session::SessionEventLog::open(root.path(), "history").expect("source"));
    let executable = root.path().join("host");
    fs::write(&executable, "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n' \"$$\" \"$ROTTWEILER_ENGINE_SOCKET\" \"$ROTTWEILER_ENGINE_TOKEN_FILE\" > \"$(dirname \"$0\")/started\"\nexec sleep 30\n").expect("host");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("executable");
    let storage = root.path().to_owned();
    let caller =
        tokio::spawn(
            async move { run_history_replay_with_tui(&storage, "history", &executable).await },
        );
    let receipt = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(receipt) = fs::read_to_string(root.path().join("started"))
                && receipt.lines().count() == 3
            {
                break receipt;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("client startup");
    let mut lines = receipt.lines();
    let pid =
        rustix::process::Pid::from_raw(lines.next().expect("pid").parse().expect("numeric pid"))
            .expect("pid");
    let socket = std::path::PathBuf::from(lines.next().expect("socket"));
    let token = fs::read_to_string(lines.next().expect("token path")).expect("bootstrap token");
    crate::remote::shutdown_authenticated_host(&socket, token.trim(), Duration::from_secs(5))
        .await
        .expect("server shutdown");
    let result = tokio::time::timeout(Duration::from_secs(5), caller)
        .await
        .expect("outer settlement")
        .expect("owner task");
    assert!(
        result
            .expect_err("unexpected server exit")
            .to_string()
            .contains("server stopped")
    );
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    assert!(!socket.parent().expect("runtime directory").exists());
}
