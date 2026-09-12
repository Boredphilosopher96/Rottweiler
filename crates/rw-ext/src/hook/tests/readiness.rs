//! Cold readiness and callbacks have distinct, shared clocks and one effect owner.
use super::{must, prompt};
use crate::PluginActivation;
use crate::hook::{
    HookClass, HookDirective, HookDispatcher, HookError, HookEvent, HookHandler, HookInput,
    HookInvocation, HookReadiness, HookRegistration, HookTransform,
};
use async_trait::async_trait;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;

struct Native {
    ready: AtomicBool,
    preparations: AtomicUsize,
    calls: AtomicUsize,
    readiness: Duration,
    callback: Duration,
    directive: HookDirective,
}
impl Native {
    fn new(readiness: u64, callback: u64) -> Arc<Self> {
        Arc::new(Self {
            ready: AtomicBool::new(false),
            preparations: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            readiness: Duration::from_secs(readiness),
            callback: Duration::from_secs(callback),
            directive: HookDirective::Continue {},
        })
    }
}
#[async_trait]
impl HookReadiness for Native {
    async fn ready(&self, _: &PluginActivation) -> Result<(), HookError> {
        self.preparations.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.readiness).await;
        self.ready.store(true, Ordering::SeqCst);
        Ok(())
    }
}
#[async_trait]
impl HookHandler for Native {
    fn readiness(&self) -> Option<&dyn HookReadiness> {
        if self.ready.load(Ordering::SeqCst) {
            None
        } else {
            Some(self)
        }
    }
    async fn invoke(&self, _: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        assert!(self.ready.load(Ordering::SeqCst));
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.callback).await;
        Ok(self.directive.clone())
    }
    async fn settle_effects(&self) -> Result<(), HookError> {
        Ok(())
    }
}
fn policy(id: &str) -> HookRegistration {
    HookRegistration::new(id, HookEvent::UserPromptSubmit, HookClass::Policy)
}

