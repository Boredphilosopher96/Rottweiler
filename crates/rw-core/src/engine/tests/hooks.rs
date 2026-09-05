#![cfg(test)]

use crate::engine::AgentTurnStatus;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::model;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::hooks::FixedHook;
use crate::engine::tests::fixtures::hooks::NeverHook;
use crate::engine::tests::fixtures::hooks::RewriteArgumentsHook;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::tool_script;
use crate::engine::tests::fixtures::tools::StubOutcome;
use crate::engine::tests::fixtures::tools::StubTool;
use rw_ext::HookDirective;
use rw_ext::HookError;
use rw_ext::HookEvent;
use rw_ext::HookFailure;
use rw_ext::HookFailurePolicy;
use rw_ext::HookRegistration;
use rw_tools::ToolRegistry;
use rw_tools::ToolResult;
use rw_types::ApprovalDecision;
use rw_types::ToolCapability;
use rw_types::config::PermissionDecision;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[tokio::test]
async fn pre_tool_rewrite_is_the_exact_invocation_presented_for_approval() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(&[("call", "fixture", json!({"path": "original"}))], &[]),
        stop_script("done", &[]),
    ]));
    let tool = Arc::new(StubTool::new(
        "fixture",
        vec![ToolCapability::WriteFilesystem],
        StubOutcome::Success(ToolResult::new("ok", Value::Null)),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).expect("register tool");
    let mut hooks = builtin_hook_dispatcher().expect("hooks");
    hooks
        .register(
            HookRegistration::new("fixture.rewrite", HookEvent::PreTool),
            RewriteArgumentsHook(json!({"path": "rewritten"})),
        )
        .expect("rewrite hook");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Ask,
        hooks,
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let event = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::PermissionRequested { .. })
    })
    .await;
    let PendingEvent::PermissionRequested { request, .. } = event.kind else {
        unreachable!("matching event")
    };
    assert_eq!(request.arguments, json!({"path": "original"}));
    handle
        .approve(
            request.id,
            request.invocation_id.clone(),
            ApprovalDecision::AllowOnce,
        )
        .await
        .expect("initial approval");
    let event = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::PermissionRequested { .. })
    })
    .await;
    let PendingEvent::PermissionRequested { request, .. } = event.kind else {
        unreachable!("matching event")
    };
    assert_eq!(request.arguments, json!({"path": "rewritten"}));
    handle
        .approve(
            request.id,
            request.invocation_id.clone(),
            ApprovalDecision::AllowOnce,
        )
        .await
        .expect("approval");
    next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::TurnFinished { .. })
    })
    .await;
    assert_eq!(
        tool.inputs.lock().expect("input lock").as_slice(),
        &[json!({"path": "rewritten"})]
    );
}

#[tokio::test]
async fn hook_order_and_fail_open_closed_are_enforced_by_the_turn_loop() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(&[("call", "fixture", json!({}))], &[]),
        stop_script("done", &[]),
    ]));
    let tool = Arc::new(StubTool::new(
        "fixture",
        vec![],
        StubOutcome::Success(ToolResult::new("ok", Value::Null)),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).expect("register tool");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut hooks = builtin_hook_dispatcher().expect("hooks");
    hooks
        .register(
            HookRegistration::new("fixture.open", HookEvent::PreTool)
                .with_priority(-10)
                .with_failure_policy(HookFailurePolicy::FailOpen),
            FixedHook {
                label: "open",
                calls: calls.clone(),
                result: Err(HookError::new("fixture", "open failure")),
            },
        )
        .expect("open hook");
    hooks
        .register(
            HookRegistration::new("fixture.middle", HookEvent::PreTool),
            FixedHook {
                label: "middle",
                calls: calls.clone(),
                result: Ok(HookDirective::Continue),
            },
        )
        .expect("middle hook");
    hooks
        .register(
            HookRegistration::new("fixture.closed", HookEvent::PreTool)
                .with_priority(10)
                .with_failure_policy(HookFailurePolicy::FailClosed),
            FixedHook {
                label: "closed",
                calls: calls.clone(),
                result: Err(HookError::new("fixture", "closed failure")),
            },
        )
        .expect("closed hook");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        hooks,
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let events = collect_turn(&mut events).await;
    assert_eq!(
        calls.lock().expect("hook calls").as_slice(),
        &["open", "middle", "closed"]
    );
    let failures = events
        .iter()
        .filter_map(|event| match &event.kind {
            PendingEvent::HookFailure {
                hook_id,
                fail_closed,
                ..
            } => Some((hook_id.as_str(), *fail_closed)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        failures,
        vec![("fixture.open", false), ("fixture.closed", true)]
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn session_lifecycle_hooks_run_on_start_and_actor_shutdown() {
    let root = TempDir::new().expect("tempdir");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut hooks = builtin_hook_dispatcher().expect("hooks");
    for (id, event, label) in [
        ("fixture.session-start", HookEvent::SessionStart, "start"),
        ("fixture.session-end", HookEvent::SessionEnd, "end"),
    ] {
        hooks
            .register(
                HookRegistration::new(id, event).with_failure_policy(HookFailurePolicy::FailClosed),
                FixedHook {
                    label,
                    calls: calls.clone(),
                    result: Ok(HookDirective::Continue),
                },
            )
            .expect("lifecycle hook");
    }
    let handle = SessionActor::spawn(config(
        root.path(),
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        hooks,
    ))
    .expect("actor");
    handle.send_message("/status").await.expect("status");
    assert_eq!(
        calls.lock().expect("lifecycle calls").as_slice(),
        &["start"]
    );
    drop(handle);
    timeout(Duration::from_secs(3), async {
        loop {
            if calls.lock().expect("lifecycle calls").as_slice() == ["start", "end"] {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("session end hook");
}

#[tokio::test]
async fn interrupt_cancels_a_hung_session_hook_without_waiting_for_its_deadline() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(RecordingSink::default());
    let mut hooks = builtin_hook_dispatcher().expect("hooks");
    hooks
        .register(
            HookRegistration::new("fixture.never", HookEvent::UserPromptSubmit)
                .with_failure_policy(HookFailurePolicy::FailClosed),
            NeverHook,
        )
        .expect("hung hook");
    let mut actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        hooks,
    );
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("hang").await.expect("message");
    assert!(handle.interrupt().await.expect("interrupt"));
    let finished = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::TurnFinished { .. })
    })
    .await;
    assert!(matches!(
        finished.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Interrupted,
            ..
        }
    ));
    assert_eq!(
        sink.batch_sizes.lock().expect("batch sizes").as_slice(),
        &[1, 2, 1]
    );
    assert!(
        !sink
            .events
            .lock()
            .expect("event sink lock")
            .iter()
            .any(|event| matches!(event.kind, PendingEvent::ConversationTurnCommitted { .. }))
    );
}
