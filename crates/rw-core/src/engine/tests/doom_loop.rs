#![cfg(test)]

use crate::engine::AgentTurnStatus;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::pending_event::PendingEvent;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::tool_script;
use crate::engine::tests::fixtures::tools::StubOutcome;
use crate::engine::tests::fixtures::tools::StubTool;
use rw_tools::ToolRegistry;
use rw_tools::ToolResult;
use rw_types::config::PermissionDecision;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
async fn identical_failures_and_max_turns_stop_deterministically() {
    let root = TempDir::new().expect("tempdir");
    let repeated = (0..5)
        .map(|index| {
            tool_script(
                &[(&format!("call-{index}"), "failing", json!({"same": true}))],
                &[],
            )
        })
        .collect::<Vec<_>>();
    let doom_model = Arc::new(ScriptedModel::new(repeated));
    let failing = Arc::new(StubTool::new(
        "failing",
        vec![],
        StubOutcome::Failure("same failure".to_owned()),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(failing).expect("register tool");
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        doom_model.clone(),
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let events = collect_turn(&mut events).await;
    assert_eq!(doom_model.request_count(), 5);
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::GuardTriggered { guard, .. }
            if guard == "identical_tool_failure"
    )));
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::DoomLoop,
            ..
        })
    ));

    let root = TempDir::new().expect("tempdir");
    let max_model = Arc::new(ScriptedModel::new((0..2).map(|index| {
        tool_script(
            &[(&format!("call-{index}"), "ok", json!({"index": index}))],
            &[],
        )
    })));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StubTool::new(
            "ok",
            vec![],
            StubOutcome::Success(ToolResult::new("ok", Value::Null)),
        )))
        .expect("register tool");
    let mut actor_config = config(
        root.path(),
        max_model.clone(),
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.max_turns = 2;
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let events = collect_turn(&mut events).await;
    assert_eq!(max_model.request_count(), 2);
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::GuardTriggered { guard, .. } if guard == "max_turns"
    )));
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::MaxTurns,
            ..
        })
    ));
}

#[tokio::test]
async fn alternating_failures_trigger_the_doom_loop_guard() {
    let root = TempDir::new().expect("tempdir");
    let scripts = (0..9)
        .map(|index| {
            let name = if index % 2 == 0 {
                "failing_a"
            } else {
                "failing_b"
            };
            tool_script(
                &[(&format!("call-{index}"), name, json!({"same": true}))],
                &[],
            )
        })
        .collect::<Vec<_>>();
    let model = Arc::new(ScriptedModel::new(scripts));
    let mut tools = ToolRegistry::new();
    for (name, message) in [("failing_a", "failure a"), ("failing_b", "failure b")] {
        tools
            .register(Arc::new(StubTool::new(
                name,
                vec![],
                StubOutcome::Failure(message.to_owned()),
            )))
            .expect("register tool");
    }
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        model.clone(),
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let events = collect_turn(&mut events).await;
    assert_eq!(model.request_count(), 9);
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::DoomLoop,
            ..
        })
    ));
}

#[tokio::test]
async fn successful_calls_do_not_clear_repeated_failure_history() {
    let root = TempDir::new().expect("tempdir");
    let scripts = (0..9)
        .map(|index| {
            let name = if index % 2 == 0 { "failing" } else { "ok" };
            tool_script(
                &[(&format!("call-{index}"), name, json!({"same": true}))],
                &[],
            )
        })
        .collect::<Vec<_>>();
    let model = Arc::new(ScriptedModel::new(scripts));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StubTool::new(
            "failing",
            vec![],
            StubOutcome::Failure("same failure".to_owned()),
        )))
        .expect("register failing tool");
    tools
        .register(Arc::new(StubTool::new(
            "ok",
            vec![],
            StubOutcome::Success(ToolResult::new("ok", Value::Null)),
        )))
        .expect("register successful tool");
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        model.clone(),
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let events = collect_turn(&mut events).await;
    assert_eq!(model.request_count(), 9);
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::DoomLoop,
            ..
        })
    ));
}

#[tokio::test]
async fn repeated_failures_decay_outside_the_doom_loop_window() {
    let root = TempDir::new().expect("tempdir");
    let scripts = (0..21)
        .map(|index| {
            let name = if index % 5 == 0 { "failing" } else { "ok" };
            tool_script(
                &[(&format!("call-{index}"), name, json!({"same": true}))],
                &[],
            )
        })
        .chain(std::iter::once(stop_script("done", &[])))
        .collect::<Vec<_>>();
    let model = Arc::new(ScriptedModel::new(scripts));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StubTool::new(
            "failing",
            vec![],
            StubOutcome::Failure("same failure".to_owned()),
        )))
        .expect("register failing tool");
    tools
        .register(Arc::new(StubTool::new(
            "ok",
            vec![],
            StubOutcome::Success(ToolResult::new("ok", Value::Null)),
        )))
        .expect("register successful tool");
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.max_turns = 22;
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let events = collect_turn(&mut events).await;
    assert_eq!(model.request_count(), 22);
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::Completed,
            ..
        })
    ));
}
