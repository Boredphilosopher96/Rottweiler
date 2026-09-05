#![cfg(test)]
use super::ActorSubagentSessionFactory;
use super::AgentLoopError;
use super::ApplyWorktreeDiffTool;
use super::Arc;
use super::CancellationToken;
use super::CapabilityManifest;
use super::ChildLifecycleReader;
use super::Cost;
use super::DurableEventSink;
use super::EngineEvent;
use super::EventMeta;
use super::FinishReason;
use super::JournalService;
use super::ModelDriver;
use super::Path;
use super::PermissionDecision;
use super::PermissionGate;
use super::Provider;
use super::ProviderEvent;
use super::ProviderModel;
use super::RejectMetadataRemove;
use super::SESSION_EVENT_VERSION;
use super::ScriptProvider;
use super::SequenceId;
use super::SessionActor;
use super::SessionActorConfig;
use super::SessionEventLog;
use super::SessionId;
use super::SubagentLimits;
use super::SubagentOrchestrator;
use super::SubagentSessionFactory;
use super::SystemEventClock;
use super::TempDir;
use super::ThinkingLevel;
use super::ToolContext;
use super::ToolLimits;
use super::ToolRegistry;
use super::TurnId;
use super::TurnStatus;
use super::Usage;
use super::WorktreeIsolation;
use super::WorktreeLimits;
use super::WorktreeSubagentSessionFactory;
use super::WriteTool;
use super::append_tool_output;
use super::async_trait;
use super::base_agent_system_turn;
use super::builtin_command_registry;
use super::builtin_hook_dispatcher;
use super::discard_rewound_subagent_record;
use super::load_session_events;
use super::project_session_events;
use super::promote_pending_recovery_record;
use super::recovery_workspace_authorized;
use super::repair_incomplete_subagent_lifecycles;
use super::test_provider_admission;
use rw_core::SubagentMetadataStore;
use rw_tools::Tool;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn actor_applies_durable_child_artifact_then_reports_conflict_without_corruption() {
    use std::process::Command;

    let fixture = TempDir::new().expect("fixture");
    let repository = fixture.path().join("repository");
    let storage = fixture.path().join("storage");
    std::fs::create_dir(&repository).expect("repository");
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&repository)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Rottweiler Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Rottweiler Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    git(&["init", "--quiet"]);
    std::fs::write(repository.join("shared.txt"), b"base\n").expect("base file");
    git(&["add", "shared.txt"]);
    git(&["commit", "--quiet", "-m", "base"]);

    let manager = WorktreeIsolation::new(
        &repository,
        storage.join("worktrees"),
        WorktreeLimits::default(),
        CancellationToken::default(),
    )
    .await
    .expect("worktree manager");
    let first_lease = manager
        .create(CancellationToken::default())
        .await
        .expect("first lease")
        .commit();
    let second_lease = manager
        .create(CancellationToken::default())
        .await
        .expect("second lease")
        .commit();
    std::fs::write(first_lease.path().join("shared.txt"), b"first child\n")
        .expect("first child edit");
    std::fs::write(second_lease.path().join("shared.txt"), b"second child\n")
        .expect("second child edit");
    let zero_usage = || Usage {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
    };
    let first = manager
        .collect(
            &first_lease,
            "first",
            zero_usage(),
            Cost::Unavailable {
                reason: "offline fixture".to_owned(),
            },
            CancellationToken::default(),
        )
        .await
        .expect("first artifact")
        .diff
        .expect("first diff");
    let second = manager
        .collect(
            &second_lease,
            "second",
            zero_usage(),
            Cost::Unavailable {
                reason: "offline fixture".to_owned(),
            },
            CancellationToken::default(),
        )
        .await
        .expect("second artifact")
        .diff
        .expect("second diff");

    let parent_session = SessionId("artifact-parent".to_owned());
    let log = SessionEventLog::open(&storage, &parent_session.0).expect("parent event log");
    let durable = DurableEventSink::new(
        log,
        storage.clone(),
        parent_session.0.clone(),
        JournalService::new(&(storage.clone())).expect("journal reads"),
    )
    .expect("durable sink");
    let meta = |sequence| EventMeta {
        protocol_version: SESSION_EVENT_VERSION,
        session_id: parent_session.clone(),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
        caused_by: None,
    };
    rw_core::commit_session_events(
        Arc::clone(&durable),
        vec![EngineEvent::TurnStarted {
            meta: meta(0),
            turn_id: TurnId("1".to_owned()),
        }],
    )
    .await
    .expect("durable turn start");
    for (sequence, name, artifact) in [
        (1_u64, "first-child", first.clone()),
        (3_u64, "second-child", second.clone()),
    ] {
        let subagent_id = rw_types::SubagentId(name.to_owned());
        let child_session_id = SessionId(format!("{name}-session"));
        rw_core::commit_session_events(
            Arc::clone(&durable),
            vec![EngineEvent::SubagentSpawned {
                meta: meta(sequence),
                subagent_id: subagent_id.clone(),
                child_session_id: child_session_id.clone(),
                task: format!("produce {name} diff"),
            }],
        )
        .await
        .expect("durable child spawn");
        rw_core::commit_session_events(
            Arc::clone(&durable),
            vec![EngineEvent::SubagentFinished {
                meta: meta(sequence + 1),
                subagent_id: subagent_id.clone(),
                result: rw_types::SubagentResult {
                    subagent_id,
                    session_id: child_session_id,
                    status: rw_types::SubagentStatus::Completed,
                    final_text: name.to_owned(),
                    touched_files: vec!["shared.txt".to_owned()],
                    diff_artifact: Some(artifact),
                    usage: zero_usage(),
                    cost: Cost::Unavailable {
                        reason: "offline fixture".to_owned(),
                    },
                    turns: 1,
                    duration_millis: 1,
                },
            }],
        )
        .await
        .expect("durable child result");
    }
    rw_core::commit_session_events(
        Arc::clone(&durable),
        vec![EngineEvent::TurnFinished {
            meta: meta(5),
            turn_id: TurnId("1".to_owned()),
            status: TurnStatus::Completed,
            usage: zero_usage(),
            cost: Cost::Unavailable {
                reason: "offline fixture".to_owned(),
            },
        }],
    )
    .await
    .expect("durable turn finish");
    let history = ChildLifecycleReader::new(Arc::clone(&durable));

    let base_tools = Arc::new(ToolRegistry::new());
    let unused_factory = ActorSubagentSessionFactory::new(
        |_launch| -> std::result::Result<SessionActorConfig, AgentLoopError> {
            panic!("fixture never spawns a child")
        },
    );
    let orchestrator = SubagentOrchestrator::new(
        SubagentLimits::default(),
        Arc::new(unused_factory),
        Arc::clone(&base_tools),
        history.clone(),
    )
    .expect("orchestrator");
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(ApplyWorktreeDiffTool::new(
            orchestrator.diff_artifact_authority(),
        )))
        .expect("apply tool");
    let scripts = vec![
        vec![
            ProviderEvent::ToolCallStart {
                id: "apply-first".to_owned(),
                name: "apply_worktree_diff".to_owned(),
            },
            ProviderEvent::ToolCallEnd {
                id: "apply-first".to_owned(),
                arguments: serde_json::json!({"artifact_id": first.id}),
            },
            ProviderEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ],
        vec![
            ProviderEvent::ToolCallStart {
                id: "apply-second".to_owned(),
                name: "apply_worktree_diff".to_owned(),
            },
            ProviderEvent::ToolCallEnd {
                id: "apply-second".to_owned(),
                arguments: serde_json::json!({"artifact_id": second.id}),
            },
            ProviderEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ],
        vec![
            ProviderEvent::TextDelta {
                text: "conflict handled".to_owned(),
            },
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        ],
    ];
    let provider: Arc<dyn Provider> = Arc::new(ScriptProvider::new(
        "artifact-apply-offline".to_owned(),
        scripts,
        0,
    ));
    let model: Arc<dyn ModelDriver> = Arc::new(
        ProviderModel::new(
            provider,
            rw_core::CompactionConfig::default(),
            rw_core::BudgetConfig::default(),
        )
        .expect("fixture concrete model"),
    );
    let actor = SessionActor::spawn(SessionActorConfig {
        ui: std::sync::Arc::new(rw_core::ui::EmptyUiRegistry),
        ui_tool_source: std::sync::Arc::new(rw_core::ui::UnavailableUiToolSource),
        budget_session_id: parent_session.clone(),
        session_id: parent_session,
        workspace_root: repository.clone(),
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
        event_sink: Arc::new(rw_core::NoopSessionEventSink::default()),
        event_clock: Arc::new(SystemEventClock),
        provider_admission: test_provider_admission(),
        secret_redactor: Arc::new(rw_core::NoopSecretRedactor),
        checkpoints: Arc::new(rw_core::NoopMutationCheckpointCoordinator),
        folder_trust: Arc::new(rw_core::NoopFolderTrustController),
        workspace_roots: Arc::new(rw_core::NoopWorkspaceRootController),
        extension_development: Arc::new(rw_core::NoopSessionExtensionController),
        resources: Arc::new(rw_core::NoopSessionResources),
        recovered: rw_core::SessionRecoveredState::default(),
        max_turns: 5,
        identical_tool_failure_limit: 3,
        max_output_tokens: 1_024,
        thinking: ThinkingLevel::Off,
        event_capacity: 128,
    })
    .expect("parent actor");
    let mut events = actor.subscribe().expect("subscription");
    actor
        .send_message("apply both durable child artifacts".to_owned())
        .await
        .expect("run parent turn");
    let mut tool_results = Vec::new();
    loop {
        let event = events.recv().await.expect("actor event");
        match event {
            EngineEvent::ToolCallFinished {
                tool_call_id,
                output,
                is_error,
                ..
            } => {
                let mut text = String::new();
                append_tool_output(&mut text, &output);
                tool_results.push((tool_call_id.0, is_error, text));
            }
            EngineEvent::TurnFinished { status, .. } => {
                assert_eq!(status, TurnStatus::Completed);
                break;
            }
            _ => {}
        }
    }

    assert_eq!(tool_results.len(), 2);
    assert_eq!(tool_results[0].0, "apply-first");
    assert!(!tool_results[0].1);
    assert!(tool_results[0].2.contains("Applied isolated diff"));
    assert_eq!(tool_results[1].0, "apply-second");
    assert!(tool_results[1].1);
    assert!(tool_results[1].2.contains("conflict"));
    assert_eq!(
        std::fs::read(repository.join("shared.txt")).expect("parent result"),
        b"first child\n"
    );
    assert!(!repository.join("shared.txt.rej").exists());
    assert!(!repository.join("shared.txt.orig").exists());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn rewound_changed_worktree_is_discarded_before_metadata_tombstone_removal() {
    use std::process::Command;

    let fixture = TempDir::new().expect("fixture");
    let repository = fixture.path().join("repository");
    let storage = fixture.path().join("storage");
    std::fs::create_dir(&repository).expect("repository");
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&repository)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Rottweiler Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Rottweiler Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    git(&["init", "--quiet"]);
    std::fs::write(repository.join("tracked.txt"), b"parent\n").expect("tracked file");
    git(&["add", "tracked.txt"]);
    git(&["commit", "--quiet", "-m", "base"]);
    let manager = WorktreeIsolation::new(
        &repository,
        storage.join("worktrees"),
        WorktreeLimits::default(),
        CancellationToken::default(),
    )
    .await
    .expect("worktree manager");
    let lease = manager
        .create(CancellationToken::default())
        .await
        .expect("lease")
        .commit();
    std::fs::write(lease.path().join("rewound.txt"), b"discard\n").expect("changed worktree");
    let lease_path = lease.path().to_path_buf();
    let parent_session_id = SessionId("parent".to_owned());
    let subagent_id = rw_types::SubagentId("rewound-child".to_owned());
    let child_session_id = SessionId("rewound-child-session".to_owned());
    let record = rw_core::SubagentRecoveryRecord {
        parent_session_id: parent_session_id.clone(),
        handle: rw_core::SubagentHandle {
            subagent_id: subagent_id.clone(),
            session_id: child_session_id.clone(),
        },
        task: "rewind fixture".to_owned(),
        agent: "fixture agent".to_owned(),
        depth: 1,
        workspace_root: std::fs::canonicalize(&repository).expect("canonical repository"),
        isolation: rw_types::SubagentIsolation::Worktree,
        worktree: Some(lease.durable_record()),
        capabilities: rw_tools::CapabilityManifest::default(),
        tool_names: Vec::new(),
        policy: rw_core::SubagentRecoveryPolicy {
            model_alias: "fast".to_owned(),
            system_prompt: None,
            permission_mode: rw_types::SessionMode::Execute,
            max_turns: 4,
        },
        phase: rw_core::SubagentRecoveryPhase::Active,
    };
    assert!(recovery_workspace_authorized(
        &record,
        std::slice::from_ref(&record.workspace_root)
    ));
    assert!(!recovery_workspace_authorized(
        &record,
        &[fixture.path().join("different-root")]
    ));
    let metadata = crate::subagent_metadata::PrivateSubagentMetadataStore::open(&storage)
        .expect("metadata store");
    metadata.save(record.clone()).await.expect("save metadata");
    let meta = |sequence| EventMeta {
        protocol_version: SESSION_EVENT_VERSION,
        session_id: parent_session_id.clone(),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
        caused_by: None,
    };
    let raw = vec![
        EngineEvent::TurnStarted {
            meta: meta(0),
            turn_id: TurnId("2".to_owned()),
        },
        EngineEvent::SubagentSpawned {
            meta: meta(1),
            subagent_id,
            child_session_id,
            task: "changed child".to_owned(),
        },
        EngineEvent::TurnFinished {
            meta: meta(2),
            turn_id: TurnId("2".to_owned()),
            status: TurnStatus::Completed,
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            cost: Cost::Unavailable {
                reason: "fixture".to_owned(),
            },
        },
        EngineEvent::ConversationRewound {
            meta: meta(3),
            to_agent_turn: 1,
            operation_id: "rewind".to_owned(),
            unrestorable_paths: Vec::new(),
        },
    ];
    let mut raw = raw;
    let mut prefix = vec![
        EngineEvent::TurnStarted {
            meta: meta(0),
            turn_id: TurnId("1".into()),
        },
        EngineEvent::TurnFinished {
            meta: meta(1),
            turn_id: TurnId("1".into()),
            status: TurnStatus::Completed,
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            cost: Cost::Unavailable {
                reason: "fixture".into(),
            },
        },
    ];
    for event in &mut raw {
        event.meta_mut().expect("durable").sequence_id.0 += 2;
    }
    prefix.extend(raw);
    let sink = DurableEventSink::new(
        SessionEventLog::open(&storage, &parent_session_id.0).expect("log"),
        storage.clone(),
        parent_session_id.0.clone(),
        JournalService::new(&storage).expect("journal"),
    )
    .expect("sink");
    rw_core::commit_session_events(Arc::clone(&sink), prefix)
        .await
        .expect("source");
    let history = ChildLifecycleReader::new(sink);

    assert!(
        discard_rewound_subagent_record(&record, &history, Some(&manager), &RejectMetadataRemove,)
            .await
            .is_err(),
        "metadata failure must retain the durable tombstone for retry"
    );
    assert!(!lease_path.exists());
    assert_eq!(
        metadata
            .load_parent_page(&parent_session_id, None)
            .expect("metadata retained")
            .records
            .into_iter()
            .map(|(record, _)| record)
            .collect::<Vec<_>>()
            .len(),
        1
    );
    assert!(
        discard_rewound_subagent_record(&record, &history, Some(&manager), &metadata,)
            .await
            .expect("idempotent discard retry")
    );
    assert!(
        metadata
            .load_parent_page(&parent_session_id, None)
            .expect("load metadata")
            .records
            .into_iter()
            .map(|(record, _)| record)
            .collect::<Vec<_>>()
            .is_empty()
    );
    assert!(String::from_utf8_lossy(&git(&["status", "--porcelain=v1"]).stdout).is_empty());

    let mut pending = record;
    pending.handle.subagent_id = rw_types::SubagentId("pending".to_owned());
    pending.handle.session_id = SessionId("pending-session".to_owned());
    pending.worktree = None;
    pending.phase = rw_core::SubagentRecoveryPhase::Pending;
    metadata.save(pending.clone()).await.expect("save pending");
    assert!(
        discard_rewound_subagent_record(&pending, &history, None, &metadata)
            .await
            .expect("discard uncommitted pending")
    );
    assert!(
        metadata
            .load_parent_page(&parent_session_id, None)
            .expect("pending removed")
            .records
            .into_iter()
            .map(|(record, _)| record)
            .collect::<Vec<_>>()
            .is_empty()
    );

    metadata
        .save(pending.clone())
        .await
        .expect("save promotable pending");
    promote_pending_recovery_record(&mut pending, &metadata)
        .await
        .expect("promote pending with durable spawn");
    assert_eq!(pending.phase, rw_core::SubagentRecoveryPhase::Active);
    assert_eq!(
        metadata
            .load_parent_page(&parent_session_id, None)
            .expect("promoted metadata")
            .records
            .into_iter()
            .map(|(record, _)| record)
            .collect::<Vec<_>>()[0]
            .phase,
        rw_core::SubagentRecoveryPhase::Active
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn crashed_worktree_child_recovers_follows_up_and_applies_after_second_restart() {
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct DurableLifecycleObserver {
        sink: Arc<DurableEventSink>,
        parent: SessionId,
        next_sequence: AtomicU64,
    }

    impl DurableLifecycleObserver {
        fn meta(&self) -> EventMeta {
            EventMeta {
                protocol_version: SESSION_EVENT_VERSION,
                session_id: self.parent.clone(),
                sequence_id: SequenceId(self.next_sequence.fetch_add(1, Ordering::SeqCst)),
                emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
                caused_by: None,
            }
        }
    }

    #[async_trait]
    impl rw_core::SubagentObserver for DurableLifecycleObserver {
        async fn spawned(
            &self,
            handle: &rw_core::SubagentHandle,
            task: &str,
        ) -> std::result::Result<(), rw_core::OrchestrationError> {
            rw_core::commit_session_events(
                Arc::clone(&self.sink),
                vec![EngineEvent::SubagentSpawned {
                    meta: self.meta(),
                    subagent_id: handle.subagent_id.clone(),
                    child_session_id: handle.session_id.clone(),
                    task: task.to_owned(),
                }],
            )
            .await
            .map(|_| ())
            .map_err(|error| rw_core::OrchestrationError::Observer(error.to_string()))
        }

        async fn finished(
            &self,
            result: &rw_types::SubagentResult,
        ) -> std::result::Result<(), rw_core::OrchestrationError> {
            rw_core::commit_session_events(
                Arc::clone(&self.sink),
                vec![EngineEvent::SubagentFinished {
                    meta: self.meta(),
                    subagent_id: result.subagent_id.clone(),
                    result: result.clone(),
                }],
            )
            .await
            .map(|_| ())
            .map_err(|error| rw_core::OrchestrationError::Observer(error.to_string()))
        }

        async fn progress(
            &self,
            _handle: &rw_core::SubagentHandle,
            _child_sequence: Option<u64>,
            _event: serde_json::Value,
        ) -> std::result::Result<(), rw_core::OrchestrationError> {
            Ok(())
        }
    }

    fn child_config(
        storage: &Path,
        session_id: &SessionId,
        workspace: &Path,
        model: Arc<dyn ModelDriver>,
        tools: Arc<ToolRegistry>,
    ) -> std::result::Result<SessionActorConfig, AgentLoopError> {
        let log = SessionEventLog::open(storage, &session_id.0)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        let events = load_session_events(&log)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        let recovered = project_session_events(&events)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        let sink = DurableEventSink::new(
            log,
            storage.to_path_buf(),
            session_id.0.clone(),
            JournalService::new(storage).expect("journal reads"),
        )
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        Ok(SessionActorConfig {
            ui: std::sync::Arc::new(rw_core::ui::EmptyUiRegistry),
            ui_tool_source: std::sync::Arc::new(rw_core::ui::UnavailableUiToolSource),
            budget_session_id: session_id.clone(),
            session_id: session_id.clone(),
            workspace_root: workspace.to_path_buf(),
            additional_workspace_roots: Vec::new(),
            workspace_generation: recovered.workspace_generation,
            initial_session_context: vec![base_agent_system_turn()],
            startup_notifications: Vec::new(),
            model_alias: "fast".to_owned(),
            model,
            tools,
            permissions: Arc::new(PermissionGate::new(PermissionDecision::Allow)),
            hooks: Arc::new(
                builtin_hook_dispatcher()
                    .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?,
            ),
            commands: Arc::new(
                builtin_command_registry()
                    .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?,
            ),
            modes: Arc::new(
                rw_ext::ModeRegistry::builtins()
                    .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?,
            ),
            event_sink: sink,
            event_clock: Arc::new(SystemEventClock),
            provider_admission: test_provider_admission(),
            secret_redactor: Arc::new(rw_core::NoopSecretRedactor),
            checkpoints: Arc::new(rw_core::NoopMutationCheckpointCoordinator),
            folder_trust: Arc::new(rw_core::NoopFolderTrustController),
            workspace_roots: Arc::new(rw_core::NoopWorkspaceRootController),
            extension_development: Arc::new(rw_core::NoopSessionExtensionController),
            resources: Arc::new(rw_core::NoopSessionResources),
            recovered,
            max_turns: 4,
            identical_tool_failure_limit: 3,
            max_output_tokens: 1_024,
            thinking: ThinkingLevel::Off,
            event_capacity: 128,
        })
    }

    let fixture = TempDir::new().expect("fixture");
    let repository = fixture.path().join("repository");
    let storage = fixture.path().join("storage");
    std::fs::create_dir(&repository).expect("repository");
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&repository)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Rottweiler Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Rottweiler Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    git(&["init", "--quiet"]);
    std::fs::write(repository.join("tracked.txt"), b"base\n").expect("tracked file");
    git(&["add", "tracked.txt"]);
    git(&["commit", "--quiet", "-m", "base"]);
    let canonical_repository = repository.canonicalize().expect("canonical repository");

    let initial_manager = WorktreeIsolation::new(
        &repository,
        storage.join("worktrees"),
        WorktreeLimits::default(),
        CancellationToken::default(),
    )
    .await
    .expect("initial worktree manager");
    let initial_lease = initial_manager
        .create(CancellationToken::default())
        .await
        .expect("initial lease")
        .commit();
    let lease_record = initial_lease.durable_record();
    let child_workspace = initial_lease.path().to_path_buf();
    let parent = SessionId("recovery-parent".to_owned());
    let handle = rw_core::SubagentHandle {
        subagent_id: rw_types::SubagentId("recoverable-child".to_owned()),
        session_id: SessionId("recoverable-child-session".to_owned()),
    };
    drop(
        SessionEventLog::open(&storage, &handle.session_id.0)
            .expect("persist empty child log before crash"),
    );
    let mut child_tools = ToolRegistry::new();
    child_tools
        .register(Arc::new(WriteTool::new(ToolLimits::default())))
        .expect("write tool");
    let child_tools = Arc::new(child_tools);
    let capabilities = CapabilityManifest::new(
        child_tools
            .descriptors()
            .into_iter()
            .flat_map(|descriptor| descriptor.capabilities.capabilities().to_vec()),
    );
    let pending = rw_core::SubagentRecoveryRecord {
        parent_session_id: parent.clone(),
        handle: handle.clone(),
        task: "recoverable fixture".to_owned(),
        agent: "fixture agent".to_owned(),
        depth: 1,
        workspace_root: canonical_repository.clone(),
        isolation: rw_types::SubagentIsolation::Worktree,
        worktree: Some(lease_record.clone()),
        capabilities,
        tool_names: vec!["write".to_owned()],
        policy: rw_core::SubagentRecoveryPolicy {
            model_alias: "fast".to_owned(),
            system_prompt: Some("complete the recovered task".to_owned()),
            permission_mode: rw_types::SessionMode::Execute,
            max_turns: 4,
        },
        phase: rw_core::SubagentRecoveryPhase::Pending,
    };
    let metadata = crate::subagent_metadata::PrivateSubagentMetadataStore::open(&storage)
        .expect("metadata store");
    metadata
        .save(pending.clone())
        .await
        .expect("persist pending metadata");
    let initial_log = SessionEventLog::open(&storage, &parent.0).expect("parent event log");
    let initial_sink = DurableEventSink::new(
        initial_log,
        storage.clone(),
        parent.0.clone(),
        JournalService::new(&(storage.clone())).expect("journal reads"),
    )
    .expect("initial parent sink");
    let meta = |sequence| EventMeta {
        protocol_version: SESSION_EVENT_VERSION,
        session_id: parent.clone(),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
        caused_by: None,
    };
    rw_core::commit_session_events(
        Arc::clone(&initial_sink),
        vec![EngineEvent::TurnStarted {
            meta: meta(0),
            turn_id: TurnId("1".to_owned()),
        }],
    )
    .await
    .expect("parent turn start");
    rw_core::commit_session_events(
        Arc::clone(&initial_sink),
        vec![EngineEvent::SubagentSpawned {
            meta: meta(1),
            subagent_id: handle.subagent_id.clone(),
            child_session_id: handle.session_id.clone(),
            task: "task interrupted after durable spawn".to_owned(),
        }],
    )
    .await
    .expect("durable spawn");
    drop(initial_sink);
    drop(initial_lease);
    drop(initial_manager);

    let parent_log = SessionEventLog::open(&storage, &parent.0).expect("reopen parent log");
    let parent_sink = DurableEventSink::new(
        parent_log,
        storage.clone(),
        parent.0.clone(),
        JournalService::new(&(storage.clone())).expect("journal reads"),
    )
    .expect("recovered parent sink");
    let history = ChildLifecycleReader::new(Arc::clone(&parent_sink));
    repair_incomplete_subagent_lifecycles(&parent_sink, &parent, &history)
        .await
        .expect("repair interrupted lifecycle");
    let repaired = parent_sink.load().expect("repaired source");
    assert!(matches!(
        repaired.last(),
        Some(EngineEvent::SubagentFinished { result, .. })
            if result.status == rw_types::SubagentStatus::Failed
    ));
    let recovered_manager = Arc::new(
        WorktreeIsolation::new(
            &repository,
            storage.join("worktrees"),
            WorktreeLimits::default(),
            CancellationToken::default(),
        )
        .await
        .expect("recovered worktree manager"),
    );
    let mut recovered_record = metadata
        .load_parent_page(&parent, None)
        .expect("load pending metadata")
        .records
        .into_iter()
        .map(|(record, _)| record)
        .collect::<Vec<_>>()
        .into_iter()
        .next()
        .expect("pending record");
    assert!(recovery_workspace_authorized(
        &recovered_record,
        std::slice::from_ref(&canonical_repository)
    ));
    assert!(
        !discard_rewound_subagent_record(
            &recovered_record,
            &history,
            Some(recovered_manager.as_ref()),
            &metadata,
        )
        .await
        .expect("retain durable recovered child")
    );
    promote_pending_recovery_record(&mut recovered_record, &metadata)
        .await
        .expect("promote recovered child");

    let scripts = vec![
        vec![
            ProviderEvent::ToolCallStart {
                id: "write-recovered".to_owned(),
                name: "write".to_owned(),
            },
            ProviderEvent::ToolCallEnd {
                id: "write-recovered".to_owned(),
                arguments: serde_json::json!({
                    "path": "recovered.txt",
                    "content": "follow-up completed\n",
                }),
            },
            ProviderEvent::Finished {
                reason: FinishReason::ToolCalls,
            },
        ],
        vec![
            ProviderEvent::TextDelta {
                text: "recovered follow-up complete".to_owned(),
            },
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        ],
    ];
    let provider: Arc<dyn Provider> = Arc::new(ScriptProvider::new(
        "recovered-child-offline".to_owned(),
        scripts,
        0,
    ));
    let model: Arc<dyn ModelDriver> = Arc::new(
        ProviderModel::new(
            provider,
            rw_core::CompactionConfig::default(),
            rw_core::BudgetConfig::default(),
        )
        .expect("fixture concrete model"),
    );
    let create_storage = storage.clone();
    let create_model = Arc::clone(&model);
    let create_tools = Arc::clone(&child_tools);
    let rebind_storage = storage.clone();
    let rebind_model = Arc::clone(&model);
    let rebind_tools = Arc::clone(&child_tools);
    let actor_factory = ActorSubagentSessionFactory::new(move |launch| {
        child_config(
            &create_storage,
            &launch.handle.session_id,
            &launch.workspace_root,
            Arc::clone(&create_model),
            Arc::clone(&create_tools),
        )
    })
    .with_rebuilder(move |session_id, workspace, _policy| {
        child_config(
            &rebind_storage,
            session_id,
            workspace,
            Arc::clone(&rebind_model),
            Arc::clone(&rebind_tools),
        )
    });
    let actor_factory: Arc<dyn SubagentSessionFactory> = Arc::new(actor_factory);
    let factory: Arc<dyn SubagentSessionFactory> = Arc::new(WorktreeSubagentSessionFactory::new(
        actor_factory,
        Arc::clone(&recovered_manager),
    ));
    let recovered_orchestrator = SubagentOrchestrator::new(
        SubagentLimits::default(),
        factory,
        Arc::clone(&child_tools),
        history.clone(),
    )
    .expect("recovered orchestrator");
    recovered_orchestrator.bind_metadata_store(Arc::new(metadata));
    recovered_orchestrator
        .recover_record(recovered_record)
        .await
        .expect("rebind recovered child");
    assert_eq!(
        recovered_orchestrator
            .worktree_recovery_record(&handle.subagent_id)
            .expect("recovered lease")
            .expect("worktree lease"),
        lease_record
    );
    let observer: Arc<dyn rw_core::SubagentObserver> = Arc::new(DurableLifecycleObserver {
        sink: Arc::clone(&parent_sink),
        parent: parent.clone(),
        next_sequence: AtomicU64::new(3),
    });
    let follow_up = recovered_orchestrator
        .follow_up(
            &parent,
            &handle.subagent_id,
            "finish the interrupted task in the same worktree".to_owned(),
            observer,
            CancellationToken::default(),
        )
        .await
        .expect("start recovered follow-up");
    assert_eq!(follow_up, handle);
    let result = recovered_orchestrator
        .wait(&follow_up)
        .await
        .expect("recovered follow-up result");
    assert_eq!(result.status, rw_types::SubagentStatus::Completed);
    let recovered_artifact = result.diff_artifact.expect("recovered durable artifact");
    assert_eq!(
        std::fs::read(child_workspace.join("recovered.txt")).expect("worktree output"),
        b"follow-up completed\n"
    );
    assert!(!repository.join("recovered.txt").exists());
    rw_core::commit_session_events(
        Arc::clone(&parent_sink),
        vec![EngineEvent::TurnFinished {
            meta: meta(5),
            turn_id: TurnId("1".to_owned()),
            status: TurnStatus::Completed,
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            cost: Cost::Unavailable {
                reason: "offline recovery fixture".to_owned(),
            },
        }],
    )
    .await
    .expect("finish recovered parent turn");
    recovered_orchestrator
        .cancel(&parent, &handle.subagent_id)
        .await
        .expect("stop recovered child actor");
    drop(recovered_orchestrator);
    drop(history);
    drop(parent_sink);
    drop(recovered_manager);
    tokio::task::yield_now().await;

    let child_events = load_session_events(
        &SessionEventLog::open(&storage, &handle.session_id.0).expect("reopen child log"),
    )
    .expect("load durable child log");
    assert!(child_events.iter().any(|event| matches!(
        event,
        EngineEvent::TurnFinished {
            status: TurnStatus::Completed,
            ..
        }
    )));

    let second_restart_log =
        SessionEventLog::open(&storage, &parent.0).expect("second parent restart");
    let second_sink = DurableEventSink::new(
        second_restart_log,
        storage.clone(),
        parent.0.clone(),
        JournalService::new(&storage).expect("journal"),
    )
    .expect("sink");
    let history = ChildLifecycleReader::new(second_sink);
    assert!(
        history
            .pending(&parent, None)
            .await
            .expect("complete recovered lifecycle")
            .1
            .is_empty()
    );
    let unused_factory = ActorSubagentSessionFactory::new(
        |_launch| -> std::result::Result<SessionActorConfig, AgentLoopError> {
            panic!("second restart only rebuilds durable authority")
        },
    );
    let second_restart_orchestrator = SubagentOrchestrator::new(
        SubagentLimits::default(),
        Arc::new(unused_factory),
        Arc::new(ToolRegistry::new()),
        history,
    )
    .expect("second restart orchestrator");
    let apply = ApplyWorktreeDiffTool::new(second_restart_orchestrator.diff_artifact_authority());
    let applied = apply
        .execute(
            &ToolContext::new(&repository)
                .expect("parent tool context")
                .with_session_id(parent),
            serde_json::json!({"artifact_id": recovered_artifact.id}),
        )
        .await
        .expect("apply artifact after second restart");
    assert_eq!(applied.data["artifact_id"], recovered_artifact.id);
    assert_eq!(
        std::fs::read(repository.join("recovered.txt")).expect("applied recovered output"),
        b"follow-up completed\n"
    );
    assert!(!repository.join("recovered.txt.rej").exists());
    assert!(!repository.join("recovered.txt.orig").exists());
}
