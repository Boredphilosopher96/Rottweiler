#![cfg(test)]
use super::Arc;
use super::AtomicUsize;
use super::BTreeMap;
use super::BTreeSet;
use super::BackgroundProcessLimits;
use super::BackgroundProcessManager;
use super::CapturingModel;
use super::CommandFixtureMode;
use super::CommandSafetyClassifier;
use super::EngineEvent;
use super::EventMeta;
use super::ExecutionLease;
use super::FixtureRedactor;
use super::FolderTrustController;
use super::FolderTrustOperation;
use super::FolderTrustStore;
use super::HashMap;
use super::JournalService;
use super::MutationCheckpointOutcome;
use super::MutationScope;
use super::Mutex;
use super::Ordering;
use super::PathBuf;
use super::PermissionGate;
use super::PermissionOutcome;
use super::PermissionRequest;
use super::RejectingPermissionApprover;
use super::ReplayCommandExecutor;
use super::RuntimeFolderTrustController;
use super::RuntimeWorkspaceRootController;
use super::RwLock;
use super::SESSION_EVENT_VERSION;
use super::SandboxSupport;
use super::SequenceId;
use super::SessionActor;
use super::SessionEventLog;
use super::SessionId;
use super::SharedCommandFixtureRedactor;
use super::ToolCapability;
use super::ToolContext;
use super::ToolchainConfig;
use super::ToolchainRuntime;
use super::UnboundQuestionAsker;
use super::WebSearchConfig;
use super::WorkspaceRootAuthorization;
use super::abort_checkpoint_root_generation;
use super::append_checkpoint_root_generation;
use super::checkpoint_root;
use super::commit_checkpoint_root_generation;
use super::load_checkpoint_root_generation;
use super::load_session_workspace_roots;
use super::open_checkpoint_stores;
use super::probe_sandbox;
use super::restore_persisted_workspace_roots;
use super::tempdir;
use super::test_provider_admission;

