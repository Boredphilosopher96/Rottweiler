#![cfg(test)]
#![allow(clippy::expect_used)]
use super::SearchStartup;
use async_trait::async_trait;
use rw_tools::{CancellationToken, ToolError, WebSearchRequest, WebSearchResponse, WebSearcher};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Backend(Arc<AtomicBool>);
#[async_trait]
impl WebSearcher for Backend {
    async fn search(
        &self,
        _: WebSearchRequest,
        _: CancellationToken,
    ) -> Result<WebSearchResponse, ToolError> {
        unreachable!("startup fixture")
    }
    async fn settle_effects(&self) -> Result<(), ToolError> {
        self.0.store(true, Ordering::Release);
        Ok(())
    }
}
#[tokio::test]
async fn cancelled_initializer_keeps_worker_result_for_settlement() {
    let owner = Arc::new(SearchStartup::default());
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let settled = Arc::new(AtomicBool::new(false));
    let proof = Arc::clone(&settled);
    let startup = owner.start(move || {
        entered.send(()).expect("entered");
        wait.recv().expect("release");
        Ok(Arc::new(Backend(proof)))
    });
    let caller = tokio::spawn(startup.wait());
    started.await.expect("worker entered");
    caller.abort();
    assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
    let settlement_owner = Arc::clone(&owner);
    let settlement = tokio::spawn(async move { settlement_owner.settle().await });
    tokio::task::yield_now().await;
    assert!(!settlement.is_finished());
    release.send(()).expect("release worker");
    settlement.await.expect("join").expect("settled");
    assert!(settled.load(Ordering::Acquire));
}
#[tokio::test]
async fn startup_panic_is_unsettled_while_completed_rejection_is_clean() {
    let panic = SearchStartup::default();
    assert!(
        panic
            .start(|| panic!("injected initialization panic"))
            .wait()
            .await
            .is_err()
    );
    assert!(matches!(
        panic.settle().await,
        Err(ToolError::EffectsUnsettled(_))
    ));
    let rejection = SearchStartup::default();
    assert!(
        rejection
            .start(|| Err("invalid endpoint".into()))
            .wait()
            .await
            .is_err()
    );
    rejection
        .settle()
        .await
        .expect("completed rejection has no worker effects");
}
