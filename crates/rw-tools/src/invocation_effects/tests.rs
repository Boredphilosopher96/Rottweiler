#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{InvocationEffects, MAX_INVOCATION_OPERATIONS};
use crate::{CancellationToken, ToolError};
use async_trait::async_trait;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;

struct Backend {
    proof: Arc<Semaphore>,
    calls: AtomicUsize,
    panic: bool,
}
#[async_trait]
impl super::InvocationEffect for Backend {
    async fn settle_effects(&self) -> Result<(), ToolError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        assert!(!self.panic, "injected proof panic");
        self.proof.acquire().await.expect("proof").forget();
        Ok(())
    }
}
fn backend(proof: usize, panic: bool) -> Arc<Backend> {
    Arc::new(Backend {
        proof: Arc::new(Semaphore::new(proof)),
        calls: AtomicUsize::new(0),
        panic,
    })
}
#[tokio::test]
async fn abandoned_invocation_retains_actual_backend_and_credit_until_settled() {
    let owner = Arc::new(InvocationEffects::default());
    let backend = backend(0, false);
    let retained = Arc::downgrade(&backend);
    let proof = Arc::clone(&backend.proof);
    let token = CancellationToken::default();
    let operation = owner.begin(backend, token.clone()).expect("admission");
    drop(operation);
    assert!(token.is_cancelled());
    assert!(retained.upgrade().is_some());
    assert_eq!(
        owner.credits.available_permits(),
        MAX_INVOCATION_OPERATIONS - 1
    );
    let settle_owner = Arc::clone(&owner);
    let settlement = tokio::spawn(async move { settle_owner.settle().await });
    tokio::task::yield_now().await;
    assert!(!settlement.is_finished());
    proof.add_permits(1);
    settlement.await.expect("join").expect("settlement");
    assert!(retained.upgrade().is_none());
    assert_eq!(owner.credits.available_permits(), MAX_INVOCATION_OPERATIONS);
}
#[tokio::test]
async fn invocation_proof_panic_retains_failed_owner_and_attempts_other_backends() {
    let owner = Arc::new(InvocationEffects::default());
    let failed = backend(0, true);
    let other = backend(1, false);
    drop(
        owner
            .begin(failed.clone(), CancellationToken::default())
            .expect("first"),
    );
    drop(
        owner
            .begin(other.clone(), CancellationToken::default())
            .expect("second"),
    );
    assert!(matches!(
        owner.settle().await,
        Err(ToolError::EffectsUnsettled(_))
    ));
    assert_eq!(other.calls.load(Ordering::Acquire), 1);
    assert_eq!(
        owner.credits.available_permits(),
        MAX_INVOCATION_OPERATIONS - 1
    );
    assert!(owner.settle().await.is_err());
    assert_eq!(
        failed.calls.load(Ordering::Acquire),
        1,
        "failed proof is sticky"
    );
}
#[tokio::test(start_paused = true)]
async fn invocation_proof_deadline_keeps_backend_and_admission_charged() {
    let owner = Arc::new(InvocationEffects::default());
    let backend = backend(0, false);
    let retained = Arc::downgrade(&backend);
    drop(
        owner
            .begin(backend, CancellationToken::default())
            .expect("first"),
    );
    assert!(matches!(
        owner.settle().await,
        Err(ToolError::EffectsUnsettled(_))
    ));
    assert!(retained.upgrade().is_some());
    assert_eq!(
        owner.credits.available_permits(),
        MAX_INVOCATION_OPERATIONS - 1
    );
}
