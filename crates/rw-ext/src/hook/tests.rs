use super::*;
use async_trait::async_trait;
use rw_types::hook_contract::{HookPermissionInput, HookPromptInput};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

struct Fixed {
    name: &'static str,
    calls: Arc<Mutex<Vec<(String, HookInput)>>>,
    directive: Result<HookDirective, HookError>,
}
#[async_trait]
impl HookHandler for Fixed {
    async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((self.name.to_owned(), invocation.input().clone()));
        self.directive.clone()
    }
    async fn settle_effects(&self) -> Result<(), HookError> {
        Ok(())
    }
}
fn fixed(
    name: &'static str,
    calls: &Arc<Mutex<Vec<(String, HookInput)>>>,
    directive: Result<HookDirective, HookError>,
) -> Fixed {
    Fixed {
        name,
        calls: Arc::clone(calls),
        directive,
    }
}
fn prompt(content: &str) -> HookInput {
    HookInput::UserPromptSubmit(HookPromptInput {
        content: content.to_owned(),
    })
}
fn transform(content: &str) -> HookDirective {
    HookDirective::Transform {
        change: HookTransform::UserPromptSubmit {
            content: content.to_owned(),
        },
    }
}
fn must<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("unexpected failure: {error:?}"),
    }
}

#[tokio::test]
async fn transforms_precede_policy_and_observation_then_priority_and_id() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut dispatcher = HookDispatcher::new();
    for (name, class, priority, directive) in [
        (
            "observer",
            HookClass::Observer,
            i32::MIN,
            HookDirective::Continue {},
        ),
        (
            "policy",
            HookClass::Policy,
            i32::MIN,
            HookDirective::Continue {},
        ),
        ("b", HookClass::Transform, 0, transform("second")),
        ("z", HookClass::Transform, 1, HookDirective::Continue {}),
        ("a", HookClass::Transform, 0, transform("first")),
    ] {
        must(dispatcher.register(
            HookRegistration::new(name, HookEvent::UserPromptSubmit, class).with_priority(priority),
            fixed(name, &calls, Ok(directive)),
        ));
    }
    let result = must(dispatcher.dispatch(prompt("original")).await);
    assert!(result.completed());
    assert_eq!(result.input(), &prompt("second"));
    let calls = calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        calls
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "z", "policy", "observer"]
    );
    assert_eq!(calls[0].1, prompt("original"));
    assert_eq!(calls[1].1, prompt("first"));
    assert_eq!(calls[3].1, prompt("second"));
}

#[tokio::test]
async fn permission_decisions_fold_to_the_most_restrictive_and_deny_stops_dispatch() {
    for (decisions, expected, count) in [
        (
            vec![
                HookPermissionDecision::Allow,
                HookPermissionDecision::Ask,
                HookPermissionDecision::Allow,
            ],
            HookPermissionDecision::Ask,
            3,
        ),
        (
            vec![HookPermissionDecision::Deny, HookPermissionDecision::Allow],
            HookPermissionDecision::Deny,
            1,
        ),
    ] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = HookDispatcher::new();
        for (index, value) in decisions.into_iter().enumerate() {
            must(dispatcher.register(
                HookRegistration::new(
                    format!("policy-{index}"),
                    HookEvent::PermissionCheck,
                    HookClass::Policy,
                ),
                fixed("policy", &calls, Ok(HookDirective::Permission { value })),
            ));
        }
        let result = must(
            dispatcher
                .dispatch(HookInput::PermissionCheck(HookPermissionInput {
                    id: "call".to_owned(),
                    name: "bash".to_owned(),
                    arguments: json!({}),
                    capabilities: vec![],
                }))
                .await,
        );
        assert_eq!(result.permission(), Some(expected));
        assert_eq!(
            calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            count
        );
    }
}

