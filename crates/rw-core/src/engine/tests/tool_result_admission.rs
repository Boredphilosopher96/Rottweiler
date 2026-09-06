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
                    status: rw_types::TurnStatus::MaxTurns,
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

#[tokio::test]
async fn failed_result_selector_stays_repairable_without_reexecuting_the_tool() {
    use super::fixtures::{sinks::FailNextBatchSink, support::next_matching};
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().expect("root");
    let model = Arc::new(ScriptedModel::new([tool_script(
        &[("call", "once", serde_json::json!({}))],
        &[],
    )]));
    let tool = Arc::new(StubTool::new(
        "once",
        vec![],
        StubOutcome::Success(ToolResult::new(
            "authoritative result",
            serde_json::Value::Null,
        )),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).expect("tool");
    let tools = Arc::new(tools);
    let sink = Arc::new(FailNextBatchSink::default());
    sink.fail_tool_result_commit.store(true, Ordering::Release);
    let mut settings = config(
        root.path(),
        model,
        tools.clone(),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    settings.event_sink = sink.clone();
    let handle = super::fixtures::history::spawn(settings)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run once").await.expect("message");
    next_matching(&mut events, |event| matches!(event, crate::engine::PendingEvent::Error { message } if message.contains("settlement is unproven"))).await;
    assert!(
        handle.close().await.is_err(),
        "unpublished result closure remains explicit"
    );
    let source = sink.test_events_after(None).await.expect("source");
    assert!(
        !source
            .iter()
            .any(|event| matches!(event, EngineEvent::TurnFinished { .. }))
    );
    let recovered = project_session_events(&source).expect("repairable prefix");
    assert_eq!(recovered.interrupted_turn, Some(1));
    assert!(
        recovered.interrupted_tool_repairs.is_empty(),
        "completed effects are never replayed"
    );
    assert!(recovered.interrupted_tool_turn.is_some());
    let reopen_model = Arc::new(ScriptedModel::default());
    let mut settings = config(
        root.path(),
        reopen_model.clone(),
        tools,
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    settings.recovered = recovered;
    settings.event_sink = sink.clone();
    let reopened = super::fixtures::history::spawn(settings)
        .await
        .expect("reopen");
    let mut replay = reopened.subscribe().expect("subscription");
    next_matching(&mut replay, |event| {
        matches!(
            event,
            crate::engine::PendingEvent::TurnFinished {
                status: AgentTurnStatus::Interrupted,
                ..
            }
        )
    })
    .await;
    assert_eq!(tool.calls.load(Ordering::Acquire), 1);
    assert_eq!(reopen_model.request_count(), 0);
    let source = sink.test_events_after(None).await.expect("repaired source");
    assert_eq!(
        source
            .iter()
            .filter(|event| matches!(event, EngineEvent::ToolCallFinished { .. }))
            .count(),
        1
    );
    assert_eq!(
        source
            .iter()
            .filter(|event| matches!(event, EngineEvent::ConversationToolResultsCommitted { .. }))
            .count(),
        1
    );
    assert!(
        project_session_events(&source)
            .expect("repaired claim")
            .interrupted_turn
            .is_none()
    );
    reopened.close().await.expect("settled reopen");
}
