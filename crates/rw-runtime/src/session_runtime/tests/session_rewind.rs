#![cfg(test)]
use super::Arc;
use super::Block;
use super::CheckpointStore;
use super::Cost;
use super::DurableCheckpointCoordinator;
use super::DurableEventSink;
use super::EngineEvent;
use super::EventMeta;
use super::FinishReason;
use super::JournalService;
use super::ModelDriver;
use super::PermissionDecision;
use super::PermissionGate;
use super::Provider;
use super::ProviderEvent;
use super::ProviderModel;
use super::Role;
use super::SESSION_EVENT_VERSION;
use super::ScriptProvider;
use super::SequenceId;
use super::SessionActor;
use super::SessionActorConfig;
use super::SessionEventLog;
use super::SessionId;
use super::SystemEventClock;
use super::ThinkingLevel;
use super::TodoRestoreBinding;
use super::TodoTool;
use super::ToolCallId;
use super::ToolContext;
use super::ToolLimits;
use super::ToolOutput;
use super::ToolRegistry;
use super::Turn;
use super::TurnId;
use super::TurnMeta;
use super::TurnStatus;
use super::Usage;
use super::WriteTool;
use super::builtin_command_registry;
use super::builtin_hook_dispatcher;
use super::checkpoint_root;
use super::restore_todo_state;
use super::tempdir;
use super::test_provider_admission;
use rw_tools::Tool;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn rewind_event_reprojects_ephemeral_todo_state() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let session = SessionId("session-todo-rewind".to_owned());
    let todo = Arc::new(TodoTool::new(ToolLimits::default()));
    let call = Turn {
        role: Role::Assistant,
        blocks: vec![Block::ToolCall {
            id: ToolCallId("todo-1".to_owned()),
            name: "todo".to_owned(),
            args: serde_json::json!({
                "action": "replace",
                "items": [{"id": "one", "content": "kept until rewind"}]
            }),
        }],
        meta: TurnMeta::default(),
    };
    let result = Turn {
        role: Role::User,
        blocks: vec![Block::ToolResult {
            id: ToolCallId("todo-1".to_owned()),
            output: ToolOutput::Text {
                text: "ok".to_owned(),
            },
            is_error: false,
        }],
        meta: TurnMeta::default(),
    };
    restore_todo_state(&[call.clone(), result.clone()], &workspace, &session, &todo)
        .await
        .expect("restore todo");
    let context = ToolContext::new(&workspace)
        .expect("tool context")
        .with_session_id(session.clone());
    let before = todo
        .execute(&context, serde_json::json!({"action": "list"}))
        .await
        .expect("list before rewind");
    assert_eq!(before.data["count"], 1);

    let log = SessionEventLog::open(root.path(), &session.0).expect("event log");
    let sink = DurableEventSink::new(
        log,
        root.path().to_owned(),
        session.0.clone(),
        JournalService::new(root.path()).expect("journal reads"),
    )
    .expect("durable sink");
    sink.bind_todo(TodoRestoreBinding {
        todo: Arc::clone(&todo),
        workspace: workspace.clone(),
        session_id: session.clone(),
    });
    let fixture_meta = |sequence: u64| EventMeta {
        protocol_version: SESSION_EVENT_VERSION,
        session_id: session.clone(),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
        caused_by: None,
    };
    for event in [
        EngineEvent::TurnStarted {
            meta: fixture_meta(0),
            turn_id: TurnId("1".to_owned()),
        },
        EngineEvent::ConversationTurnCommitted {
            meta: fixture_meta(1),
            agent_turn: 1,
            turn: call,
        },
        EngineEvent::ConversationTurnCommitted {
            meta: fixture_meta(2),
            agent_turn: 1,
            turn: result,
        },
        EngineEvent::TurnFinished {
            meta: fixture_meta(3),
            turn_id: TurnId("1".to_owned()),
            status: TurnStatus::Completed,
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            cost: Cost::Monetary {
                amount_micros: 0,
                currency: "USD".to_owned(),
            },
        },
        EngineEvent::ConversationRewound {
            meta: fixture_meta(4),
            to_agent_turn: 0,
            operation_id: "rewind-todo-0".to_owned(),
            unrestorable_paths: Vec::new(),
        },
    ] {
        rw_core::commit_session_events(Arc::clone(&sink), vec![event])
            .await
            .expect("fixture event append");
    }
    let after = todo
        .execute(&context, serde_json::json!({"action": "list"}))
        .await
        .expect("list after rewind");
    assert_eq!(after.data["count"], 0);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn session_handle_rewind_restores_ten_agent_edits_to_turn_three() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let storage = root.path().join("storage");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let session = SessionId("session-direct-rewind".to_owned());
    let mut scripts = Vec::new();
    for turn in 1..=10_u64 {
        let id = format!("write-{turn}");
        scripts.push(vec![
            ProviderEvent::ToolCallStart {
                id: id.clone(),
                name: "write".to_owned(),
            },
            ProviderEvent::ToolCallEnd {
                id,
                arguments: serde_json::json!({
                    "path": "state.txt",
                    "content": format!("turn-{turn}\n"),
                }),
            },
            ProviderEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ]);
        scripts.push(vec![ProviderEvent::Finished {
            reason: FinishReason::Stop,
        }]);
    }
    let scripted: Arc<dyn Provider> =
        Arc::new(ScriptProvider::new("direct-rewind".to_owned(), scripts, 0));
    let model: Arc<dyn ModelDriver> = Arc::new(
        ProviderModel::new(
            scripted,
            rw_core::CompactionConfig::default(),
            rw_core::BudgetConfig::default(),
        )
        .expect("fixture concrete model"),
    );
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(WriteTool::new(ToolLimits::default())))
        .expect("write tool");
    let log = SessionEventLog::open(&storage, &session.0).expect("event log");
    let sink = DurableEventSink::new(
        log,
        storage.clone(),
        session.0.clone(),
        JournalService::new(&(storage.clone())).expect("journal reads"),
    )
    .expect("durable sink");
    let coordinator_root = checkpoint_root(&storage, &workspace, &session.0);
    let checkpoints = Arc::new(DurableCheckpointCoordinator::new(
        coordinator_root.clone(),
        Arc::new(CheckpointStore::open(&coordinator_root, &workspace).expect("checkpoint store")),
    ));
    let actor = SessionActor::spawn(SessionActorConfig {
        budget_session_id: session.clone(),
        session_id: session,
        workspace_root: workspace.clone(),
        additional_workspace_roots: Vec::new(),
        workspace_generation: 0,
        initial_session_context: Vec::new(),
        startup_notifications: Vec::new(),
        model_alias: "fast".to_owned(),
        model,
        tools: Arc::new(registry),
        permissions: Arc::new(PermissionGate::new(PermissionDecision::Allow)),
        hooks: Arc::new(builtin_hook_dispatcher().expect("hooks")),
        commands: Arc::new(builtin_command_registry().expect("commands")),
        modes: Arc::new(rw_ext::ModeRegistry::builtins().expect("built-in modes")),
        event_sink: sink,
        event_clock: Arc::new(SystemEventClock),
        provider_admission: test_provider_admission(),
        secret_redactor: Arc::new(rw_core::NoopSecretRedactor),
        checkpoints,
        folder_trust: Arc::new(rw_core::NoopFolderTrustController),
        workspace_roots: Arc::new(rw_core::NoopWorkspaceRootController),
        extension_development: Arc::new(rw_core::NoopSessionExtensionController),
        resources: Arc::new(rw_core::NoopSessionResources),
        recovered: rw_core::SessionRecoveredState::default(),
        max_turns: 4,
        identical_tool_failure_limit: 5,
        max_output_tokens: 1024,
        thinking: ThinkingLevel::Off,
        event_capacity: 256,
    })
    .expect("session actor");
    let mut events = actor.subscribe().expect("subscription");
    for turn in 1..=10_u64 {
        actor
            .send_message(format!("edit number {turn}"))
            .await
            .expect("start turn");
        loop {
            let event = events.recv().await.expect("turn event");
            if matches!(
                event,
                EngineEvent::TurnFinished {
                    turn_id,
                    status: TurnStatus::Completed,
                    ..
                } if turn_id.0 == turn.to_string()
            ) {
                break;
            }
        }
    }
    actor.rewind(3).await.expect("direct rewind");
    loop {
        let event = events.recv().await.expect("rewind event");
        if matches!(
            event,
            EngineEvent::ConversationRewound {
                to_agent_turn: 3,
                ..
            }
        ) {
            break;
        }
    }
    assert_eq!(
        std::fs::read(workspace.join("state.txt")).expect("rewound file"),
        b"turn-3\n"
    );
}
