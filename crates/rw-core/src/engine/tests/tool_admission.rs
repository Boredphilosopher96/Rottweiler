#![cfg(test)]
use super::fixtures::{
    models::ScriptedModel,
    support::{collect_turn, config},
    tools::{StubOutcome, StubTool},
};
use crate::engine::{AgentTurnStatus, PendingEvent, builtin_hook_dispatcher};
use rw_providers::{FinishReason, ProviderEvent};
use rw_tools::ToolRegistry;
use rw_types::{
    Block,
    config::PermissionDecision,
    tool_admission::{MAX_PENDING_TOOL_ARGUMENT_BYTES, MAX_PENDING_TOOL_INVOCATIONS},
};
use std::sync::{Arc, atomic::Ordering};

async fn rejected_batch(count: usize, argument_bytes: usize) {
    let root = tempfile::tempdir().expect("root");
    let mut script = Vec::new();
    for index in 0..count {
        let id = format!("call-{index}");
        script.push(Ok(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: "probe".into(),
        }));
        script.push(Ok(ProviderEvent::ToolCallEnd {
            id,
            arguments: serde_json::json!({"text":"x".repeat(argument_bytes)}),
        }));
    }
    script.push(Ok(ProviderEvent::Finished {
        reason: FinishReason::ToolCalls,
    }));
    let model = Arc::new(ScriptedModel::new([script]));
    let tool = Arc::new(StubTool::new(
        "probe",
        vec![],
        StubOutcome::Failure("must not run".into()),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).expect("tool");
    let actor = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let mut subscription = actor.subscribe().expect("subscription");
    actor.send_message("run tool batch").await.expect("message");
    let events = collect_turn(&mut subscription).await;
    assert!(events.iter().any(|event| matches!(&event.kind,
        PendingEvent::Error { message } if message.contains("admission"))));
    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Failed,
            ..
        }
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, PendingEvent::ToolCallStarted { .. }))
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert!(!events.iter().any(|event| matches!(&event.kind,
        PendingEvent::ConversationTurnCommitted {turn, ..}
            if turn.blocks.iter().any(|block| matches!(block, Block::ToolCall { .. })))));
    actor.close().await.expect("settlement");
}

#[tokio::test]
async fn provider_tool_batch_count_is_rejected_before_any_canonical_announcement_or_execution() {
    rejected_batch(MAX_PENDING_TOOL_INVOCATIONS + 1, 0).await;
}
#[tokio::test]
async fn provider_tool_batch_argument_bytes_are_aggregate_before_announcements() {
    rejected_batch(2, MAX_PENDING_TOOL_ARGUMENT_BYTES / 2).await;
}

struct ExpandingApprovalRedactor;
impl crate::engine::SecretRedactor for ExpandingApprovalRedactor {
    fn redact(&self, value: &str) -> String {
        if value.starts_with("--- ") {
            "x".repeat(rw_types::tool_admission::MAX_PENDING_TOOL_APPROVAL_BYTES + 1)
        } else {
            value.to_owned()
        }
    }
}

#[tokio::test]
async fn oversized_redacted_preview_fails_tool_before_diff_or_approval_publication() {
    use super::fixtures::support::{stop_script, tool_script};
    let root = tempfile::tempdir().expect("root");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[(
                "call",
                "write",
                serde_json::json!({"path":"bound.txt","content":"after"}),
            )],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(rw_tools::WriteTool::new(
            rw_tools::ToolLimits::default(),
        )))
        .expect("write");
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Ask,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.secret_redactor = Arc::new(ExpandingApprovalRedactor);
    let actor = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    let mut events = actor.subscribe().expect("subscription");
    actor.send_message("write file").await.expect("message");
    let events = collect_turn(&mut events).await;
    assert!(events.iter().any(|event| matches!(&event.kind, PendingEvent::ToolCallFinished {is_error: true, output: rw_types::ToolOutput::Text {text}, ..} if text.contains("admission"))));
    assert!(!events.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::ToolDiffReady { .. } | PendingEvent::PermissionRequested { .. }
    )));
    assert!(!root.path().join("bound.txt").exists());
    actor.close().await.expect("settled");
}
