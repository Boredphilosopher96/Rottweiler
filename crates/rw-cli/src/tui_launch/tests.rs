#![allow(clippy::expect_used)]
use super::*;
use std::{fs, os::unix::fs::PermissionsExt as _, time::Duration};

fn start(root: &Path, script: &str, replay: bool) -> TuiProcess {
    let executable = root.join("host");
    fs::write(&executable, script).expect("host fixture");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("executable");
    TuiProcess::start(&TuiLaunch {
        executable: &executable,
        socket: &root.join("engine.sock"),
        token_file: &root.join("token"),
        session_id: "session",
        last_seen_file: &root.join("last-seen"),
        fork_operation_directory: root,
        keybindings: None,
        theme: "",
        replay,
    })
}

#[tokio::test]
async fn every_client_restart_preserves_role_parent_and_handoff_beyond_crash_budget() {
    for replay in [false, true] {
        let root = tempfile::tempdir().expect("runtime");
        let mut client = start(
            root.path(),
            "#!/bin/sh\n\
             [ \"$#\" = 1 ] && [ \"$1\" = tui ] || exit 64\n\
             [ \"$ROTTWEILER_SUPERVISOR_PID\" = \"$PPID\" ] || exit 64\n\
             [ -n \"$ROTTWEILER_TUI_RECYCLE_STATE_FILE\" ] || exit 64\n\
             state=\"$ROTTWEILER_TUI_RECYCLE_STATE_FILE\"\n\
             if [ -f \"$state\" ]; then count=$(cat \"$state\"); else count=0; fi\n\
             count=$((count + 1))\n\
             printf %s \"$count\" > \"$state\"\n\
             if [ \"$count\" -le 8 ]; then exit 75; fi\n\
             printf %s \"$ROTTWEILER_REPLAY_MODE\" > \"$ROTTWEILER_FORK_OPERATION_DIRECTORY/mode\"\n",
            replay,
        );
        tokio::time::timeout(Duration::from_secs(5), client.wait())
            .await
            .expect("restart deadline")
            .expect("client close");
        assert_eq!(
            fs::read_to_string(root.path().join("tui-recycle-state.json")).expect("state"),
            "9"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("mode")).expect("mode"),
            if replay { "1" } else { "0" }
        );
        client.shutdown().await.expect("already settled");
    }
}

async fn child_pid(root: &Path) -> rustix::process::Pid {
    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(value) = fs::read_to_string(root.join("pid"))
                && let Ok(value) = value.parse::<i32>()
            {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("child startup");
    rustix::process::Pid::from_raw(pid).expect("pid")
}

const HELD_CLIENT: &str =
    "#!/bin/sh\nprintf %s \"$$\" > \"$ROTTWEILER_FORK_OPERATION_DIRECTORY/pid\"\nexec sleep 30\n";

#[tokio::test]
async fn shutdown_reaps_the_client_before_releasing_its_runtime_directory() {
    let root = tempfile::tempdir().expect("runtime");
    let mut client = start(root.path(), HELD_CLIENT, false);
    let pid = child_pid(root.path()).await;
    tokio::time::timeout(Duration::from_secs(5), client.shutdown())
        .await
        .expect("shutdown deadline")
        .expect("reaped child");
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
}

#[tokio::test]
async fn dropped_caller_keeps_a_process_owner_until_the_client_is_reaped() {
    let root = tempfile::tempdir().expect("runtime");
    let client = start(root.path(), HELD_CLIENT, false);
    let pid = child_pid(root.path()).await;
    drop(client);
    tokio::time::timeout(Duration::from_secs(5), async {
        while rustix::process::test_kill_process(pid).is_ok() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("abandoned client owner reaps");
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
}

#[tokio::test]
async fn interrupt_exit_closes_without_restarting_the_client() {
    let root = tempfile::tempdir().expect("runtime");
    let mut client = start(root.path(), "#!/bin/sh\nexit 130\n", false);
    client.wait().await.expect("user close");
}
