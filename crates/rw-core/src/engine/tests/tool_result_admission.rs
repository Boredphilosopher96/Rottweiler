#![allow(clippy::expect_used)]
use super::fixtures::{
    models::ScriptedModel,
    sinks::RecordingSink,
    support::{TestEventSinkExt, config, tool_script},
    tools::{StubOutcome, StubTool},
};
use crate::engine::{AgentTurnStatus, builtin_hook_dispatcher, project_session_events};
use rw_tools::{ToolRegistry, ToolResult};
use rw_types::{Block, EngineEvent, Role, ToolOutput, config::PermissionDecision};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn excess_batch_output_is_rejected_before_completion_and_closes_exact_ir() {
    let root = tempfile::tempdir().expect("root");
    let model = Arc::new(ScriptedModel::new([tool_script(
        &[
            ("first", "large", serde_json::json!({})),
            ("second", "large", serde_json::json!({})),
        ],
        &[],
    )]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StubTool::new(
            "large",
            vec![],
            StubOutcome::Success(ToolResult::new(
                "x".repeat(9 * 1024 * 1024),
                serde_json::Value::Null,
            )),
        )))
        .expect("tool");
    let sink = Arc::new(RecordingSink::default());
    let mut config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    config.max_turns = 1;
    config.event_sink = sink.clone();
    let handle = super::fixtures::history::spawn(config)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run both").await.expect("message");
    // This acceptance includes three CPU-admitted encodings of multi-megabyte bodies.
    // It checks closure, not the small-event inactivity clock used by streaming tests.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = events.recv().await.expect("event");
            if matches!(
                event.as_ref(),
                EngineEvent::TurnFinished {
                    status: AgentTurnStatus::MaxTurns,
                    ..
                }
            ) {
                break;
            }
        }
    })
    .await
    .expect("bounded result closure");
    let source = sink.test_events_after(None).await.expect("source");
    let completed = source
        .iter()
        .filter_map(|event| match event {
            EngineEvent::ToolCallFinished {
                output, is_error, ..
            } => Some((output, *is_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 2);
    assert!(!completed[0].1);
    assert!(completed[1].1);
    assert!(matches!(completed[0].0, ToolOutput::Text { text } if text.len() == 9 * 1024 * 1024));
    assert!(
        matches!(completed[1].0, ToolOutput::Text { text } if text.contains("aggregate result") && text.len() < 256)
    );
    let recovered = project_session_events(&source).expect("complete canonical claim");
    assert!(recovered.interrupted_turn.is_none());
    let blocks = recovered
        .conversation
        .iter()
        .find(|turn| turn.role == Role::Tool)
        .expect("Tool IR");
    assert!(matches!(
        blocks.blocks.as_slice(),
        [
            Block::ToolResult {
                is_error: false,
                ..
            },
            Block::ToolResult { is_error: true, .. }
        ]
    ));
    handle.close().await.expect("settled actor");
}
