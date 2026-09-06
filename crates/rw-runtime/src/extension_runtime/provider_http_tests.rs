#![allow(clippy::expect_used)]
use super::{HttpOperation, PluginRuntimeBudget, retire};
use rw_ext::PluginProviderHttpOperation;
use rw_tools::{CancellationToken, EgressPolicy, SupervisedEgressProxy};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::oneshot;

fn operation(
    budget: &PluginRuntimeBudget,
    worker: tokio::task::JoinHandle<Result<(), rw_ext::PluginRpcError>>,
) -> Arc<HttpOperation> {
    Arc::new(HttpOperation {
        input: Mutex::new(None),
        response: tokio::sync::Mutex::new(None),
        worker: tokio::sync::Mutex::new(Some(worker)),
        cancellation: CancellationToken::default(),
        started: AtomicBool::new(true),
        settled: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        permit: Some(budget.http().expect("HTTP admission")),
    })
}

#[tokio::test]
async fn dropped_proof_waiter_retains_the_private_runtime_and_its_blocking_work() {
    let budget = PluginRuntimeBudget::default();
    let (started, start) = oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let committed = Arc::new(AtomicBool::new(false));
    let worker = tokio::task::spawn_blocking({
        let committed = committed.clone();
        move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let proxy = SupervisedEgressProxy::start(EgressPolicy::new(std::iter::empty::<&str>()))
                .expect("proxy");
            drop(runtime.spawn_blocking(move || {
                started.send(()).expect("start observer");
                released.recv().expect("release native work");
                committed.store(true, Ordering::Release);
            }));
            retire(runtime, proxy)
        }
    });
    let operation = operation(&budget, worker);
    start.await.expect("blocking operation admitted");
    let waiting = tokio::spawn({
        let operation = operation.clone();
        async move { operation.settle_effects().await }
    });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    waiting.abort();
    let _ = waiting.await;
    assert!(!committed.load(Ordering::Acquire));
    let mut other_slots = Vec::new();
    for _ in 0..7 {
        other_slots.push(budget.http().expect("remaining slot"));
    }
    assert!(
        budget.http().is_err(),
        "dropped waiter cannot release active HTTP admission"
    );
    release.send(()).expect("release blocking effect");
    operation
        .settle_effects()
        .await
        .expect("actual private runtime/proxy proof");
    assert!(committed.load(Ordering::Acquire));
    assert!(
        budget.http().is_err(),
        "response owner retains its slot through terminal publication"
    );
    drop(operation);
    drop(other_slots);
    budget.close().expect("all HTTP effects settled");
}

#[tokio::test]
async fn panicked_http_worker_keeps_failed_proof_and_aggregate_admission() {
    let budget = PluginRuntimeBudget::default();
    let operation = operation(
        &budget,
        tokio::task::spawn_blocking(|| panic!("lost HTTP owner")),
    );
    assert_eq!(
        operation
            .settle_effects()
            .await
            .expect_err("panic is not proof")
            .code,
        "effects_unsettled"
    );
    assert_eq!(
        operation
            .settle_effects()
            .await
            .expect_err("sticky failed proof")
            .code,
        "effects_unsettled"
    );
    drop(operation);
    assert!(
        budget.close().is_err(),
        "unproven slot cannot return to the shared application budget"
    );
}
