#![cfg(test)]

use crate::InitDepth;
use crate::PermissionGate;
use crate::PermissionOutcome;
use crate::PermissionRequest;
use crate::engine::MessageDisposition;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::commands::FolderTrustOperation;
use crate::engine::commands::builtin_command_registry;
use crate::engine::mutation_checkpoints::MutationCheckpointOutcome;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::checkpoints::InitRecordingCheckpoints;
use crate::engine::tests::fixtures::controllers::EchoCommand;
use crate::engine::tests::fixtures::controllers::FixedSessionExtensionController;
use crate::engine::tests::fixtures::controllers::FixedWorkspaceRootController;
use crate::engine::tests::fixtures::controllers::InitActionCommand;
use crate::engine::tests::fixtures::controllers::RecordingFolderTrust;
use crate::engine::tests::fixtures::controllers::ScopedPromptCommand;
use crate::engine::tests::fixtures::controllers::StaticApprover;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::WorkspaceChangeFailingSink;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::has_command;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::protocol_meta;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::tools::StubOutcome;
use crate::engine::tests::fixtures::tools::StubTool;
use rw_ext::CommandDescriptor;
use rw_tools::MutationScope;
use rw_tools::ToolRegistry;
use rw_tools::ToolResult;
use rw_types::ApprovalDecision;
use rw_types::ClientCommand;
use rw_types::ClientRole;
use rw_types::CommandOutcome;
use rw_types::EngineEvent;
use rw_types::ToolCapability;
use rw_types::config::PermissionDecision;
use serde_json::Value;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[tokio::test]
async fn commands_share_the_public_registry_and_events_round_trip() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::default());
    let mut commands = builtin_command_registry().expect("built-ins");
    commands
        .register(
            CommandDescriptor::new("echo", "fixture extension command")
                .with_argument_hint("<text>"),
            EchoCommand,
        )
        .expect("extension command");
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.commands = Arc::new(commands);
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    assert_eq!(
        handle.send_message("/echo hello").await.expect("command"),
        MessageDisposition::Command
    );
    let event = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::CommandFinished { .. })
    })
    .await;
    assert!(matches!(
        &event.kind,
        PendingEvent::CommandFinished { name, message, .. }
            if name == "echo" && message == "hello"
    ));
    let encoded = serde_json::to_vec(&event.wire).expect("serialize event");
    assert!(String::from_utf8_lossy(&encoded).contains("\"sequence_id\":\""));
    let decoded: EngineEvent = serde_json::from_slice(&encoded).expect("deserialize event");
    assert_eq!(decoded, event.wire);

    assert_eq!(
        handle.send_message("/help").await.expect("help command"),
        MessageDisposition::Command
    );
    let help = next_matching(
        &mut events,
        |kind| matches!(kind, PendingEvent::CommandFinished { name, .. } if name == "help"),
    )
    .await;
    assert!(matches!(
        &help.kind,
        PendingEvent::CommandFinished { message, .. }
            if message.contains("/echo <text> — fixture extension command")
    ));
}

#[tokio::test]
async fn initialization_acks_before_scan_and_checkpoints_every_generated_path() {
    let root = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(root.path().join("packages/one")).expect("package directory");
    std::fs::write(
        root.path().join("package.json"),
        r#"{"name":"fixture","scripts":{"test":"true"}}"#,
    )
    .expect("root package marker");
    std::fs::write(
        root.path().join("packages/one/package.json"),
        r#"{"name":"one"}"#,
    )
    .expect("package marker");
    let model = Arc::new(ScriptedModel::default());
    let mut commands = builtin_command_registry().expect("built-ins");
    commands
        .register(
            CommandDescriptor::new("deep-init", "fixture initialization"),
            InitActionCommand(InitDepth::Deep),
        )
        .expect("init command");
    let checkpoints = Arc::new(InitRecordingCheckpoints::new(Duration::from_millis(100)));
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.commands = Arc::new(commands);
    actor_config.checkpoints = checkpoints.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    assert_eq!(
        timeout(Duration::from_millis(16), handle.send_message("/deep-init"))
            .await
            .expect("initialization acknowledgement deadline")
            .expect("initialization acknowledgement"),
        MessageDisposition::Command
    );
    let completed = next_matching(
        &mut events,
        |kind| matches!(kind, PendingEvent::CommandFinished { name, .. } if name == "deep-init"),
    )
    .await;
    assert!(matches!(
        completed.kind,
        PendingEvent::CommandFinished { ref message, .. }
            if message.contains("generated 2 instruction file(s)")
    ));
    assert!(root.path().join("AGENTS.md").is_file());
    assert!(root.path().join("packages/one/AGENTS.md").is_file());
    assert_eq!(
        checkpoints.scopes.lock().expect("scopes").as_slice(),
        &[MutationScope::Paths(vec![
            PathBuf::from("AGENTS.md"),
            PathBuf::from("packages/one/AGENTS.md"),
        ])]
    );
    assert_eq!(
        checkpoints.outcomes.lock().expect("outcomes").as_slice(),
        &[MutationCheckpointOutcome::Completed]
    );
    assert_eq!(checkpoints.turns.lock().expect("turns").as_slice(), &[1]);
}