#[tokio::test]
async fn observer_cannot_mutate_and_transform_cannot_block() {
    for (class, directive) in [
        (HookClass::Observer, transform("forbidden")),
        (
            HookClass::Transform,
            HookDirective::Block {
                message: "forbidden".to_owned(),
            },
        ),
    ] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = HookDispatcher::new();
        must(
            dispatcher.register(
                HookRegistration::new("invalid", HookEvent::UserPromptSubmit, class)
                    .with_failure_policy(HookFailurePolicy::FailClosed),
                fixed("invalid", &calls, Ok(directive)),
            ),
        );
        let result = must(dispatcher.dispatch(prompt("input")).await);
        assert!(!result.completed());
        assert_eq!(result.input(), &prompt("input"));
        assert_eq!(result.failures().len(), 1);
    }
}

#[tokio::test]
async fn failed_open_transform_leaves_input_intact_and_policy_still_runs() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut dispatcher = HookDispatcher::new();
    must(
        dispatcher.register(
            HookRegistration::new("bad", HookEvent::UserPromptSubmit, HookClass::Transform)
                .with_failure_policy(HookFailurePolicy::FailOpen),
            fixed("bad", &calls, Err(HookError::new("failed", "failure"))),
        ),
    );
    must(dispatcher.register(
        HookRegistration::new("policy", HookEvent::UserPromptSubmit, HookClass::Policy),
        fixed("policy", &calls, Ok(HookDirective::Continue {})),
    ));
    let result = must(dispatcher.dispatch(prompt("input")).await);
    assert!(result.completed());
    assert_eq!(result.failures().len(), 1);
    assert_eq!(
        calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[1]
            .1,
        prompt("input")
    );
}

#[tokio::test]
async fn malformed_or_oversized_transform_is_rejected_before_policy_observes_it() {
    for directive in [
        HookDirective::Transform {
            change: HookTransform::PreTool {
                name: "bad".to_owned(),
                arguments: json!({}),
            },
        },
        transform(&"x".repeat(rw_plugin_protocol::MAX_HOOK_PAYLOAD_BYTES + 1)),
    ] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = HookDispatcher::new();
        must(dispatcher.register(
            HookRegistration::new(
                "transform",
                HookEvent::UserPromptSubmit,
                HookClass::Transform,
            ),
            fixed("transform", &calls, Ok(directive)),
        ));
        let result = must(dispatcher.dispatch(prompt("input")).await);
        assert!(!result.completed());
        assert_eq!(result.input(), &prompt("input"));
    }
}

struct Cleanup {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    cleaned: Arc<AtomicBool>,
    fail: bool,
}
#[async_trait]
impl HookHandler for Cleanup {
    async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        self.started.notify_one();
        invocation.cancellation().cancelled().await;
        Ok(HookDirective::Continue {})
    }
    async fn settle_effects(&self) -> Result<(), HookError> {
        self.release.notified().await;
        self.cleaned.store(true, Ordering::SeqCst);
        if self.fail {
            Err(HookError::new("failed", "effect proof failed"))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn dropping_dispatch_retains_cleanup_and_prevents_success_until_settled() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let cleaned = Arc::new(AtomicBool::new(false));
    let mut dispatcher = HookDispatcher::new();
    must(dispatcher.register(
        HookRegistration::new("cleanup", HookEvent::UserPromptSubmit, HookClass::Observer),
        Cleanup {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            cleaned: Arc::clone(&cleaned),
            fail: false,
        },
    ));
    let dispatcher = Arc::new(dispatcher);
    let task_dispatcher = Arc::clone(&dispatcher);
    let task = tokio::spawn(async move { task_dispatcher.dispatch(prompt("input")).await });
    started.notified().await;
    task.abort();
    let _ = task.await;
    let settling = dispatcher.settle_effects(HookEvent::UserPromptSubmit);
    tokio::pin!(settling);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut settling)
            .await
            .is_err()
    );
    assert!(!cleaned.load(Ordering::SeqCst));
    release.notify_one();
    must(settling.await);
    assert!(cleaned.load(Ordering::SeqCst));
}

#[tokio::test]
async fn failed_settlement_is_fatal_even_for_fail_open_and_closes_admission() {
    let release = Arc::new(tokio::sync::Notify::new());
    release.notify_one();
    let mut dispatcher = HookDispatcher::new();
    must(
        dispatcher.register(
            HookRegistration::new("cleanup", HookEvent::UserPromptSubmit, HookClass::Observer)
                .with_timeout(Duration::from_millis(1)),
            Cleanup {
                started: Arc::new(tokio::sync::Notify::new()),
                release,
                cleaned: Arc::new(AtomicBool::new(false)),
                fail: true,
            },
        ),
    );
    for _ in 0..2 {
        let Err(error) = dispatcher.dispatch(prompt("input")).await else {
            panic!("unsettled hook completed");
        };
        assert_eq!(error.code(), "effects_unsettled");
    }
}

