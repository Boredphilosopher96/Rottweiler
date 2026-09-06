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
            history::working_allowance(Owner(dropped.clone())),
            &config,
            &oversized,
            &[super::context_cache::source(1)],
            &VecDeque::new()
        )
        .is_err()
    );
    assert!(dropped.load(Ordering::Acquire));
    drop(oversized);
    dropped.store(false, Ordering::Release);
    let admitted = admit(
        history::working_allowance(Owner(dropped.clone())),
        &config,
        &[text_turn(Role::User, "bounded")],
        &[super::context_cache::source(1)],
        &VecDeque::new(),
    )
    .expect("working allowance");
    let delivered = HistoryRead::new("result", admitted);
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

struct MeasuredAllowance {
    requests: Arc<std::sync::Mutex<Vec<usize>>>,
    limit: usize,
    _owner: Owner,
}
impl crate::engine::recovery::HistoryWorkingAllowance for MeasuredAllowance {
    fn resize(&mut self, bytes: usize) -> Result<(), crate::engine::AgentLoopError> {
        self.requests.lock().expect("requests").push(bytes);
        if bytes > self.limit {
            return Err(crate::engine::AgentLoopError::Persistence(
                "working admission exhausted".into(),
            ));
        }
        Ok(())
    }
}

#[tokio::test]
async fn checked_working_growth_precedes_normalization_and_retains_cache_high_water() {
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
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let dropped = Arc::new(AtomicBool::new(false));
    let allowance = MeasuredAllowance {
        requests: requests.clone(),
        limit: 128 * 1024 * 1024,
        _owner: Owner(dropped.clone()),
    };
    let working = admit(
        Box::new(allowance),
        &config,
        &[text_turn(Role::User, "x".repeat(1024 * 1024))],
        &[super::context_cache::source(1)],
        &VecDeque::new(),
    )
    .expect("checked work");
    let large = *requests.lock().expect("requests").last().expect("growth");
    assert!(
        large < 64 * 1024 * 1024,
        "small work must not reserve the128MiB ceiling"
    );
    assert_eq!(requests.lock().expect("requests")[0], 64 * 1024 + 512);
    assert_eq!(
        working.normalizations(),
        0,
        "allocation precedes normalization"
    );
    let working = crate::engine::turn::context_memory::readmit(
        working,
        &config,
        &[text_turn(Role::User, "small")],
        &[super::context_cache::source(2)],
        &VecDeque::new(),
    )
    .expect("smaller source");
    assert_eq!(
        *requests.lock().expect("requests").last().expect("retained"),
        large
    );
    drop(working);
    assert!(dropped.load(Ordering::Acquire));

    dropped.store(false, Ordering::Release);
    requests.lock().expect("requests").clear();
    let allowance = MeasuredAllowance {
        requests: requests.clone(),
        limit: 1024 * 1024,
        _owner: Owner(dropped.clone()),
    };
    assert!(
        admit(
            Box::new(allowance),
            &config,
            &[text_turn(Role::User, "small")],
            &[super::context_cache::source(1)],
            &VecDeque::new()
        )
        .is_err()
    );
    assert!(
        dropped.load(Ordering::Acquire),
        "failed growth releases the metadata owner"
    );
    assert!(
        *requests
            .lock()
            .expect("requests")
            .last()
            .expect("rejected growth")
            > 1024 * 1024
    );
}