#[tokio::test]
async fn failed_initialization_reports_failed_checkpoint_without_partial_writes() {
    let root = TempDir::new().expect("tempdir");
    std::fs::write(root.path().join("Cargo.toml"), "[workspace]\nmembers=[]\n")
        .expect("cargo marker");
    std::fs::write(root.path().join("AGENTS.md"), "human owned").expect("existing instructions");
    let mut commands = builtin_command_registry().expect("built-ins");
    commands
        .register(
            CommandDescriptor::new("init", "fixture initialization"),
            InitActionCommand(InitDepth::Root),
        )
        .expect("init command");
    let checkpoints = Arc::new(InitRecordingCheckpoints::new(Duration::ZERO));
    let mut actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.commands = Arc::new(commands);
    actor_config.checkpoints = checkpoints.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    assert_eq!(
        handle.send_message("/init").await.expect("init ack"),
        MessageDisposition::Command
    );
    let completed = next_matching(
        &mut events,
        |kind| matches!(kind, PendingEvent::CommandFinished { name, .. } if name == "init"),
    )
    .await;
    assert!(matches!(
        completed.kind,
        PendingEvent::CommandFinished { ref message, .. }
            if message.contains("initialization failed")
    ));
    assert_eq!(
        std::fs::read_to_string(root.path().join("AGENTS.md")).expect("human instructions remain"),
        "human owned"
    );
    assert_eq!(
        checkpoints.outcomes.lock().expect("outcomes").as_slice(),
        &[MutationCheckpointOutcome::Failed]
    );
}

#[tokio::test]
async fn custom_prompt_model_and_tool_overrides_are_turn_scoped() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        stop_script("scoped", &[]),
        stop_script("normal", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    for name in ["read", "write"] {
        tools
            .register(Arc::new(StubTool::new(
                name,
                vec![ToolCapability::ReadFilesystem],
                StubOutcome::Success(ToolResult::new("ok", Value::Null)),
            )))
            .expect("tool");
    }
    let mut commands = builtin_command_registry().expect("commands");
    commands
        .register(
            CommandDescriptor::new("scoped", "scoped custom prompt"),
            ScopedPromptCommand,
        )
        .expect("custom command");
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.commands = Arc::new(commands);
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");

    assert_eq!(
        handle.send_message("/scoped").await.expect("custom turn"),
        MessageDisposition::Started
    );
    collect_turn(&mut events).await;
    assert_eq!(
        handle
            .send_message("normal prompt")
            .await
            .expect("normal turn"),
        MessageDisposition::Started
    );
    collect_turn(&mut events).await;

    assert_eq!(model.aliases(), ["slow", "fast"]);
    let requests = model.requests.lock().expect("requests").clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["read"]
    );
    assert_eq!(
        requests[1]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["read", "write"]
    );
    assert_eq!(
        handle.snapshot().await.expect("snapshot").model_alias,
        "fast"
    );
}

#[tokio::test]
async fn trust_slash_command_dispatches_status_grant_and_revoke_to_host_boundary() {
    let root = TempDir::new().expect("tempdir");
    let trust = Arc::new(RecordingFolderTrust::default());
    let mut actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.folder_trust = trust.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    for (command, expected) in [
        ("/trust", FolderTrustOperation::Status),
        (
            "/trust grant",
            FolderTrustOperation::Grant { confirmation: None },
        ),
        ("/trust revoke", FolderTrustOperation::Revoke),
    ] {
        assert_eq!(
            handle.send_message(command).await.expect("trust command"),
            MessageDisposition::Command
        );
        let event = next_matching(
            &mut events,
            |kind| matches!(kind, PendingEvent::CommandFinished { name, .. } if name == "trust"),
        )
        .await;
        assert!(matches!(
            event.kind,
            PendingEvent::CommandFinished { message, .. }
                if message == format!("trust operation: {expected:?}")
        ));
    }
    assert_eq!(
        *trust.operations.lock().expect("trust operations"),
        vec![
            FolderTrustOperation::Status,
            FolderTrustOperation::Grant { confirmation: None },
            FolderTrustOperation::Revoke,
        ]
    );
}

