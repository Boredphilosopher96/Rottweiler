#![cfg(test)]
use super::Arc;
use super::CheckpointStore;
use super::DurableCheckpointCoordinator;
use super::DurableEventSink;
use super::EngineEvent;
use super::FinishReason;
use super::JournalService;
use super::ModelDriver;
use super::PermissionDecision;
use super::PermissionGate;
use super::Provider;
use super::ProviderEvent;
use super::ProviderModel;
use super::ScriptProvider;
use super::SessionActor;
use super::SessionActorConfig;
use super::SessionEventLog;
use super::SessionId;
use super::SystemEventClock;
use super::ThinkingLevel;
use super::ToolLimits;
use super::ToolRegistry;
use super::TurnStatus;
use super::WriteTool;
use super::builtin_command_registry;
use super::builtin_hook_dispatcher;
use super::checkpoint_root;
use super::tempdir;
use super::test_provider_admission;
use rw_core::recovery::SessionHistory;

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
    let mode_registry = Arc::new(rw_ext::ModeRegistry::builtins().expect("built-in modes"));
    sink.configure_canonical(Arc::clone(&mode_registry), None)
        .expect("canonical owner");
    let coordinator_root = checkpoint_root(&storage, &workspace, &session.0);
    let checkpoints = Arc::new(DurableCheckpointCoordinator::new(
        coordinator_root.clone(),
        Arc::new(
            CheckpointStore::open(
                &coordinator_root,
                &workspace,
                rw_store::checkpoint::CheckpointBlobStore::open(&storage, &workspace)
                    .expect("workspace blobs"),
            )
            .expect("checkpoint store"),
        ),
    ));
    let recovered = rw_core::SessionActorRecovery::from_bootstrap(
        sink.capture_history()
            .await
            .expect("history")
            .bootstrap()
            .await
            .expect("bootstrap"),
    )
    .expect("actor recovery");
    let actor = SessionActor::spawn(SessionActorConfig {
        ui: std::sync::Arc::new(rw_core::ui::EmptyUiRegistry),
        ui_tool_source: std::sync::Arc::new(rw_core::ui::UnavailableUiToolSource),
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
        modes: mode_registry,
        history: sink.clone(),
        event_sink: sink,
        event_clock: Arc::new(SystemEventClock),
        provider_admission: test_provider_admission(),
        secret_redactor: Arc::new(rw_core::NoopSecretRedactor),
        checkpoints,
        folder_trust: Arc::new(rw_core::NoopFolderTrustController),
        workspace_roots: Arc::new(rw_core::NoopWorkspaceRootController),
        extension_development: Arc::new(rw_core::NoopSessionExtensionController),
        resources: Arc::new(rw_core::NoopSessionResources),
        recovered,
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
