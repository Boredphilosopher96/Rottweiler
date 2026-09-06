#![allow(clippy::expect_used)]

use std::sync::Arc;

use tokio::process::Command;

use super::{CommandOutputTasks, NativeCommandState};
use crate::ToolError;

#[tokio::test]
async fn output_failure_still_settles_proxy_and_retains_cancelled_join() {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .process_group(0)
        .spawn()
        .expect("physical child");
    let child_id = child.id();
    child.wait().await.expect("child exited");
    let (release, released) = tokio::sync::oneshot::channel();
    let proxy = tokio::spawn(async move {
        released.await.expect("proxy retirement release");
    });
    let owner = Arc::new(tokio::sync::Mutex::new(NativeCommandState {
        _scratch: None,
        _helper: None,
        _process_credit: rw_resources::try_acquire(rw_resources::ResourceClass::Process)
            .expect("process resource"),
        _admission: Arc::new(tokio::sync::Semaphore::new(1))
            .acquire_owned()
            .await
            .expect("native admission"),
        child,
        child_id,
        watchdog: None,
        output: Some(CommandOutputTasks::new(
            tokio::spawn(async { Err(ToolError::Output("sink failed".to_owned())) }),
            tokio::spawn(async { Ok(()) }),
        )),
        proxy: None,
        proxy_cleanup: Some(proxy),
        proxy_failure: None,
        _lease: None,
    }));
    let draining = Arc::clone(&owner);
    let mut waiter = tokio::spawn(async move { draining.lock().await.settle().await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut waiter)
            .await
            .is_err(),
        "output failure must still await actual proxy retirement"
    );
    waiter.abort();
    assert!(waiter.await.expect_err("aborted waiter").is_cancelled());
    release.send(()).expect("proxy worker remains owned");
    let mut state = owner.lock().await;
    let first = state.settle().await.expect_err("sticky output error");
    assert!(state.child_id.is_none());
    assert!(state.proxy_cleanup.is_none());
    assert_eq!(
        first.to_string(),
        state.settle().await.expect_err("same proof").to_string()
    );
}