#[tokio::test]
async fn runtime_trust_controller_persists_grant_and_revoke_for_slash_commands() {
    let root = tempdir().expect("root");
    let workspaces = [root.path().join("workspace"), root.path().join("added")];
    let configs = workspaces
        .each_ref()
        .map(|workspace| workspace.join(".rottweiler/config.toml"));
    for (index, config) in configs.iter().enumerate() {
        std::fs::create_dir_all(config.parent().expect("project parent"))
            .expect("project directory");
        std::fs::write(config, format!("[models]\ndefault = \"fast-{index}\"\n"))
            .expect("project config");
    }
    let workspaces =
        workspaces.map(|workspace| std::fs::canonicalize(workspace).expect("canonical workspace"));
    let ledger = root.path().join("private/trust.json");
    let controller = RuntimeFolderTrustController::new(ledger.clone(), workspaces.to_vec());

    let status = controller
        .execute(FolderTrustOperation::Status)
        .await
        .expect("status");
    assert_eq!(status.matches("state: Untrusted").count(), 2);
    for (index, workspace) in workspaces.iter().enumerate() {
        assert!(status.contains(&format!("@root/{index}")));
        assert!(!status.contains(&workspace.to_string_lossy().to_string()));
    }
    let preview = controller
        .execute(FolderTrustOperation::Grant { confirmation: None })
        .await
        .expect("grant preview");
    let stale_token = preview
        .split("`/trust grant ")
        .nth(1)
        .and_then(|tail| tail.split('`').next())
        .expect("confirmation token")
        .to_owned();
    std::fs::write(&configs[1], "[models]\ndefault = \"changed\"\n").expect("change after preview");
    assert!(
        controller
            .execute(FolderTrustOperation::Grant {
                confirmation: Some(stale_token),
            })
            .await
            .is_err(),
        "changed inventory must invalidate the bound confirmation"
    );
    assert!(
        !ledger.is_file(),
        "stale confirmation must not grant any root"
    );

    let preview = controller
        .execute(FolderTrustOperation::Grant { confirmation: None })
        .await
        .expect("fresh preview");
    assert!(preview.contains("config.toml"));
    let token = preview
        .split("`/trust grant ")
        .nth(1)
        .and_then(|tail| tail.split('`').next())
        .expect("fresh confirmation token")
        .to_owned();
    let granted = controller
        .execute(FolderTrustOperation::Grant {
            confirmation: Some(token),
        })
        .await
        .expect("confirmed grant");
    assert_eq!(granted.matches("state: Trusted").count(), 2);
    assert!(granted.contains("state: Trusted"));
    assert!(granted.contains("activates in the next session"));
    assert!(ledger.is_file(), "grant must persist the trust ledger");

    let revoked = controller
        .execute(FolderTrustOperation::Revoke)
        .await
        .expect("revoke");
    assert_eq!(revoked.matches("state: Untrusted").count(), 2);
    assert!(revoked.contains("unloads in the next session"));
    for output in [&preview, &granted, &revoked] {
        for workspace in &workspaces {
            assert!(!output.contains(&workspace.to_string_lossy().to_string()));
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn runtime_trust_grant_refuses_uninventoriable_root() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let offending = workspace.join(".agents/commands/foo.md");
    std::fs::create_dir_all(offending.parent().expect("commands")).expect("commands");
    std::fs::write(root.path().join("outside.md"), "outside").expect("outside");
    symlink(root.path().join("outside.md"), &offending).expect("symlink");
    let workspace = std::fs::canonicalize(workspace).expect("canonical workspace");
    let offending = workspace.join(".agents/commands/foo.md");
    let ledger = root.path().join("private/trust.json");
    let controller = RuntimeFolderTrustController::new(ledger.clone(), vec![workspace]);

    let status = controller
        .execute(FolderTrustOperation::Status)
        .await
        .expect("status remains available");
    assert!(status.contains("state: Untrustable"));
    assert!(status.contains(&offending.display().to_string()));
    let error = controller
        .execute(FolderTrustOperation::Grant { confirmation: None })
        .await
        .expect_err("grant must be refused");
    assert!(error.to_string().contains("inventory is incomplete"));
    assert!(error.to_string().contains(&offending.display().to_string()));
    assert!(!ledger.exists());
}

#[test]
fn aborted_workspace_root_generation_is_retry_clean() {
    let root = tempdir().expect("root");
    let primary = root.path().join("primary");
    let added = root.path().join("added");
    let checkpoint = root.path().join("checkpoint");
    std::fs::create_dir(&primary).expect("primary");
    std::fs::create_dir(&added).expect("added");
    let primary = std::fs::canonicalize(primary).expect("canonical primary");
    let added = std::fs::canonicalize(added).expect("canonical added");
    open_checkpoint_stores(&checkpoint, std::slice::from_ref(&primary)).expect("base generation");
    let appended = vec![primary.clone(), added];
    append_checkpoint_root_generation(&checkpoint, std::slice::from_ref(&primary), &appended, 1, 2)
        .expect("prepare generation");
    abort_checkpoint_root_generation(&checkpoint, 1).expect("abort generation");
    let recovered = load_checkpoint_root_generation(&checkpoint)
        .expect("load base")
        .expect("base generation");
    assert_eq!(recovered.generation, 0);
    assert_eq!(recovered.roots, vec![primary.clone()]);
    append_checkpoint_root_generation(&checkpoint, std::slice::from_ref(&primary), &appended, 1, 2)
        .expect("retry same generation");
    abort_checkpoint_root_generation(&checkpoint, 1).expect("cleanup retry");
}

#[test]
fn host_root_load_ignores_pre_event_committed_marker_after_crash() {
    let root = tempdir().expect("root");
    let storage = root.path().join("state");
    let primary = root.path().join("primary");
    let added = root.path().join("added");
    std::fs::create_dir_all(&primary).expect("primary");
    std::fs::create_dir_all(&added).expect("added");
    let primary = std::fs::canonicalize(primary).expect("canonical primary");
    let added = std::fs::canonicalize(added).expect("canonical added");
    let session_id = "pre-event-crash";
    SessionEventLog::open(&storage, session_id).expect("empty durable event log");
    let checkpoint = checkpoint_root(&storage, &primary, session_id);
    open_checkpoint_stores(&checkpoint, std::slice::from_ref(&primary))
        .expect("base root generation");
    let prepared = vec![primary.clone(), added.clone()];
    append_checkpoint_root_generation(&checkpoint, std::slice::from_ref(&primary), &prepared, 1, 1)
        .expect("prepare root generation");
    commit_checkpoint_root_generation(&checkpoint, 1).expect("prepare durable marker");
    assert_eq!(
        load_checkpoint_root_generation(&checkpoint)
            .expect("latest marker")
            .expect("committed marker")
            .roots,
        prepared,
        "fixture must represent the crash after marker persistence and before the event"
    );

    let visible = load_session_workspace_roots(
        &JournalService::new(&storage).expect("journal reads"),
        &storage,
        &primary,
        session_id,
    )
    .expect("host workspace query");
    assert_eq!(visible, vec![primary]);
    assert!(!visible.contains(&added));
}

#[tokio::test]
#[allow(clippy::if_not_else, clippy::too_many_lines)]
async fn live_root_generation_immediately_swaps_tools_sandbox_and_checkpoints() {
    let root = tempdir().expect("root");
    let primary = root.path().join("primary");
    let added = root.path().join("added");
    let private = root.path().join("private");
    std::fs::create_dir_all(&primary).expect("primary");
    std::fs::create_dir_all(&added).expect("added");
    std::fs::create_dir_all(&private).expect("private");
    std::fs::write(
        primary.join("parent-only.rs"),
        "fn uniquely_parent_bound_symbol() {}\n",
    )
    .expect("parent symbol");
    let child_command = added.join(".agents/commands/child-only.md");
    std::fs::create_dir_all(child_command.parent().expect("child command parent"))
        .expect("child command directory");
    std::fs::write(
        &child_command,
        "---\ndescription: Child-only trusted command\n---\nInspect the child workspace",
    )
    .expect("child command");
    let primary = std::fs::canonicalize(primary).expect("canonical primary");
    let added = std::fs::canonicalize(added).expect("canonical added");
    let checkpoint_root = private.join("checkpoint");
    open_checkpoint_stores(&checkpoint_root, std::slice::from_ref(&primary))
        .expect("initial checkpoint mapping");
    let lease =
        Arc::new(ExecutionLease::acquire(private.join("execution.lock")).expect("execution lease"));
    let approvals = private.join("approvals.json");
    let configured_permissions = Arc::new(
        PermissionGate::from_config(rw_core::PermissionConfig::default())
            .with_workspace_roots([&primary])
            .with_project_approval_file(approvals),
    );
    let journal_service = JournalService::new(&private).expect("journal reads");
    let controller = RuntimeWorkspaceRootController {
        native: super::super::native_registry_recipe::RootNativeBinding::Standalone,
        transcripts: crate::transcript_service::TranscriptReader::new(Arc::clone(&journal_service)),
        index_pool: Arc::new(rw_tools::WorkspaceIndexPool::default()),
        journal_service,
        checkpoint_root: checkpoint_root.clone(),
        storage_root: private.clone(),
        question_asker: Arc::new(UnboundQuestionAsker),
        offline: false,
        global_proxy: None,
        deferred_global_proxy: None,
        command_fixture_mode: CommandFixtureMode::Live,
        execution_lease: lease,
        command_safety: Arc::new(CommandSafetyClassifier::default()),
        websearch_config: WebSearchConfig::default(),
        websearch_headers: BTreeMap::new(),
        deferred_websearch_headers: None,
        background_redactor: Arc::new(SharedCommandFixtureRedactor(FixtureRedactor::default())),
        background_manager: Arc::new(BackgroundProcessManager::new(
            Arc::new(SharedCommandFixtureRedactor(FixtureRedactor::default())),
            BackgroundProcessLimits::default(),
        )),
        native_websearch_possible: false,
        trust_store_path: private.join("trust.json"),
        toolchain_config: ToolchainConfig::default(),
        toolchain_runtime: Arc::new(ToolchainRuntime::new(
            Arc::new(ReplayCommandExecutor::empty(&primary).expect("offline executor")),
            std::slice::from_ref(&primary),
        )),
        validated_wasm_hooks: Arc::from([]),
        extension_user_home: private.clone(),
        extension_user_rottweiler: private.join(".rottweiler"),
        dangerously_trust: false,
        // Simulate a trusted parent. Child extension discovery must still
        // use the child's independently assessed trust state.
        instruction_workspace_roots: Arc::new(RwLock::new(vec![primary.clone()])),
        active_nested_instruction_sources: Arc::new(RwLock::new(BTreeSet::new())),
        pending_instruction_roots: Mutex::new(HashMap::new()),
        root_authorization: WorkspaceRootAuthorization::LocalUnrestricted,
    };

    // Model a real restart: YOLO is durable in the parent event log, the
    // resumed parent actor reapplies it to its configured gate, and the
    // subsequently recovered child is rebound from that effective gate.
    let parent_session = SessionId("recovered-permission-parent".to_owned());
    let mut parent_log =
        SessionEventLog::open(&private, &parent_session.0).expect("parent event log");
    parent_log
        .append(EngineEvent::PermissionModeChanged {
            meta: EventMeta {
                protocol_version: SESSION_EVENT_VERSION,
                session_id: parent_session.clone(),
                sequence_id: SequenceId(0),
                emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                caused_by: None,
            },
            mode: Some("yolo".to_owned()),
        })
        .expect("persist parent yolo mode");
    drop(parent_log);
    let resumed_parent = controller
        .child_config(
            &private,
            &SessionId("family-fixture".into()),
            &parent_session,
            &primary,
            "fast",
            super::super::native_model_generations::ChildNativeModel {
                provider: Arc::new(CapturingModel {
                    request: Arc::new(Mutex::new(None)),
                }),
                redactor: rw_providers::FixtureRedactor::default(),
                resources: Arc::new(rw_core::NoopSessionResources),
            },
            Arc::new(rw_core::NoopSecretRedactor),
            configured_permissions.as_ref(),
            4,
            test_provider_admission(),
        )
        .await
        .expect("rebuild parent runtime after restart");
    let resumed_parent_permissions = Arc::clone(&resumed_parent.permissions);
    let resumed_parent_actor = SessionActor::spawn(resumed_parent).expect("resume parent actor");
    assert_eq!(
        resumed_parent_permissions.snapshot().runtime_mode,
        Some(rw_types::PermissionModeDescriptor::Yolo),
        "the restarted parent must restore its durable YOLO mode"
    );

    let child = controller
        .child_config(
            &private,
            &SessionId("family-fixture".into()),
            &SessionId("lease-child".to_owned()),
            &added,
            "fast",
            super::super::native_model_generations::ChildNativeModel {
                provider: Arc::new(CapturingModel {
                    request: Arc::new(Mutex::new(None)),
                }),
                redactor: rw_providers::FixtureRedactor::default(),
                resources: Arc::new(rw_core::NoopSessionResources),
            },
            Arc::new(rw_core::NoopSecretRedactor),
            resumed_parent_permissions.as_ref(),
            4,
            test_provider_admission(),
        )
        .await
        .expect("lease-root child runtime");
    assert_eq!(child.workspace_root, added);
    assert_eq!(
        child.permissions.snapshot().runtime_mode,
        Some(rw_types::PermissionModeDescriptor::Yolo),
        "fresh child inherits the parent's effective permission mode"
    );
    let rejecting_approver = RejectingPermissionApprover(AtomicUsize::new(0));
    assert_eq!(
        child
            .permissions
            .authorize(
                PermissionRequest {
                    id: "recovered-child-write".to_owned(),
                    invocation_id: rw_types::ToolInvocationId(
                        "recovered-child-write-invocation".to_owned()
                    ),
                    tool_name: "write".to_owned(),
                    arguments: serde_json::json!({
                        "path": "child-write.txt",
                        "content": "allowed without another prompt\n",
                    }),
                    capabilities: vec![ToolCapability::WriteFilesystem],
                    approval_diff: None,
                },
                &rejecting_approver,
            )
            .await,
        PermissionOutcome::Allowed,
        "the recovered child must inherit write authority from the parent"
    );
    assert_eq!(
        rejecting_approver.0.load(Ordering::SeqCst),
        0,
        "inherited YOLO authority must not invoke the approval UI"
    );
    assert!(child.additional_workspace_roots.is_empty());
    assert!(
        child
            .commands
            .descriptors()
            .all(|command| command.name() != "child-only"),
        "a trusted parent must not authorize executable child extensions"
    );
    let child_assessment = FolderTrustStore::new(private.join("trust.json"))
        .assess(&added)
        .expect("assess child trust");
    FolderTrustStore::new(private.join("trust.json"))
        .grant(&child_assessment)
        .expect("trust child");
    let trusted_child = controller
        .child_config(
            &private,
            &SessionId("family-fixture".into()),
            &SessionId("trusted-lease-child".to_owned()),
            &added,
            "fast",
            super::super::native_model_generations::ChildNativeModel {
                provider: Arc::new(CapturingModel {
                    request: Arc::new(Mutex::new(None)),
                }),
                redactor: rw_providers::FixtureRedactor::default(),
                resources: Arc::new(rw_core::NoopSessionResources),
            },
            Arc::new(rw_core::NoopSecretRedactor),
            resumed_parent_permissions.as_ref(),
            4,
            test_provider_admission(),
        )
        .await
        .expect("trusted child runtime");
    assert!(
        trusted_child
            .commands
            .descriptors()
            .any(|command| command.name() == "child-only"),
        "an independently trusted child must load its project extensions"
    );
    drop(resumed_parent_actor);
    let child_context = ToolContext::new(&added).expect("child tool context");
    let symbols = child
        .tools
        .resolve("symbols")
        .expect("lease-root symbols")
        .execute(
            &child_context,
            serde_json::json!({"pattern":"uniquely_parent_bound_symbol"}),
        )
        .await
        .expect("symbol query");
    assert!(
        !symbols.content.contains("uniquely_parent_bound_symbol"),
        "child symbol index must not retain the parent root"
    );
    let escaped = primary.join("child-escaped.txt");
    let _ = child
        .tools
        .resolve("bash")
        .expect("lease-root bash")
        .execute(
            &child_context,
            serde_json::json!({"command": format!("printf escaped > {}", escaped.display())}),
        )
        .await;
    assert!(
        !escaped.exists(),
        "lease-root bash must never retain the parent executor boundary"
    );
    verify_captured_catalog(&controller, &[primary.clone(), added.clone()]);
    let generation = rw_core::WorkspaceRootController::append_root(
        &controller,
        rw_core::WorkspaceRootRequest {
            requested: &added,
            roots: std::slice::from_ref(&primary),
            generation: 0,
            effective_from_turn: 1,
            permissions: Arc::clone(&resumed_parent_permissions),
            model: child.model.clone(),
            model_alias: &child.model_alias,
            mcp_policy: child.tools.mcp_tool_policy().clone(),
        },
    )
    .await
    .expect("prepare generation");
    rw_core::WorkspaceRootController::prepare_commit_generation(&controller, 1)
        .await
        .expect("commit generation");
    rw_core::WorkspaceRootController::finalize_generation(&controller, 1);
    let context = ToolContext::from_workspace_roots(&generation.roots).expect("tool context");
    let session = SessionId("live-root-test".to_owned());

    let known = generation
        .checkpoints
        .begin(
            &session,
            1,
            "write-added",
            &MutationScope::Paths(vec![PathBuf::from("@root/1/created.txt")]),
        )
        .await
        .expect("known checkpoint");
    generation
        .tools
        .resolve("write")
        .expect("write tool")
        .execute(
            &context,
            serde_json::json!({"path":"@root/1/created.txt","content":"live-root"}),
        )
        .await
        .expect("write added root");
    generation
        .checkpoints
        .finish(&known, MutationCheckpointOutcome::Completed)
        .await
        .expect("finish known");
    let listing = generation
        .tools
        .resolve("ls")
        .expect("ls tool")
        .execute(&context, serde_json::json!({"path":"."}))
        .await
        .expect("search roots");
    assert!(listing.content.contains("@root/1/created.txt"));
    assert!(
        generation
            .tools
            .resolve("write")
            .expect("write tool")
            .execute(
                &context,
                serde_json::json!({"path":"@root/1/../parent.txt","content":"escape"}),
            )
            .await
            .is_err()
    );
    let rewind = generation
        .checkpoints
        .prepare_apply_rewind(&session, 0, "rewind-live-root")
        .await
        .expect("rewind added root");
    assert!(!added.join("created.txt").exists());
    generation
        .checkpoints
        .acknowledge_rewind(&rewind)
        .await
        .expect("ack rewind");

    let opaque = generation
        .checkpoints
        .begin(&session, 2, "bash-added", &MutationScope::OpaqueWorkspace)
        .await
        .expect("opaque checkpoint");
    let bash_result = generation
        .tools
        .resolve("bash")
        .expect("bash tool")
        .execute(
            &context,
            serde_json::json!({"command":"printf shell > shell.txt","cwd":"@root/1"}),
        )
        .await;
    let sandbox = probe_sandbox();
    if sandbox.support != SandboxSupport::Enforced {
        generation
            .checkpoints
            .finish(&opaque, MutationCheckpointOutcome::Failed)
            .await
            .expect("finish refused opaque mutation");
        if let Ok(output) = bash_result {
            assert!(
                output.content.contains("exit code:"),
                "sandbox refusal must be visible to the model: {}",
                output.content
            );
        }
        assert!(
            !added.join("shell.txt").exists(),
            "an unavailable sandbox must fail closed before mutating the workspace"
        );
        assert!(
            sandbox.warning.is_some(),
            "an unavailable sandbox capability must explain the degradation"
        );
    } else {
        let bash_result = bash_result.expect("sandboxed bash in added root");
        assert_eq!(bash_result.data["exit_code"], 0);
        generation
            .checkpoints
            .finish(&opaque, MutationCheckpointOutcome::Completed)
            .await
            .expect("finish opaque");
        assert_eq!(
            std::fs::read(added.join("shell.txt")).expect("bash output"),
            b"shell"
        );
        let escaped = generation
                .tools
                .resolve("bash")
                .expect("bash tool")
                .execute(
                    &context,
                    serde_json::json!({"command":"printf escape > ../parent-shell.txt","cwd":"@root/1"}),
                )
                .await
                .expect("sandbox reports command exit");
        assert!(escaped.content.contains("exit code:"));
        assert!(!root.path().join("parent-shell.txt").exists());
        let rewind = generation
            .checkpoints
            .prepare_apply_rewind(&session, 1, "rewind-live-root-bash")
            .await
            .expect("rewind bash root");
        assert!(!added.join("shell.txt").exists());
        generation
            .checkpoints
            .acknowledge_rewind(&rewind)
            .await
            .expect("ack bash rewind");
    }

    let journal_service = JournalService::new(&private).expect("journal reads");
    let pending = RuntimeWorkspaceRootController {
        native: super::super::native_registry_recipe::RootNativeBinding::Standalone,
        transcripts: crate::transcript_service::TranscriptReader::new(Arc::clone(&journal_service)),
        index_pool: Arc::new(rw_tools::WorkspaceIndexPool::default()),
        journal_service,
        checkpoint_root: checkpoint_root.clone(),
        storage_root: private.clone(),
        question_asker: Arc::new(UnboundQuestionAsker),
        offline: false,
        global_proxy: None,
        deferred_global_proxy: None,
        command_fixture_mode: CommandFixtureMode::Live,
        execution_lease: Arc::new(
            ExecutionLease::acquire(private.join("execution-2.lock")).expect("second lease"),
        ),
        command_safety: Arc::new(CommandSafetyClassifier::default()),
        websearch_config: WebSearchConfig::default(),
        websearch_headers: BTreeMap::new(),
        deferred_websearch_headers: None,
        background_redactor: Arc::new(SharedCommandFixtureRedactor(FixtureRedactor::default())),
        background_manager: Arc::new(BackgroundProcessManager::new(
            Arc::new(SharedCommandFixtureRedactor(FixtureRedactor::default())),
            BackgroundProcessLimits::default(),
        )),
        native_websearch_possible: false,
        trust_store_path: private.join("trust.json"),
        toolchain_config: ToolchainConfig::default(),
        toolchain_runtime: Arc::new(ToolchainRuntime::new(
            Arc::new(ReplayCommandExecutor::empty(&primary).expect("offline executor")),
            &generation.roots,
        )),
        validated_wasm_hooks: Arc::from([]),
        extension_user_home: private.clone(),
        extension_user_rottweiler: private.join(".rottweiler"),
        dangerously_trust: false,
        instruction_workspace_roots: Arc::new(RwLock::new(generation.roots.clone())),
        active_nested_instruction_sources: Arc::new(RwLock::new(BTreeSet::new())),
        pending_instruction_roots: Mutex::new(HashMap::new()),
        root_authorization: WorkspaceRootAuthorization::LocalUnrestricted,
    };
    let third = root.path().join("third");
    std::fs::create_dir(&third).expect("third root");
    let third = std::fs::canonicalize(third).expect("canonical third");
    let _prepared = rw_core::WorkspaceRootController::append_root(
        &pending,
        rw_core::WorkspaceRootRequest {
            requested: &third,
            roots: &generation.roots,
            generation: 1,
            effective_from_turn: 2,
            permissions: Arc::clone(&generation.permissions),
            model: generation.model.clone(),
            model_alias: &child.model_alias,
            mcp_policy: child.tools.mcp_tool_policy().clone(),
        },
    )
    .await
    .expect("prepare uncommitted generation");
    let recovered =
        restore_persisted_workspace_roots(&checkpoint_root, &primary, &generation.roots, 1)
            .expect("recover committed generation")
            .expect("generation");
    assert_eq!(recovered.roots, generation.roots);
    assert!(!recovered.roots.contains(&third));
}

fn verify_captured_catalog(controller: &RuntimeWorkspaceRootController, roots: &[PathBuf]) {
    // User declarations have stable execution authority while this test changes
    // their contents; editing a project declaration would invalidate its trust
    // fingerprint and test trust revocation instead of catalog capture.
    let command = controller
        .extension_user_rottweiler
        .join("commands/captured-catalog.md");
    std::fs::create_dir_all(command.parent().expect("command directory")).expect("user commands");
    std::fs::write(
        &command,
        "---\ndescription: Captured declaration\n---\nUse the captured recipe",
    )
    .expect("captured command");
    let captured = controller
        .extension_catalog(roots)
        .expect("capture catalog");
    std::fs::write(
        &command,
        "---\ndescription: Replaced declaration\n---\nUse the changed recipe",
    )
    .expect("replace command source");
    let built = controller.prepare_tools(roots).expect("prepare registry");
    let prepared = controller
        .prepare_extensions(&captured, roots, &built)
        .expect("compose captured catalog");
    let description = |commands: &rw_ext::CommandRegistry<
        rw_core::SessionCommandContext,
        rw_core::SessionCommandOutput,
    >| {
        commands
            .descriptors()
            .find(|command| command.name() == "captured-catalog")
            .expect("captured command descriptor")
            .description()
            .to_owned()
    };
    assert_eq!(description(&prepared.commands), "Captured declaration");
    let replaced = controller.extension_catalog(roots).expect("new catalog");
    let fresh = controller
        .prepare_extensions(&replaced, roots, &built)
        .expect("compose new catalog");
    assert_eq!(description(&fresh.commands), "Replaced declaration");
    std::fs::remove_file(command).expect("remove catalog fixture");
}
