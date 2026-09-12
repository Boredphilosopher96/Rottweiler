#![allow(clippy::expect_used)]
use super::*;
use std::{fs, os::unix::fs::PermissionsExt as _, time::Duration};

struct ReplayCaller(tokio::task::JoinHandle<Result<()>>);
impl Drop for ReplayCaller {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn replay_rejects_host_shutdown_and_caller_loss_reaps_client_and_runtime() {
    let root = tempfile::tempdir().expect("fixture");
    drop(rw_store::session::SessionEventLog::open(root.path(), "history").expect("source"));
    let executable = root.path().join("host");
    fs::write(&executable, "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n' \"$$\" \"$ROTTWEILER_ENGINE_SOCKET\" \"$ROTTWEILER_ENGINE_TOKEN_FILE\" > \"$(dirname \"$0\")/started\"\nexec sleep 30\n").expect("host");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("executable");
    let storage = root.path().to_owned();
    let mut caller = ReplayCaller(tokio::spawn(async move {
        run_history_replay_with_tui(&storage, "history", &executable).await
    }));
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
    let shutdown =
        crate::remote::shutdown_authenticated_host(&socket, token.trim(), Duration::from_secs(5))
            .await;
    let client_alive = rustix::process::test_kill_process(pid);
    let runtime_directory = socket.parent().expect("runtime directory").to_owned();
    let runtime_alive = runtime_directory.exists();
    // Caller loss uses the production owner cancellation path. A replay observer
    // cannot stop the host through its authenticated read connection.
    caller.0.abort();
    assert!(
        (&mut caller.0)
            .await
            .expect_err("caller was cancelled")
            .is_cancelled()
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while rustix::process::test_kill_process(pid) != Err(rustix::io::Errno::SRCH)
            || runtime_directory.exists()
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("owned replay must reap the client before removing its runtime directory");
    assert_eq!(
        shutdown,
        Err("remote engine rejected host shutdown: read_only".to_owned())
    );
    assert_eq!(client_alive, Ok(()));
    assert!(runtime_alive);
    assert_eq!(
        rw_store::session::SessionEventLog::open(root.path(), "history")
            .expect("source remains available")
            .next_sequence(),
        0,
        "neither denied host shutdown nor caller cleanup may append to history"
    );
}
