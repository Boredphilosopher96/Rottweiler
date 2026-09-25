//! Background child agents: parents keep working, results arrive once, idle parents wake.
use super::fixtures::{
    models::M3Model,
    support::{collect_turn, config, fixture_subagent_result, stop_script, tool_script},
    tools::{StubOutcome, StubTool},
};
use crate::engine::pending_event::PendingEvent;
use async_trait::async_trait;
use rw_tools::{
    CapabilityManifest, SessionActivity, SubagentLifecycleEvent, Tool, ToolContext, ToolDescriptor,
    ToolError, ToolRegistry, ToolResult,
};
use rw_types::{SessionId, SubagentId, ToolCapability, config::PermissionDecision};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

fn spawned(id: &str) -> SubagentLifecycleEvent {
    let result = fixture_subagent_result(id);
    SubagentLifecycleEvent::Spawned {
        subagent_id: result.subagent_id,
        child_session_id: result.session_id,
        task: format!("task {id}"),
    }
}

fn finished(id: &str) -> SubagentLifecycleEvent {
    let mut result = fixture_subagent_result(id);
    result.final_text = format!("report from {id}");
    SubagentLifecycleEvent::Finished {
        subagent_id: SubagentId(id.to_owned()),
        result: Box::new(result),
    }
}

fn deliveries(request: &rw_providers::ProviderRequest, id: &str) -> usize {
    let tag = format!("<child-agent-result id=\"{id}\"");
    request
        .turns
        .iter()
        .flat_map(|turn| &turn.blocks)
        .filter(|block| matches!(block, rw_types::Block::Text { text } if text.starts_with(&tag)))
        .count()
}

async fn quiesce(model: &M3Model, expected: usize) {
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(model.requests().len(), expected, "no unsolicited turn");
}

#[tokio::test]
async fn an_idle_parent_wakes_once_for_each_background_result() {
    let root = tempfile::tempdir().expect("workspace");
    let model = Arc::new(M3Model::new([
        stop_script("parent continues", &[]),
        stop_script("first received", &[]),
        stop_script("second received", &[]),
    ]));
    let handle = super::fixtures::history::spawn(config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        crate::engine::builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let sink = handle.background_subagent_event_sink();
    let mut events = handle.subscribe().expect("events");
    handle.send_message("delegate").await.expect("send");
    collect_turn(&mut events).await;
    for id in ["first", "second"] {
        sink.lifecycle(spawned(id)).await.expect("spawn");
    }
    quiesce(&model, 1).await;

    sink.background_finished(finished("first"))
        .await
        .expect("finish first");
    collect_turn(&mut events).await;
    let requests = model.requests();
    assert_eq!(requests.len(), 2, "the idle parent woke for the result");
    assert_eq!(deliveries(&requests[1], "first"), 1);
    assert_eq!(deliveries(&requests[1], "second"), 0);
    let delivered = requests[1].turns.last().expect("delivery");
    assert_eq!(delivered.role, rw_types::Role::User);

    sink.background_finished(finished("second"))
        .await
        .expect("finish second");
    collect_turn(&mut events).await;
    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        deliveries(&requests[2], "first"),
        1,
        "a delivered result stays in history and is never repeated"
    );
    assert_eq!(deliveries(&requests[2], "second"), 1);
    quiesce(&model, 3).await;
    handle.close().await.expect("close");
}

/// Finishes a background child from inside a parent tool call.
struct FinishChild;

#[async_trait]
impl Tool for FinishChild {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "finish_child".into(),
            description: "fixture".into(),
            input_schema: json!({"type": "object"}),
            capabilities: CapabilityManifest::default(),
        }
    }
    async fn settle_effects(&self) -> Result<(), ToolError> {
        Ok(())
    }
    async fn execute(&self, context: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        let sink = context
            .background_subagent_event_sink()
            .ok_or_else(|| ToolError::Output("missing session sink".into()))?;
        sink.background_finished(finished("mid")).await?;
        Ok(ToolResult::new("child finished meanwhile", Value::Null))
    }
}