struct Slow(Arc<AtomicBool>);
#[async_trait]
impl HookHandler for Slow {
    async fn invoke(&self, _: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        tokio::time::sleep(Duration::from_millis(30)).await;
        self.0.store(true, Ordering::SeqCst);
        Ok(HookDirective::Continue {})
    }
    async fn settle_effects(&self) -> Result<(), HookError> {
        Ok(())
    }
}
#[tokio::test(start_paused = true)]
async fn phase_budget_does_not_multiply_by_handler_count() {
    let first = Arc::new(AtomicBool::new(false));
    let second = Arc::new(AtomicBool::new(false));
    let mut dispatcher = HookDispatcher::new();
    for (id, done) in [("a", &first), ("b", &second)] {
        must(
            dispatcher.register(
                HookRegistration::new(id, HookEvent::UserPromptSubmit, HookClass::Policy)
                    .with_timeout(Duration::from_millis(40)),
                Slow(Arc::clone(done)),
            ),
        );
    }
    let result = must(dispatcher.dispatch(prompt("input")).await);
    assert!(!result.completed());
    assert!(first.load(Ordering::SeqCst));
    assert_eq!(result.failures().len(), 1);
    assert_eq!(result.failures()[0].hook_id(), "b");
}

#[test]
fn registration_rejects_invalid_classes_policies_effects_ids_and_duplicates() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut dispatcher = HookDispatcher::new();
    for registration in [
        HookRegistration::new("x", HookEvent::TurnEnd, HookClass::Transform),
        HookRegistration::new("x", HookEvent::PreTool, HookClass::Policy)
            .with_failure_policy(HookFailurePolicy::FailOpen),
        HookRegistration::new("x", HookEvent::PreTool, HookClass::Observer)
            .with_effect(HookEffect::WorkspaceMutating),
        HookRegistration::new("bad\nid", HookEvent::PreTool, HookClass::Observer),
        HookRegistration::new("x", HookEvent::PreTool, HookClass::Observer)
            .with_timeout(Duration::ZERO),
    ] {
        assert!(
            dispatcher
                .register(
                    registration,
                    fixed("x", &calls, Ok(HookDirective::Continue {}))
                )
                .is_err()
        );
    }
    for event in [HookEvent::SessionStart, HookEvent::SessionEnd] {
        must(dispatcher.register(
            HookRegistration::new("same", event, HookClass::Observer),
            fixed("x", &calls, Ok(HookDirective::Continue {})),
        ));
    }
    assert!(
        dispatcher
            .register(
                HookRegistration::new("same", HookEvent::SessionStart, HookClass::Observer),
                fixed("x", &calls, Ok(HookDirective::Continue {}))
            )
            .is_err()
    );
}

struct StalledCleanup;
#[async_trait]
impl HookHandler for StalledCleanup {
    async fn invoke(&self, _: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        Ok(HookDirective::Continue {})
    }
    async fn settle_effects(&self) -> Result<(), HookError> {
        std::future::pending().await
    }
}
#[tokio::test(start_paused = true)]
async fn settlement_timeout_closes_admission_even_when_handler_returned_successfully() {
    let mut dispatcher = HookDispatcher::new();
    must(
        dispatcher.register(
            HookRegistration::new("stalled", HookEvent::UserPromptSubmit, HookClass::Observer)
                .with_timeout(Duration::from_mins(10)),
            StalledCleanup,
        ),
    );
    let started = Instant::now();
    for _ in 0..2 {
        let Err(error) = dispatcher.dispatch(prompt("input")).await else {
            panic!("unsettled hook completed");
        };
        assert_eq!(error.code(), "effects_unsettled");
    }
    assert_eq!(started.elapsed(), HOOK_SETTLEMENT_TIMEOUT);
}
