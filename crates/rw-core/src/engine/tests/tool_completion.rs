#![cfg(test)]
//! Each tool's completion reaches clients when that tool finishes, while the
//! durable results of one batch remain journaled in call order.

use crate::engine::builtin_hook_dispatcher;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::descriptor;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::tool_script;
use crate::engine::tests::fixtures::tools::StubOutcome;
use crate::engine::tests::fixtures::tools::StubTool;
use async_trait::async_trait;
use rw_tools::CapabilityManifest;
use rw_tools::Tool;
use rw_tools::ToolContext;
use rw_tools::ToolDescriptor;
use rw_tools::ToolError;
use rw_tools::ToolRegistry;
use rw_tools::ToolResult;
use rw_types::ApprovalDecision;
use rw_types::EngineEvent;
use rw_types::ToolCapability;
use rw_types::config::PermissionDecision;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::time::timeout;

/// Tool that optionally waits for the test to release it.
struct GatedTool {
    descriptor: ToolDescriptor,
    gate: Option<Arc<Notify>>,
}

#[async_trait]
impl Tool for GatedTool {
    async fn settle_effects(&self) -> std::result::Result<(), ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn execute(
        &self,
        _context: &ToolContext,
        _input: Value,
    ) -> Result<ToolResult, ToolError> {
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        Ok(ToolResult::new(&self.descriptor.name, Value::Null))
    }
}

fn gated(name: &str, gate: Option<Arc<Notify>>) -> Arc<GatedTool> {
    Arc::new(GatedTool {
        descriptor: descriptor(name),
        gate,
    })
}

fn gated_writer(name: &str, gate: Arc<Notify>) -> Arc<GatedTool> {
    let mut descriptor = descriptor(name);
    descriptor.capabilities = CapabilityManifest::new([ToolCapability::WriteFilesystem]);
    Arc::new(GatedTool {
        descriptor,
        gate: Some(gate),
    })
}

async fn next_event(events: &mut crate::engine::SessionSubscription) -> EngineEvent {
    timeout(Duration::from_secs(3), events.recv())
        .await
        .expect("event timeout")
        .expect("event channel")
        .as_ref()
        .clone()
}

fn finished_call(event: &EngineEvent) -> Option<(&str, u32)> {
    match event {
        EngineEvent::ToolCallFinished {
            tool_call_id,
            call_index,
            ..
        } => Some((tool_call_id.0.as_str(), *call_index)),
        _ => None,
    }
}

