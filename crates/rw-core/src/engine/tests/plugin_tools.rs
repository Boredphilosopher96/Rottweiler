use super::fixtures::{
    models::PendingModel,
    sinks::RecordingSink,
    support::config,
    tools::{StubOutcome, StubTool},
};
use crate::engine::{
    builtin_hook_dispatcher,
    commands::{SessionCommandAction, SessionCommandContext, SessionCommandOutput},
    session::PluginSessionCapability,
};
use async_trait::async_trait;
use rw_ext::{
    CommandDescriptor, CommandExecutionError, CommandHandler, CommandInvocation, CommandRegistry,
};
use rw_tools::{ToolRegistry, ToolResult};
use rw_types::{
    EngineEvent, ToolCapability,
    config::PermissionDecision,
    extension_tools::{ExtensionToolCall, ExtensionToolOutcome},
};
use std::sync::{Arc, OnceLock, atomic::Ordering};

#[derive(Default)]
struct BrokerCommand {
    session: OnceLock<PluginSessionCapability>,
    outcome: OnceLock<crate::engine::recovery::HistoryRead<ExtensionToolOutcome>>,
    origin: OnceLock<rw_types::extension_invocation::ExtensionInvocationId>,
}
#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for BrokerCommand {
    async fn execute(
        &self,
        _: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        let session = self.session.get().expect("bound session");
        let origin = invocation.origin().cloned().expect("command origin");
        self.origin.set(origin.clone()).expect("one command");
        assert!(
            session
                .call_tool(ExtensionToolCall {
                    origin: origin.clone(),
                    name: "undeclared".into(),
                    input: serde_json::json!({})
                })
                .await
                .is_err()
        );
        let outcome = session
            .call_tool(ExtensionToolCall {
                origin,
                name: "read".into(),
                input: serde_json::json!({"path":"data.txt"}),
            })
            .await
            .expect("host tool completes without actor deadlock");
        assert!(self.outcome.set(outcome).is_ok(), "one completion");
        session
            .set_status("tool complete")
            .await
            .expect("command callback remains serviceable");
        Ok(SessionCommandOutput {
            message: "broker complete".into(),
            action: SessionCommandAction::None,
        })
    }
}

#[tokio::test]
async fn command_host_tool_uses_canonical_turn_without_provider_ir_and_retires_origin() {
    let root = tempfile::TempDir::new().expect("root");
    let sink = Arc::new(RecordingSink::default());
    let tool = Arc::new(StubTool::new(
        "read",
        vec![ToolCapability::ReadFilesystem],
        StubOutcome::Success(ToolResult::new("canonical result", serde_json::Value::Null)),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).expect("tool");
    let mut cfg = config(
        root.path(),
        Arc::new(PendingModel),
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    cfg.event_sink = sink.clone();
    let command = Arc::new(BrokerCommand::default());
    let mut commands = CommandRegistry::new();
    commands
        .register_shared(
            CommandDescriptor::new("broker", "host tool").with_host_tools(["read".into()]),
            command.clone(),
        )
        .expect("command");
    cfg.commands = Arc::new(commands);
    let handle = crate::engine::tests::fixtures::history::spawn(cfg)
        .await
        .expect("actor");
    command
        .session
        .set(
            handle
                .plugin_session_capability("broker")
                .expect("capability"),
        )
        .expect("bind");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        handle.send_message("/broker"),
    )
    .await
    .expect("duplex completion")
    .expect("command succeeds");
    let outcome = command.outcome.get().expect("outcome");
    assert!(!outcome.is_error);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    {
        let events = sink.events.lock().expect("events");
        let started = events.iter().position(|event|matches!(&event.wire,EngineEvent::ToolCallStarted {invocation_id,..} if invocation_id==&outcome.invocation_id)).expect("canonical start");
        let finished = events.iter().position(|event|matches!(&event.wire,EngineEvent::ToolCallFinished {invocation_id,..} if invocation_id==&outcome.invocation_id)).expect("canonical finish");
        let terminal = events.iter().position(|event|matches!(&event.wire,EngineEvent::TurnFinished {turn_id,..} if turn_id==&outcome.turn_id)).expect("terminal");
        assert!(started < finished && finished < terminal);
        assert!(
            !events.iter().any(|event| matches!(
                &event.wire,
                EngineEvent::ConversationTurnCommitted { .. }
                    | EngineEvent::ConversationInputCommitted { .. }
            )),
            "host calls do not invent model messages"
        );
    }
    assert!(
        command
            .session
            .get()
            .expect("session")
            .call_tool(ExtensionToolCall {
                origin: command.origin.get().expect("origin").clone(),
                name: "read".into(),
                input: serde_json::json!({})
            })
            .await
            .is_err(),
        "settled command cannot retain tool authority"
    );
    handle.close().await.expect("settled close");
}