#[tokio::test(start_paused = true)]
async fn cold_readiness_exceeds_callback_budget_and_warm_dispatch_has_no_readiness() {
    let native = Native::new(6, 4);
    let mut dispatcher = HookDispatcher::new();
    must(dispatcher.register_shared(policy("native"), native.clone()));
    for expected in [10, 4] {
        let began = Instant::now();
        assert!(must(dispatcher.dispatch(prompt("readiness")).await).completed());
        assert_eq!(began.elapsed(), Duration::from_secs(expected));
    }
    assert_eq!(native.preparations.load(Ordering::SeqCst), 1);
    assert_eq!(native.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn two_cold_handlers_keep_one_five_second_callback_allowance() {
    let mut dispatcher = HookDispatcher::new();
    let first = Native::new(6, 3);
    let second = Native::new(6, 3);
    must(dispatcher.register_shared(policy("a"), first.clone()));
    must(dispatcher.register_shared(policy("b"), second.clone()));
    let began = Instant::now();
    let result = must(dispatcher.dispatch(prompt("shared clock")).await);
    assert!(!result.completed());
    assert_eq!(result.failures()[0].hook_id(), "b");
    assert_eq!(began.elapsed(), Duration::from_secs(17));
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn selected_generations_share_one_absolute_readiness_deadline() {
    let mut dispatcher = HookDispatcher::new();
    let first = Native::new(20, 1);
    let second = Native::new(20, 1);
    must(dispatcher.register_shared(policy("a"), first.clone()));
    must(dispatcher.register_shared(policy("b"), second.clone()));
    let began = Instant::now();
    let result = must(dispatcher.dispatch(prompt("one readiness clock")).await);
    assert!(!result.completed());
    assert_eq!(result.failures()[0].hook_id(), "b");
    assert_eq!(began.elapsed(), Duration::from_secs(30));
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn ready_handlers_are_not_rejected_when_readiness_clock_expires() {
    let mut dispatcher = HookDispatcher::new();
    let cold = Native::new(29, 2);
    let warm = Native::new(0, 2);
    warm.ready.store(true, Ordering::SeqCst);
    must(dispatcher.register_shared(policy("a"), cold.clone()));
    must(dispatcher.register_shared(policy("b"), warm.clone()));
    let began = Instant::now();
    assert!(
        must(
            dispatcher
                .dispatch(prompt("warm after absolute expiry"))
                .await
        )
        .completed()
    );
    assert_eq!(began.elapsed(), Duration::from_secs(33));
    assert_eq!(warm.preparations.load(Ordering::SeqCst), 0);
    assert_eq!(warm.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn transformed_tool_selects_readiness_lazily_and_keeps_prior_callback_time() {
    let mut dispatcher = HookDispatcher::new();
    let transform = Arc::new(Native {
        ready: AtomicBool::new(true),
        preparations: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        readiness: Duration::ZERO,
        callback: Duration::from_secs(3),
        directive: HookDirective::Transform {
            change: HookTransform::PreTool {
                name: "write".to_owned(),
                arguments: serde_json::json!({}),
            },
        },
    });
    must(dispatcher.register_shared(
        HookRegistration::new("transform", HookEvent::PreTool, HookClass::Transform),
        transform,
    ));
    let read = Native::new(20, 1);
    let write = Native::new(6, 3);
    for (name, native) in [("read", read.clone()), ("write", write.clone())] {
        must(
            dispatcher.register_shared(
                HookRegistration::new(name, HookEvent::PreTool, HookClass::Policy)
                    .with_applicable_tools([name]),
                native,
            ),
        );
    }
    let began = Instant::now();
    let result = must(
        dispatcher
            .dispatch(HookInput::PreTool(rw_types::hook_contract::HookToolInput {
                id: "call".to_owned(),
                name: "read".to_owned(),
                arguments: serde_json::json!({}),
            }))
            .await,
    );
    assert!(!result.completed());
    assert_eq!(result.failures()[0].hook_id(), "write");
    assert_eq!(began.elapsed(), Duration::from_secs(11));
    assert_eq!(read.preparations.load(Ordering::SeqCst), 0);
    assert_eq!(read.calls.load(Ordering::SeqCst), 0);
    assert_eq!(write.preparations.load(Ordering::SeqCst), 1);
    assert_eq!(write.calls.load(Ordering::SeqCst), 1);
}

struct HeldReadiness {
    started: tokio::sync::Notify,
    cancelled: Arc<AtomicBool>,
    release: tokio::sync::Notify,
    token: std::sync::Mutex<Option<rw_tools::CancellationToken>>,
}
#[async_trait]
impl HookReadiness for HeldReadiness {
    async fn ready(&self, activation: &PluginActivation) -> Result<(), HookError> {
        *self
            .token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(activation.cancellation().clone());
        self.started.notify_one();
        std::future::pending().await
    }
}
#[async_trait]
impl HookHandler for HeldReadiness {
    fn readiness(&self) -> Option<&dyn HookReadiness> {
        Some(self)
    }
    async fn invoke(&self, _: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        panic!("readiness never completed")
    }
    async fn settle_effects(&self) -> Result<(), HookError> {
        let token = self
            .token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(token.is_some_and(|token| token.is_cancelled()));
        self.cancelled.store(true, Ordering::SeqCst);
        self.release.notified().await;
        Ok(())
    }
}
#[tokio::test(start_paused = true)]
async fn dropped_readiness_keeps_settlement_owned_until_effects_retire() {
    let handler = Arc::new(HeldReadiness {
        started: tokio::sync::Notify::new(),
        cancelled: Arc::new(AtomicBool::new(false)),
        release: tokio::sync::Notify::new(),
        token: std::sync::Mutex::new(None),
    });
    let mut dispatcher = HookDispatcher::new();
    must(dispatcher.register_shared(policy("held"), handler.clone()));
    let dispatcher = Arc::new(dispatcher);
    let owned = Arc::clone(&dispatcher);
    let task = tokio::spawn(async move { owned.dispatch(prompt("drop readiness")).await });
    handler.started.notified().await;
    task.abort();
    assert!(must(task.await.err().ok_or("expected cancellation")).is_cancelled());
    let settling = dispatcher.settle_effects(HookEvent::UserPromptSubmit);
    tokio::pin!(settling);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut settling)
            .await
            .is_err()
    );
    assert!(handler.cancelled.load(Ordering::SeqCst));
    handler.release.notify_one();
    must(settling.await);
}
