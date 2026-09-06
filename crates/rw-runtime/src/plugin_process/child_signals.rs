#![allow(clippy::expect_used)]
use super::{PluginChild, SupervisedPluginProcess};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const READY: &str = "RW_PLUGIN_GROUP_CHANGE_READY";
const GROUP: &str = "RW_PLUGIN_GROUP_CHANGE_TARGET";

#[test]
fn child_joins_its_parents_process_group() {
    let Some(ready) = std::env::var_os(READY) else {
        return;
    };
    let group = std::env::var(GROUP)
        .expect("target group")
        .parse::<i32>()
        .expect("group ID");
    let group = rustix::process::Pid::from_raw(group).expect("valid group");
    rustix::process::setpgid(None, Some(group)).expect("change process group");
    std::fs::write(ready, "ready").expect("ready marker");
    std::thread::sleep(Duration::from_secs(10));
}

#[tokio::test]
async fn kill_tree_signals_the_actual_child_after_it_changes_groups() {
    let directory = tempfile::tempdir().expect("directory");
    let ready = directory.path().join("ready");
    let mut command =
        tokio::process::Command::new(std::env::current_exe().expect("test executable"));
    command
        .args([
            "--exact",
            "plugin_process::child_signals::child_joins_its_parents_process_group",
            "--nocapture",
        ])
        .env(READY, &ready)
        .env(
            GROUP,
            rustix::process::getpgrp()
                .as_raw_nonzero()
                .get()
                .to_string(),
        )
        .kill_on_drop(true)
        .process_group(0)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = command.spawn().expect("child");
    let owner = PluginChild {
        _helper: rw_tools::SandboxHelper::from_running(
            &std::env::current_exe().expect("executable"),
        )
        .expect("helper"),
        process_group: child.id(),
        child: Mutex::new(child),
        violation: Arc::new(Mutex::new(None)),
        proxy: super::proxy_settlement::PluginProxy::new(None),
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("child changed groups");
    owner.kill_tree().expect("signal exact child");
    tokio::time::timeout(Duration::from_secs(2), owner.wait_for_exit())
        .await
        .expect("child terminated")
        .expect("exit proof");
}