#[tokio::test]
async fn earlier_calls_run_and_finish_while_a_later_call_awaits_approval() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[
                ("read-a", "read_a", json!({})),
                ("read-b", "read_b", json!({})),
                ("write", "writer", json!({"path": "a"})),
            ],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let writer = Arc::new(StubTool::new(
        "writer",
        vec![ToolCapability::WriteFilesystem],
        StubOutcome::Success(ToolResult::new("written", Value::Null)),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(gated("read_a", None)).expect("read a");
    tools.register(gated("read_b", None)).expect("read b");
    tools.register(writer.clone()).expect("writer");
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Ask,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let mut events = handle.subscribe_live().expect("subscription");
    handle.send_message("run").await.expect("message");
    let mut approval = None;
    let mut live = Vec::new();
    let mut durable = Vec::new();
    // Nothing is approved inside this loop: both reads must settle durably
    // while the writer's approval prompt is still open.
    while approval.is_none() || durable.len() < 2 || live.len() < 2 {
        let event = next_event(&mut events).await;
        match &event {
            EngineEvent::ToolApprovalNeeded {
                tool_call_id,
                invocation_id,
                ..
            } => approval = Some((tool_call_id.0.clone(), invocation_id.clone())),
            EngineEvent::ToolExecutionFinished {
                tool_call_id,
                is_error,
                ..
            } => {
                assert!(!is_error);
                live.push(tool_call_id.0.clone());
            }
            EngineEvent::TurnFinished { .. } => panic!("turn ended before approval"),
            _ => {}
        }
        if let Some((id, index)) = finished_call(&event) {
            durable.push((id.to_owned(), index));
        }
    }
    assert_eq!(
        durable,
        vec![("read-a".to_owned(), 0), ("read-b".to_owned(), 1)]
    );
    assert_eq!(writer.calls.load(Ordering::SeqCst), 0);
    let (id, invocation_id) = approval.expect("writer approval");
    assert_eq!(id, "write");
    assert!(
        handle
            .approve(id, invocation_id, ApprovalDecision::AllowOnce)
            .await
            .expect("approval")
    );
    loop {
        let event = next_event(&mut events).await;
        if let Some((id, index)) = finished_call(&event) {
            durable.push((id.to_owned(), index));
        }
        if matches!(event, EngineEvent::TurnFinished { .. }) {
            break;
        }
    }
    assert_eq!(durable.last(), Some(&("write".to_owned(), 2)));
    assert_eq!(writer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn later_parallel_call_reports_completion_before_its_ordered_result() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[
                ("slow-id", "slow", json!({})),
                ("fast-id", "fast", json!({})),
            ],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let release = Arc::new(Notify::new());
    let mut tools = ToolRegistry::new();
    tools
        .register(gated("slow", Some(release.clone())))
        .expect("slow");
    tools.register(gated("fast", None)).expect("fast");
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let mut events = handle.subscribe_live().expect("subscription");
    handle.send_message("run").await.expect("message");
    loop {
        let event = next_event(&mut events).await;
        assert!(
            finished_call(&event).is_none(),
            "no durable result may precede the blocked first call"
        );
        if let EngineEvent::ToolExecutionFinished {
            tool_call_id,
            is_error,
            finished_at,
            ..
        } = &event
        {
            assert_eq!(tool_call_id.0, "fast-id");
            assert!(!is_error);
            assert!(!finished_at.is_empty());
            break;
        }
    }
    release.notify_one();
    let mut durable = Vec::new();
    let mut live_slow = false;
    loop {
        let event = next_event(&mut events).await;
        if let Some((id, index)) = finished_call(&event) {
            durable.push((id.to_owned(), index));
        }
        if let EngineEvent::ToolExecutionFinished { tool_call_id, .. } = &event {
            assert_eq!(tool_call_id.0, "slow-id");
            assert!(
                durable.is_empty(),
                "live completion precedes the durable result"
            );
            live_slow = true;
        }
        if matches!(event, EngineEvent::TurnFinished { .. }) {
            break;
        }
    }
    assert!(live_slow);
    assert_eq!(
        durable,
        vec![("slow-id".to_owned(), 0), ("fast-id".to_owned(), 1)]
    );
}

#[tokio::test]
async fn later_call_is_not_prepared_while_an_earlier_mutation_runs() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[
                ("write-id", "write", json!({})),
                ("read-id", "read", json!({})),
            ],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let release = Arc::new(Notify::new());
    let mut tools = ToolRegistry::new();
    tools
        .register(gated_writer("write", release.clone()))
        .expect("write");
    tools.register(gated("read", None)).expect("read");
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let mut events = handle.subscribe_live().expect("subscription");
    handle.send_message("run").await.expect("message");
    let mut order = Vec::new();
    loop {
        let event = next_event(&mut events).await;
        if let EngineEvent::ToolCallStarted { tool_call_id, .. } = &event {
            order.push(format!("start {}", tool_call_id.0));
            if tool_call_id.0 == "write-id" {
                // The writer is blocked; the read must stay unprepared.
                assert!(
                    timeout(Duration::from_millis(100), async {
                        loop {
                            if matches!(
                                next_event(&mut events).await,
                                EngineEvent::ToolCallStarted { .. }
                            ) {
                                break;
                            }
                        }
                    })
                    .await
                    .is_err(),
                    "no later call starts preparation during a mutation"
                );
                release.notify_one();
            }
        }
        if let EngineEvent::ToolExecutionFinished { tool_call_id, .. } = &event {
            order.push(format!("done {}", tool_call_id.0));
        }
        if matches!(event, EngineEvent::TurnFinished { .. }) {
            break;
        }
    }
    assert_eq!(
        order,
        [
            "start write-id",
            "done write-id",
            "start read-id",
            "done read-id"
        ]
    );
}
