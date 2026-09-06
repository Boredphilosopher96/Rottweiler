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
    // Every result fits the registry's 256 KiB per-tool limit; their combined
    // logical IR exceeds 16 MiB, exercising the batch owner itself.
    const CALLS: usize = 96;
    const BODY_BYTES: usize = 192 * 1024;
    let root = tempfile::tempdir().expect("root");
    let ids = (0..CALLS)
        .map(|index| format!("call-{index}"))
        .collect::<Vec<_>>();
    let calls = ids
        .iter()
        .map(|id| (id.as_str(), "large", serde_json::json!({"item":id})))
        .collect::<Vec<_>>();
    let model = Arc::new(ScriptedModel::new([tool_script(&calls, &[])]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(StubTool::new(
            "large",
            vec![],
            StubOutcome::Success(ToolResult::new(
                "x".repeat(BODY_BYTES),
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
    handle.send_message("run the batch").await.expect("message");
    // This acceptance includes three CPU-admitted encodings of multi-megabyte bodies.
    // It checks closure, not the small-event inactivity clock used by streaming tests.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = events.recv().await.expect("event");
            if let EngineEvent::TurnFinished { status, .. } = event.as_ref() {
                assert_eq!(
                    *status,
                    rw_types::TurnStatus::MaxTurns,
                    "batch terminal status"
                );
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
    assert_eq!(completed.len(), CALLS);
    assert!(completed.iter().any(|(_, error)| !error));
    assert!(completed.iter().any(|(_, error)| *error));
    for (output, is_error) in &completed {
        assert!(matches!(output, ToolOutput::Text { text } if if *is_error {
            text.contains("aggregate result") && text.len() < 256
        } else { text.len() == BODY_BYTES }));
    }
    let recovered = project_session_events(&source).expect("complete canonical claim");
    assert!(recovered.interrupted_turn.is_none());
    let tool_turn = recovered
        .conversation
        .iter()
        .find(|turn| turn.role == Role::Tool)
        .expect("Tool IR");
    assert_eq!(tool_turn.blocks.len(), CALLS);
    for (block, (expected, error)) in tool_turn.blocks.iter().zip(completed) {
        assert!(
            matches!(block, Block::ToolResult { output, is_error, .. } if output == expected && *is_error == error)
        );
    }
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
    let terminal = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.expect("event delivery");
            if let EngineEvent::TurnFinished { status, .. } = event.as_ref() {
                break status.clone();
            }
        }
    })
    .await;
    let diagnostic = sink
        .test_events_after(None)
        .await
        .expect("diagnostic source");
    let trace = closure_trace(&diagnostic);
    assert_eq!(
        terminal.ok(),
        Some(rw_types::TurnStatus::Interrupted),
        "canonical closure trace: {trace:?}"
    );
    handle
        .close()
        .await
        .expect("journal repair settled the actor");
    let source = sink.test_events_after(None).await.expect("source");
    assert_repaired_order(&source);
    let recovered = project_session_events(&source).expect("repaired prefix");
    assert!(recovered.interrupted_turn.is_none());
    assert!(
        recovered.interrupted_tool_repairs.is_empty(),
        "completed effects are never replayed"
    );
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
    assert_repaired_order(&source);
    assert!(
        project_session_events(&source)
            .expect("repaired claim")
            .interrupted_turn
            .is_none()
    );
    reopened.close().await.expect("settled reopen");
}

fn assert_repaired_order(source: &[EngineEvent]) {
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
    assert_eq!(
        source
            .iter()
            .filter(|event| matches!(event, EngineEvent::TurnFinished { .. }))
            .count(),
        1
    );
    let completion = source
        .iter()
        .position(|event| matches!(event, EngineEvent::ToolCallFinished { .. }))
        .expect("durable completion");
    let selector = source
        .iter()
        .position(|event| matches!(event, EngineEvent::ConversationToolResultsCommitted { .. }))
        .expect("repair selector");
    let terminal = source
        .iter()
        .position(|event| {
            matches!(
                event,
                EngineEvent::TurnFinished {
                    status: rw_types::TurnStatus::Interrupted,
                    ..
                }
            )
        })
        .expect("repaired terminal");
    assert!(completion < selector && selector < terminal);
}

fn closure_trace(events: &[EngineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::TurnStarted { .. } => Some("started".into()),
            EngineEvent::ToolCallFinished { is_error, .. } => Some(format!("tool:{is_error}")),
            EngineEvent::ConversationToolResultsCommitted { .. } => Some("selector".into()),
            EngineEvent::TurnFinished { status, .. } => Some(format!("terminal:{status:?}")),
            EngineEvent::Error { error, .. } => Some(format!(
                "error:{}",
                error.message.chars().take(512).collect::<String>()
            )),
            _ => None,
        })
        .collect()
}
