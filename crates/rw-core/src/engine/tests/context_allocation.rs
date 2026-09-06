#![cfg(test)]
use crate::engine::turn::context_memory::admit;
use crate::engine::{
    builtin_hook_dispatcher,
    recovery::HistoryRead,
    tests::fixtures::{
        history,
        models::ScriptedModel,
        support::{config, text_turn},
    },
};
use rw_tools::ToolRegistry;
use rw_types::{Role, config::PermissionDecision};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
struct Owner(Arc<AtomicBool>);
impl Drop for Owner {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[tokio::test]
async fn context_work_is_admitted_before_copies_and_owned_through_delivery() {
    let root = tempfile::tempdir().expect("root");
    let config = history::bind(config(
        root.path(),
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("source");
    let dropped = Arc::new(AtomicBool::new(false));
    let oversized = vec![text_turn(Role::User, "x".repeat(33 * 1024 * 1024))];
    assert!(
        admit(
            HistoryRead::new((), Owner(dropped.clone())),
            &config,
            &oversized,
            &VecDeque::new()
        )
        .is_err()
    );
    assert!(dropped.load(Ordering::Acquire));
    drop(oversized);
    dropped.store(false, Ordering::Release);
    let admitted = admit(
        HistoryRead::new((), Owner(dropped.clone())),
        &config,
        &[text_turn(Role::User, "bounded")],
        &VecDeque::new(),
    )
    .expect("working allowance");
    let delivered = admitted.map(|_| "result");
    assert!(!dropped.load(Ordering::Acquire));
    drop(delivered);
    assert!(dropped.load(Ordering::Acquire));
}

#[tokio::test]
async fn dropped_blocking_reply_keeps_the_actual_worker_registered_until_exit() {
    let root = tempfile::tempdir().expect("root");
    let config = Arc::new(
        history::bind(config(
            root.path(),
            Arc::new(ScriptedModel::default()),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .await
        .expect("source"),
    );
    let tasks = crate::engine::task_ownership::ActorTasks::default();
    let (entered, entry) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let worker = tasks
        .spawn_blocking(
            config,
            rw_tools::CancellationToken::default(),
            rw_resources::ResourceClass::Blocking,
            move || {
                entered.send(()).expect("worker entry");
                wait.recv().expect("release worker");
                42
            },
        )
        .await
        .expect("admitted worker");
    entry.await.expect("actual execution");
    drop(worker);
    assert!(!tasks.idle());
    tasks.cancel();
    assert!(
        !tasks.idle(),
        "cancellation cannot claim completion of a blocking worker"
    );
    release.send(()).expect("release");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !tasks.idle() {
            tasks.changed().await;
        }
    })
    .await
    .expect("actual completion");
    assert!(tasks.failure().is_none());
}
