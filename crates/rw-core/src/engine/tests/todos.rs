#![cfg(test)]
use super::fixtures::{
    hooks::MarkPostToolFailed,
    models::ScriptedModel,
    sinks::RecordingSink,
    support::{collect_turn, config, stop_script, tool_script},
};
use crate::engine::pending_event::PendingEvent;
use crate::engine::{SessionActor, SessionEventSink, builtin_hook_dispatcher};
use rw_ext::{HookEvent, HookRegistration};
use rw_tools::{TodoTool, ToolLimits, ToolRegistry};
use rw_types::{config::PermissionDecision, hook_contract::HookClass};
use serde_json::json;
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn task_commit_precedes_failed_result_transform_and_remains_authoritative() {
    let root = tempfile::tempdir().expect("root");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[(
                "task",
                "todo",
                json!({"action":"upsert","item":{"id":"a","content":"durable task","status":"pending"}}),
            )],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(TodoTool::new(ToolLimits::default())))
        .expect("tool");
    let mut hooks = builtin_hook_dispatcher().expect("hooks");
    hooks
        .register(
            HookRegistration::new(
                "task.presentation",
                HookEvent::PostTool,
                HookClass::Transform,
            ),
            MarkPostToolFailed,
        )
        .expect("transform");
    let sink = Arc::new(RecordingSink::default());
    let mut options = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        hooks,
    );
    options.event_sink = sink.clone();
    let actor = SessionActor::spawn(options).expect("actor");
    let mut events = actor.subscribe().expect("subscribe");
    actor.send_message("make a task").await.expect("message");
    let events = tokio::time::timeout(Duration::from_secs(3), collect_turn(&mut events))
        .await
        .expect("actor services task mailbox while turn runs");
    let committed = events
        .iter()
        .position(|event| matches!(event.kind, PendingEvent::TodoStateCommitted { .. }))
        .expect("durable task");
    let completed = events
        .iter()
        .position(|event| {
            matches!(
                event.kind,
                PendingEvent::ToolCallFinished { is_error: true, .. }
            )
        })
        .expect("transformed result");
    assert!(committed < completed);
    let snapshot = sink.todo_state().await.expect("authoritative task");
    assert_eq!(snapshot.items[0].content, "durable task");
    actor.close().await.expect("close");
}
