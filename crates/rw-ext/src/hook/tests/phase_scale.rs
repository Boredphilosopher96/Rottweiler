//! Handler count changes do not create another phase or cleanup deadline.
use super::{must, prompt};
use crate::hook::{
    HookClass, HookDirective, HookDispatcher, HookError, HookEvent, HookHandler, HookInvocation,
    HookRegistration,
};
use async_trait::async_trait;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

struct TimedPolicy {
    started: Arc<AtomicUsize>,
    completed: Arc<AtomicUsize>,
    settled: Arc<AtomicUsize>,
}
#[async_trait]
impl HookHandler for TimedPolicy {
    async fn invoke(&self, _: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(30)).await;
        self.completed.fetch_add(1, Ordering::SeqCst);
        Ok(HookDirective::Continue {})
    }
    async fn settle_effects(&self) -> Result<(), HookError> {
        self.settled.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn one_ten_fifty_policy_handlers_share_one_aggregate_deadline() {
    for count in [1, 10, 50] {
        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let settled = Arc::new(AtomicUsize::new(0));
        let mut dispatcher = HookDispatcher::new();
        for index in 0..count {
            must(
                dispatcher.register(
                    HookRegistration::new(
                        format!("policy-{index:02}"),
                        HookEvent::UserPromptSubmit,
                        HookClass::Policy,
                    )
                    .with_timeout(Duration::from_millis(40)),
                    TimedPolicy {
                        started: Arc::clone(&started),
                        completed: Arc::clone(&completed),
                        settled: Arc::clone(&settled),
                    },
                ),
            );
        }
        let began = tokio::time::Instant::now();
        let outcome = must(dispatcher.dispatch(prompt("bounded phase")).await);
        let elapsed = began.elapsed();
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(started.load(Ordering::SeqCst), count.min(2));
        assert!(settled.load(Ordering::SeqCst) >= started.load(Ordering::SeqCst));
        if count == 1 {
            assert!(outcome.completed());
            assert_eq!(elapsed, Duration::from_millis(30));
        } else {
            assert!(!outcome.completed());
            assert_eq!(outcome.failures().len(), 1);
            assert_eq!(outcome.failures()[0].hook_id(), "policy-01");
            assert_eq!(elapsed, Duration::from_millis(40));
        }
        println!(
            "hook_phase_oracle count={count} virtual_elapsed_us={} started={} completed=1",
            elapsed.as_micros(),
            started.load(Ordering::SeqCst)
        );
        must(dispatcher.settle_effects(HookEvent::UserPromptSubmit).await);
    }
}