#[tokio::test]
async fn a_result_arriving_mid_turn_joins_the_next_call_once_without_a_wake() {
    let root = tempfile::tempdir().expect("workspace");
    let model = Arc::new(M3Model::new([
        tool_script(&[("call-1", "finish_child", json!({}))], &[]),
        stop_script("used the result", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(FinishChild)).expect("tool");
    let handle = super::fixtures::history::spawn(config(
        root.path(),
        model.clone(),
        Arc::new(tools),
        PermissionDecision::Allow,
        crate::engine::builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let sink = handle.background_subagent_event_sink();
    let mut events = handle.subscribe().expect("events");
    sink.lifecycle(spawned("mid")).await.expect("spawn");
    handle.send_message("keep working").await.expect("send");
    let turn = collect_turn(&mut events).await;
    assert!(turn.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::ConversationContextCommitted {
            selection: rw_types::conversation_input::ContextSelection::ChildResult { .. },
            ..
        }
    )));
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(deliveries(&requests[0], "mid"), 0);
    assert_eq!(deliveries(&requests[1], "mid"), 1);
    let tool_result = requests[1]
        .turns
        .iter()
        .position(|turn| turn.role == rw_types::Role::Tool)
        .expect("tool result");
    let result = requests[1]
        .turns
        .iter()
        .position(|turn| {
            turn.blocks.iter().any(|block| {
                matches!(block, rw_types::Block::Text { text } if text.contains("report from mid"))
            })
        })
        .expect("child result");
    assert!(
        tool_result < result,
        "the result follows the tool result it interrupted"
    );
    quiesce(&model, 2).await;
    handle.close().await.expect("close");
}

/// Reports whether this parent has an editing child in its shared workspace.
struct Children {
    shared_writer: AtomicBool,
}

#[async_trait]
impl Tool for Children {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "children".into(),
            description: "fixture".into(),
            input_schema: json!({"type": "object"}),
            capabilities: CapabilityManifest::default(),
        }
    }
    async fn settle_effects(&self) -> Result<(), ToolError> {
        Ok(())
    }
    fn observes_session_resources(&self) -> bool {
        true
    }
    fn session_activity(&self, _: &SessionId) -> Option<SessionActivity> {
        self.shared_writer
            .load(Ordering::SeqCst)
            .then_some(SessionActivity::SharedWorkspaceChild)
    }
    async fn execute(&self, _: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        unreachable!("fixture only reports activity")
    }
}

#[tokio::test]
async fn parent_edits_while_worktree_children_run_and_only_shared_editors_lock() {
    let root = tempfile::tempdir().expect("workspace");
    let model = Arc::new(M3Model::new([
        tool_script(&[("edit-1", "edit", json!({}))], &[]),
        stop_script("edited alongside children", &[]),
        tool_script(&[("edit-2", "edit", json!({}))], &[]),
        stop_script("blocked by the shared child", &[]),
    ]));
    let edit = Arc::new(StubTool::new(
        "edit",
        vec![ToolCapability::WriteFilesystem],
        StubOutcome::Success(ToolResult::new("edited", Value::Null)),
    ));
    let children = Arc::new(Children {
        shared_writer: AtomicBool::new(false),
    });
    let mut tools = ToolRegistry::new();
    tools.register(edit.clone()).expect("edit");
    tools.register(children.clone()).expect("children");
    let handle = super::fixtures::history::spawn(config(
        root.path(),
        model.clone(),
        Arc::new(tools),
        PermissionDecision::Allow,
        crate::engine::builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let sink = handle.background_subagent_event_sink();
    let mut events = handle.subscribe().expect("events");
    for id in ["worktree-a", "worktree-b"] {
        sink.lifecycle(spawned(id)).await.expect("spawn");
    }
    handle.send_message("edit now").await.expect("send");
    let turn = collect_turn(&mut events).await;
    assert!(turn.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::TextDelta { text, .. } if text == "edited alongside children"
    )));
    assert!(turn.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::ToolCallFinished { is_error: false, id, .. } if id == "edit-1"
    )));
    assert_eq!(edit.calls.load(Ordering::SeqCst), 1);

    children.shared_writer.store(true, Ordering::SeqCst);
    handle.send_message("edit again").await.expect("send");
    let turn = collect_turn(&mut events).await;
    assert!(turn.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::ToolCallFinished { is_error: true, id, output: rw_types::ToolOutput::Text { text }, .. }
            if id == "edit-2" && text.contains("a child agent is editing the shared workspace")
    )));
    assert_eq!(edit.calls.load(Ordering::SeqCst), 1);
    children.shared_writer.store(false, Ordering::SeqCst);
    handle.close().await.expect("close");
}
