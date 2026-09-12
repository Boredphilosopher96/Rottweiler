#![allow(clippy::expect_used)]
use super::*;
use std::{future::Future, task::Poll};

#[tokio::test]
async fn cancelled_readiness_wait_keeps_the_watchdog_pipe_until_disarm() {
    let root = tempfile::tempdir().expect("gate root");
    let gate = root.path().join("publish-ready");
    std::fs::write(&gate, b"hold").expect("hold readiness");
    let script = format!(
        "while [ -e {} ]; do sleep 0.01; done\n{}",
        shell_words::quote(&gate.to_string_lossy()),
        WATCHDOG_SCRIPT,
    );
    let mut target = Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .expect("command group");
    let mut owner = None;
    let mut arm = Box::pin(arm_with_script(&mut owner, target.id(), None, &script));
    std::future::poll_fn(|context| {
        assert!(
            arm.as_mut().poll(context).is_pending(),
            "ready gate is closed"
        );
        Poll::Ready(())
    })
    .await;
    drop(arm);
    std::fs::remove_file(&gate).expect("allow watchdog readiness after caller loss");
    target.kill().await.expect("settle command");
    tokio::time::timeout(
        Duration::from_secs(3),
        owner.as_mut().expect("owned watchdog").disarm(),
    )
    .await
    .expect("bounded disarm")
    .expect("watchdog ready write still has its owned reader");
}
