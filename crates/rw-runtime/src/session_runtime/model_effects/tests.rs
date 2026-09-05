#![allow(clippy::expect_used)]
use super::*;
use async_trait::async_trait;
use tokio::sync::Notify;

#[derive(Default)]
struct Driver {
    entered: Notify,
    release: Notify,
    block: bool,
    fail: bool,
    panic: bool,
    calls: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl ModelDriver for Driver {
    fn stream(
        &self,
        _: &str,
        _: rw_providers::ProviderRequest,
        _: rw_core::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "unused fixture route".to_owned(),
        ))
    }
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.entered.notify_one();
        if self.block {
            self.release.notified().await;
        }
        assert!(!self.panic, "fixture cleanup panic");
        if self.fail {
            return Err(AgentLoopError::EffectsUnsettled(
                "fixture unproven cleanup".to_owned(),
            ));
        }
        Ok(())
    }
}
fn empty() -> BoxEventStream {
    Box::pin(futures_util::stream::empty())
}

#[tokio::test]
async fn model_replacement_waits_for_old_invocation_and_ignores_unrelated_live_stream() {
    let effects = ModelEffects::default();
    let old = Arc::new(Driver {
        block: true,
        ..Driver::default()
    });
    let weak = Arc::downgrade(&old);
    drop(
        effects
            .stream(old.clone(), |_| Ok(empty()))
            .expect("old invocation"),
    );
    old.entered.notified().await;
    let next = Arc::new(Driver::default());
    let live = effects
        .stream(next.clone(), |_| Ok(empty()))
        .expect("replacement invocation");
    let settlement = effects.settle();
    tokio::pin!(settlement);
    assert!(futures_util::poll!(&mut settlement).is_pending());
    assert_eq!(next.calls.load(Ordering::Acquire), 0);
    old.release.notify_one();
    settlement.await.expect("old effects settled");
    drop(old);
    assert!(weak.upgrade().is_none());
    assert_eq!(next.calls.load(Ordering::Acquire), 0);
    drop(live);
    effects.settle().await.expect("replacement cleanup");
}

#[tokio::test]
async fn failed_or_panicked_model_owner_is_retained_and_blocks_further_admission() {
    for panic in [false, true] {
        let effects = ModelEffects::default();
        let driver = Arc::new(Driver {
            fail: !panic,
            panic,
            ..Driver::default()
        });
        let weak = Arc::downgrade(&driver);
        drop(
            effects
                .stream(driver.clone(), |_| Ok(empty()))
                .expect("invocation"),
        );
        let proof = tokio::time::timeout(std::time::Duration::from_secs(1), effects.settle())
            .await
            .expect("bounded proof");
        assert!(matches!(proof, Err(AgentLoopError::EffectsUnsettled(_))));
        drop(driver);
        assert!(weak.upgrade().is_some());
        assert!(
            effects
                .stream(Arc::new(Driver::default()), |_| Ok(empty()))
                .is_err()
        );
    }
}

#[tokio::test]
async fn constructor_error_still_settles_the_exact_selected_driver() {
    let effects = ModelEffects::default();
    let driver = Arc::new(Driver::default());
    assert!(
        effects
            .stream(driver.clone(), |_| Err(
                AgentLoopError::InvalidConfiguration("construction rejected".to_owned())
            ))
            .is_err()
    );
    effects.settle().await.expect("constructor cleanup");
    assert_eq!(driver.calls.load(Ordering::Acquire), 1);
}