#[tokio::test]
async fn add_dir_commit_failure_aborts_generation_and_preserves_live_runtime() {
    let root = TempDir::new().expect("tempdir");
    let primary = std::fs::canonicalize(root.path()).expect("canonical primary");
    let added_dir = TempDir::new().expect("added tempdir");
    let added = std::fs::canonicalize(added_dir.path()).expect("canonical added");
    let tools = Arc::new(ToolRegistry::new());
    let permissions = Arc::new(PermissionGate::new(PermissionDecision::Allow));
    let controller = Arc::new(FixedWorkspaceRootController {
        roots: vec![primary.clone(), added.clone()],
        tools: Arc::clone(&tools),
        permissions: Arc::clone(&permissions),
        committed: AtomicU64::new(0),
        aborted: AtomicU64::new(0),
        fail_commit: true,
    });
    let mut actor_config = config(
        &primary,
        Arc::new(ScriptedModel::default()),
        tools,
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.permissions = permissions;
    actor_config.workspace_roots = controller.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    let failure = handle
        .send_message(format!("/add-dir {}", added.display()))
        .await
        .expect_err("generation commit failure");
    assert!(failure.to_string().contains("could not commit"));
    while let Ok(event) = events.receiver.try_recv() {
        assert!(!matches!(
            event.event,
            EngineEvent::WorkspaceRootsChanged { .. }
        ));
    }
    assert_eq!(controller.committed.load(Ordering::SeqCst), 0);
    assert_eq!(controller.aborted.load(Ordering::SeqCst), 1);
    let snapshot = handle.snapshot().await.expect("snapshot");
    assert_eq!(snapshot.workspace_generation, 0);
    assert_eq!(
        snapshot
            .workspace_roots
            .iter()
            .map(|root| root.path.as_str())
            .collect::<Vec<_>>(),
        vec!["@root/0"]
    );

    let failing_permissions = Arc::new(PermissionGate::new(PermissionDecision::Allow));
    let failing_controller = Arc::new(FixedWorkspaceRootController {
        roots: vec![primary.clone(), added.clone()],
        tools: Arc::new(ToolRegistry::new()),
        permissions: Arc::clone(&failing_permissions),
        committed: AtomicU64::new(0),
        aborted: AtomicU64::new(0),
        fail_commit: false,
    });
    let mut failing_config = config(
        &primary,
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    failing_config.permissions = failing_permissions;
    failing_config.workspace_roots = failing_controller.clone();
    failing_config.event_sink = Arc::new(WorkspaceChangeFailingSink::default());
    let failing = SessionActor::spawn(failing_config).expect("failing actor");
    let failure = failing
        .send_message(format!("/add-dir {}", added.display()))
        .await
        .expect_err("durable event failure");
    let failure_bytes = format!("{failure:?}{failure}");
    assert!(!failure_bytes.contains(&added.to_string_lossy().to_string()));
    let unchanged = failing.snapshot().await.expect("unchanged snapshot");
    assert_eq!(unchanged.workspace_generation, 0);
    assert_eq!(unchanged.workspace_roots.len(), 1);
    assert_eq!(failing_controller.committed.load(Ordering::SeqCst), 0);
    assert_eq!(failing_controller.aborted.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn add_dir_commit_refreshes_the_nonblocking_command_catalog() {
    let root = TempDir::new().expect("tempdir");
    let primary = std::fs::canonicalize(root.path()).expect("canonical primary");
    let added_dir = TempDir::new().expect("added tempdir");
    let added = std::fs::canonicalize(added_dir.path()).expect("canonical added");
    let tools = Arc::new(ToolRegistry::new());
    let permissions = Arc::new(PermissionGate::new(PermissionDecision::Allow));
    let controller = Arc::new(FixedWorkspaceRootController {
        roots: vec![primary.clone(), added.clone()],
        tools: Arc::clone(&tools),
        permissions: Arc::clone(&permissions),
        committed: AtomicU64::new(0),
        aborted: AtomicU64::new(0),
        fail_commit: false,
    });
    let mut actor_config = config(
        &primary,
        Arc::new(ScriptedModel::default()),
        tools,
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.permissions = permissions;
    actor_config.workspace_roots = controller.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    assert!(
        handle
            .command_descriptors()
            .iter()
            .all(|descriptor| descriptor.name() != "generation-marker")
    );

    handle
        .send_message(format!("/add-dir {}", added.display()))
        .await
        .expect("add workspace root");

    assert_eq!(controller.committed.load(Ordering::SeqCst), 1);
    assert!(
        handle
            .command_descriptors()
            .iter()
            .any(|descriptor| descriptor.name() == "generation-marker")
    );
}

#[tokio::test]
async fn live_plugin_reload_swaps_only_successful_generations_and_detach_restores_base() {
    let root = TempDir::new().expect("tempdir");
    let primary = std::fs::canonicalize(root.path()).expect("canonical primary");
    let added_dir = TempDir::new().expect("added tempdir");
    let added = std::fs::canonicalize(added_dir.path()).expect("canonical added");
    let tools = Arc::new(ToolRegistry::new());
    let permissions = Arc::new(PermissionGate::new(PermissionDecision::Allow));
    let workspace_controller = Arc::new(FixedWorkspaceRootController {
        roots: vec![primary.clone(), added.clone()],
        tools: Arc::clone(&tools),
        permissions: Arc::clone(&permissions),
        committed: AtomicU64::new(0),
        aborted: AtomicU64::new(0),
        fail_commit: false,
    });
    let extension_controller = Arc::new(FixedSessionExtensionController::default());
    let mut actor_config = config(
        &primary,
        Arc::new(ScriptedModel::default()),
        tools,
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.permissions = permissions;
    actor_config.workspace_roots = workspace_controller;
    actor_config.extension_development = extension_controller.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let session_id = handle.session_id().clone();
    assert_eq!(
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("driver", "attach-driver"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("driver attach"),
        CommandOutcome::Accepted
    );
    let attach = |request: &str| ClientCommand::AttachDevelopmentPlugin {
        meta: protocol_meta("plugin-dev", request),
        session_id: session_id.clone(),
        source: primary.to_string_lossy().into_owned(),
    };

    assert_eq!(
        handle.dispatch(attach("dev-attach")).await.expect("attach"),
        CommandOutcome::Accepted
    );
    assert!(has_command(&handle, "development-marker"));

    assert_eq!(
        handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "add-root-with-development-plugin"),
                session_id: session_id.clone(),
                content: format!("/add-dir {}", added.display()),
                attachments: Vec::new(),
            })
            .await
            .expect("add workspace root while development plugin is attached"),
        CommandOutcome::Accepted
    );
    assert!(has_command(&handle, "generation-marker"));
    assert!(has_command(&handle, "development-marker"));

    extension_controller.reject.store(true, Ordering::SeqCst);
    assert!(matches!(
        handle
            .dispatch(attach("dev-reload-rejected"))
            .await
            .expect("typed reload rejection"),
        CommandOutcome::Rejected { .. }
    ));
    assert!(
        has_command(&handle, "development-marker"),
        "a rejected candidate must retain the last good generation"
    );

    assert_eq!(
        handle
            .dispatch(ClientCommand::DetachDevelopmentPlugin {
                meta: protocol_meta("plugin-dev", "dev-detach"),
                session_id,
            })
            .await
            .expect("detach"),
        CommandOutcome::Accepted
    );
    assert!(!has_command(&handle, "development-marker"));
    assert!(has_command(&handle, "generation-marker"));
    assert_eq!(extension_controller.attaches.load(Ordering::SeqCst), 2);
    assert_eq!(extension_controller.detaches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn permissions_slash_command_edits_rules_and_revokes_opaque_approvals() {
    let root = TempDir::new().expect("tempdir");
    let permissions = Arc::new(
        PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_project_approval_file(root.path().join("approvals.json")),
    );
    let approval_request = |id: &str, secret: &str| PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: id.to_owned(),
        tool_name: "bash".to_owned(),
        arguments: json!({"command": format!("printf {secret}")}),
        capabilities: vec![rw_types::ToolCapability::Execute],
        approval_diff: None,
    };
    assert_eq!(
        permissions
            .authorize(
                approval_request("session", "SESSION_SECRET_CANARY"),
                &StaticApprover(ApprovalDecision::AllowSession),
            )
            .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        permissions
            .authorize(
                approval_request("project", "PROJECT_SECRET_CANARY"),
                &StaticApprover(ApprovalDecision::AllowProject),
            )
            .await,
        PermissionOutcome::Allowed
    );
    let approvals = permissions.approval_snapshot();
    let session_id = approvals.session[0].id.clone();
    let project_id = approvals.project[0].id.clone();
    let mut actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Ask,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.permissions = permissions.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle
        .send_message("/permissions mode yolo")
        .await
        .expect("switch permission mode");
    let mode_changed = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::PermissionModeChanged { .. })
    })
    .await;
    assert!(matches!(
        mode_changed.kind,
        PendingEvent::PermissionModeChanged {
            mode: Some(rw_types::PermissionModeDescriptor::Yolo)
        }
    ));
    next_matching(
        &mut events,
        |kind| matches!(kind, PendingEvent::CommandFinished { name, .. } if name == "permissions"),
    )
    .await;
    assert_eq!(
        handle
            .snapshot()
            .await
            .expect("yolo snapshot")
            .permission_mode,
        Some(rw_types::PermissionModeDescriptor::Yolo)
    );
    handle
        .send_message("/permissions approvals")
        .await
        .expect("list approvals");
    let listed = next_matching(
        &mut events,
        |kind| matches!(kind, PendingEvent::CommandFinished { name, .. } if name == "permissions"),
    )
    .await;
    let PendingEvent::CommandFinished { message, .. } = listed.kind else {
        unreachable!("permission command event")
    };
    assert!(message.contains(&session_id));
    assert!(message.contains(&project_id));
    assert!(!message.contains("SESSION_SECRET_CANARY"));
    assert!(!message.contains("PROJECT_SECRET_CANARY"));
    for command in [
        format!("/permissions revoke-session {session_id}"),
        format!("/permissions revoke-project {project_id}"),
    ] {
        handle.send_message(command).await.expect("revoke approval");
        next_matching(&mut events, |kind| {
                matches!(kind, PendingEvent::CommandFinished { name, .. } if name == "permissions")
            })
            .await;
    }
    assert!(permissions.approval_snapshot().session.is_empty());
    assert!(permissions.approval_snapshot().project.is_empty());
    for command in [
        "/permissions add allow bash(cargo test*)",
        "/permissions add deny bash(rm *)",
    ] {
        assert_eq!(
            handle
                .send_message(command)
                .await
                .expect("permission command"),
            MessageDisposition::Command
        );
        next_matching(&mut events, |kind| {
                matches!(kind, PendingEvent::CommandFinished { name, .. } if name == "permissions")
            })
            .await;
    }
    assert_eq!(permissions.snapshot().session_rules.len(), 2);
    handle
        .send_message("/permissions list")
        .await
        .expect("list permissions");
    let listed = next_matching(
        &mut events,
        |kind| matches!(kind, PendingEvent::CommandFinished { name, .. } if name == "permissions"),
    )
    .await;
    assert!(matches!(
        listed.kind,
        PendingEvent::CommandFinished { message, .. }
            if message.contains("Session rules:") && message.contains("bash(cargo test*)")
    ));
    handle
        .send_message("/permissions remove bash(cargo test*)")
        .await
        .expect("remove permission");
    next_matching(
        &mut events,
        |kind| matches!(kind, PendingEvent::CommandFinished { name, .. } if name == "permissions"),
    )
    .await;
    assert_eq!(permissions.snapshot().session_rules.len(), 1);
    handle
        .send_message("/permissions mode strict")
        .await
        .expect("restore strict permission mode");
    let mode_changed = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::PermissionModeChanged { .. })
    })
    .await;
    assert!(matches!(
        mode_changed.kind,
        PendingEvent::PermissionModeChanged {
            mode: Some(rw_types::PermissionModeDescriptor::Strict)
        }
    ));
    next_matching(
        &mut events,
        |kind| matches!(kind, PendingEvent::CommandFinished { name, .. } if name == "permissions"),
    )
    .await;
    assert_eq!(
        permissions
            .authorize(
                approval_request("clear", "CLEAR_SECRET_CANARY"),
                &StaticApprover(ApprovalDecision::AllowSession),
            )
            .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(permissions.snapshot().session_approvals, 1);
    handle
        .send_message("/permissions clear-session")
        .await
        .expect("clear permissions");
    next_matching(
        &mut events,
        |kind| matches!(kind, PendingEvent::CommandFinished { name, .. } if name == "permissions"),
    )
    .await;
    assert!(permissions.snapshot().session_rules.is_empty());
    assert_eq!(permissions.snapshot().session_approvals, 0);
    assert!(permissions.snapshot().rules.is_empty());
}
