#![cfg(test)]
use super::Arc;
use super::CancellationToken;
use super::CapabilityManifest;
use super::Cost;
use super::DurableEventSink;
use super::EngineEvent;
use super::EventMeta;
use super::HistoricalPromptTool;
use super::JournalReads;
use super::Path;
use super::RecoveryProbeFactory;
use super::RecoveryProbeObserver;
use super::SESSION_EVENT_VERSION;
use super::SequenceId;
use super::SessionEventLog;
use super::SessionId;
use super::SubagentLimits;
use super::SubagentOrchestrator;
use super::TempDir;
use super::ToolDescriptor;
use super::ToolRegistry;
use super::TurnId;
use super::TurnStatus;
use super::Usage;
use super::effective_subagent_events;
use super::load_session_events;
use super::recover_subagent_tree;
use super::recovery_workspace_authorized;
use super::repair_incomplete_subagent_lifecycles;
use rw_core::SessionEventSink;
use rw_core::SubagentMetadataStore;

#[test]
fn effective_child_lifecycle_drops_rewound_branch_and_keeps_new_branch() {
    let meta = |sequence| EventMeta {
        protocol_version: SESSION_EVENT_VERSION,
        session_id: SessionId("parent".to_owned()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
        caused_by: None,
    };
    let spawn = |sequence, turn: u64, name: &str| {
        vec![
            EngineEvent::TurnStarted {
                meta: meta(sequence),
                turn_id: rw_core::TurnId(turn.to_string()),
            },
            EngineEvent::SubagentSpawned {
                meta: meta(sequence + 1),
                subagent_id: rw_types::SubagentId(name.to_owned()),
                child_session_id: SessionId(format!("session-{name}")),
                task: name.to_owned(),
            },
            EngineEvent::TurnFinished {
                meta: meta(sequence + 2),
                turn_id: rw_core::TurnId(turn.to_string()),
                status: TurnStatus::Completed,
                usage: Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    reasoning_tokens: 0,
                },
                cost: rw_core::Cost::Unavailable {
                    reason: "fixture".to_owned(),
                },
            },
        ]
    };
    let mut events = spawn(0, 1, "kept-old");
    events.extend(spawn(3, 2, "rewound"));
    events.push(EngineEvent::ConversationRewound {
        meta: meta(6),
        to_agent_turn: 1,
        operation_id: "rewind".to_owned(),
        unrestorable_paths: Vec::new(),
    });
    events.extend(spawn(7, 3, "kept-new"));

    let effective = effective_subagent_events(&events).expect("effective lifecycle");
    let names = effective
        .iter()
        .filter_map(|event| match event {
            EngineEvent::SubagentSpawned { subagent_id, .. } => Some(subagent_id.0.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["kept-old", "kept-new"]);
}

#[test]
fn tail_repair_closes_original_turn_and_rewind_removes_both_lifecycle_events() {
    let parent = SessionId("parent".to_owned());
    let child = rw_types::SubagentId("child".to_owned());
    let child_session = SessionId("child-session".to_owned());
    let meta = |sequence| EventMeta {
        protocol_version: SESSION_EVENT_VERSION,
        session_id: parent.clone(),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
        caused_by: None,
    };
    let mut events = vec![
        EngineEvent::TurnStarted {
            meta: meta(0),
            turn_id: TurnId("1".to_owned()),
        },
        EngineEvent::SubagentSpawned {
            meta: meta(1),
            subagent_id: child.clone(),
            child_session_id: child_session.clone(),
            task: "inspect".to_owned(),
        },
        EngineEvent::TurnFinished {
            meta: meta(2),
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
                reason: "fixture".to_owned(),
            },
        },
        EngineEvent::SubagentFinished {
            meta: meta(3),
            subagent_id: child.clone(),
            result: rw_core::interrupted_subagent_recovery_result(&rw_core::SubagentHandle {
                subagent_id: child,
                session_id: child_session,
            }),
        },
    ];
    let effective = effective_subagent_events(&events).expect("tail repair is effective");
    assert_eq!(effective.len(), 2);

    events.push(EngineEvent::ConversationRewound {
        meta: meta(4),
        to_agent_turn: 0,
        operation_id: "rewind-before-child".to_owned(),
        unrestorable_paths: Vec::new(),
    });
    assert!(
        effective_subagent_events(&events)
            .expect("rewound repair")
            .is_empty()
    );
}

#[tokio::test]
async fn recovery_durably_repairs_incomplete_children_once_in_spawn_order() {
    let storage = TempDir::new().expect("storage");
    let parent = SessionId("repair-parent".to_owned());
    let log = SessionEventLog::open(storage.path(), &parent.0).expect("event log");
    let sink = DurableEventSink::new(
        log,
        storage.path().to_path_buf(),
        parent.0.clone(),
        JournalReads::new(storage.path()).expect("journal reads"),
    )
    .expect("durable sink");
    let meta = |sequence| EventMeta {
        protocol_version: SESSION_EVENT_VERSION,
        session_id: parent.clone(),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
        caused_by: None,
    };
    for event in [
        EngineEvent::TurnStarted {
            meta: meta(0),
            turn_id: TurnId("1".to_owned()),
        },
        EngineEvent::SubagentSpawned {
            meta: meta(1),
            subagent_id: rw_types::SubagentId("first".to_owned()),
            child_session_id: SessionId("first-session".to_owned()),
            task: "first".to_owned(),
        },
        EngineEvent::SubagentSpawned {
            meta: meta(2),
            subagent_id: rw_types::SubagentId("second".to_owned()),
            child_session_id: SessionId("second-session".to_owned()),
            task: "second".to_owned(),
        },
    ] {
        sink.append(event).await.expect("append lifecycle");
    }
    let before = sink.load().expect("load before repair");
    let repaired = repair_incomplete_subagent_lifecycles(&sink, &parent, &before)
        .await
        .expect("repair incomplete children");
    let repaired_ids = repaired
        .iter()
        .filter_map(|event| match event {
            EngineEvent::SubagentFinished { subagent_id, .. } => Some(subagent_id.0.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(repaired_ids, ["first", "second"]);
    assert!(
        rw_core::incomplete_subagent_lifecycles(
            &effective_subagent_events(&repaired).expect("effective repaired lifecycle")
        )
        .expect("scan repaired lifecycle")
        .is_empty()
    );
    let repeated = repair_incomplete_subagent_lifecycles(&sink, &parent, &repaired)
        .await
        .expect("idempotent repair");
    assert_eq!(repeated.len(), repaired.len());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn recovery_recursively_rebinds_depth_two_children_and_is_restart_idempotent() {
    async fn append_spawn(
        storage: &Path,
        parent: &SessionId,
        child_id: &rw_types::SubagentId,
        child_session: &SessionId,
    ) -> DurableEventSink {
        let log = SessionEventLog::open(storage, &parent.0).expect("open parent log");
        let sink = DurableEventSink::new(
            log,
            storage.to_path_buf(),
            parent.0.clone(),
            JournalReads::new(storage).expect("journal reads"),
        )
        .expect("parent sink");
        let meta = |sequence| EventMeta {
            protocol_version: SESSION_EVENT_VERSION,
            session_id: parent.clone(),
            sequence_id: SequenceId(sequence),
            emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
            caused_by: None,
        };
        sink.append(EngineEvent::TurnStarted {
            meta: meta(0),
            turn_id: TurnId("1".to_owned()),
        })
        .await
        .expect("turn start");
        sink.append(EngineEvent::SubagentSpawned {
            meta: meta(1),
            subagent_id: child_id.clone(),
            child_session_id: child_session.clone(),
            task: "interrupted nested task".to_owned(),
        })
        .await
        .expect("durable nested spawn");
        sink
    }

    fn record(
        parent: &SessionId,
        child_id: &rw_types::SubagentId,
        child_session: &SessionId,
        depth: usize,
        workspace: &Path,
    ) -> rw_core::SubagentRecoveryRecord {
        rw_core::SubagentRecoveryRecord {
            parent_session_id: parent.clone(),
            handle: rw_core::SubagentHandle {
                subagent_id: child_id.clone(),
                session_id: child_session.clone(),
            },
            task: "fixture task".to_owned(),
            agent: "fixture agent".to_owned(),
            depth,
            workspace_root: workspace.to_path_buf(),
            isolation: rw_types::SubagentIsolation::Shared,
            worktree: None,
            capabilities: CapabilityManifest::default(),
            tool_names: vec!["spawn_agent".to_owned(), "apply_worktree_diff".to_owned()],
            policy: rw_core::SubagentRecoveryPolicy {
                model_alias: "fast".to_owned(),
                system_prompt: None,
                permission_mode: rw_types::SessionMode::Execute,
                max_turns: 4,
            },
            phase: rw_core::SubagentRecoveryPhase::Active,
        }
    }

    fn orchestration_registry() -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        for name in ["spawn_agent", "apply_worktree_diff"] {
            registry
                .register(Arc::new(HistoricalPromptTool(ToolDescriptor {
                    name: name.to_owned(),
                    description: format!("recovery fixture {name}"),
                    input_schema: serde_json::json!({"type": "object"}),
                    capabilities: CapabilityManifest::default(),
                })))
                .expect("fixture orchestration tool");
        }
        Arc::new(registry)
    }

    async fn assert_follow_up(
        orchestrator: &SubagentOrchestrator,
        owner: &SessionId,
        child_id: &rw_types::SubagentId,
        expected_session: &SessionId,
    ) {
        let observer: Arc<dyn rw_core::SubagentObserver> = Arc::new(RecoveryProbeObserver);
        let handle = orchestrator
            .follow_up(
                owner,
                child_id,
                "continue after restart".to_owned(),
                observer,
                CancellationToken::default(),
            )
            .await
            .expect("recovered follow-up");
        assert_eq!(&handle.session_id, expected_session);
        let result = orchestrator.wait(&handle).await.expect("follow-up result");
        assert_eq!(result.status, rw_types::SubagentStatus::Completed);
        assert!(result.final_text.contains("continue after restart"));
    }

    let fixture = TempDir::new().expect("fixture");
    let storage = fixture.path().join("storage");
    let workspace = fixture.path().join("workspace");
    std::fs::create_dir(&storage).expect("storage");
    std::fs::create_dir(&workspace).expect("workspace");
    #[cfg(unix)]
    std::fs::set_permissions(
        &storage,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("private storage");
    let workspace = workspace.canonicalize().expect("canonical workspace");
    let parent = SessionId("tree-parent".to_owned());
    let child_id = rw_types::SubagentId("tree-child".to_owned());
    let child_session = SessionId("tree-child-session".to_owned());
    let grandchild_id = rw_types::SubagentId("tree-grandchild".to_owned());
    let grandchild_session = SessionId("tree-grandchild-session".to_owned());

    let root_sink = append_spawn(&storage, &parent, &child_id, &child_session).await;
    let child_sink = append_spawn(
        &storage,
        &child_session,
        &grandchild_id,
        &grandchild_session,
    )
    .await;
    drop(child_sink);
    drop(
        SessionEventLog::open(&storage, &grandchild_session.0)
            .expect("persist empty grandchild log"),
    );
    let metadata = Arc::new(
        crate::subagent_metadata::PrivateSubagentMetadataStore::open(&storage)
            .expect("metadata store"),
    );
    metadata
        .save(record(&parent, &child_id, &child_session, 1, &workspace))
        .await
        .expect("child metadata");
    metadata
        .save(record(
            &child_session,
            &grandchild_id,
            &grandchild_session,
            2,
            &workspace,
        ))
        .await
        .expect("grandchild metadata");

    let first_factory = Arc::new(RecoveryProbeFactory::default());
    let first_rebound = Arc::clone(&first_factory.rebound);
    let first = SubagentOrchestrator::new(
        SubagentLimits {
            max_depth: 2,
            ..SubagentLimits::default()
        },
        first_factory,
        Arc::new(ToolRegistry::new()),
    )
    .expect("first orchestrator");
    first.bind_metadata_store(metadata.clone());
    let first_registry = orchestration_registry();
    first.bind_tools(Arc::clone(&first_registry));
    let initial_root_events = root_sink.load().expect("initial root events");
    recover_subagent_tree(
        &storage,
        &parent,
        &root_sink,
        &initial_root_events,
        std::slice::from_ref(&workspace),
        2,
        &first,
        metadata.as_ref(),
        None,
    )
    .await
    .expect("recover complete child tree");
    let rebound = first_rebound
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(rebound, [grandchild_session.clone(), child_session.clone()]);
    assert_follow_up(&first, &parent, &child_id, &child_session).await;
    assert_follow_up(&first, &child_session, &grandchild_id, &grandchild_session).await;
    drop(first);

    let second_factory = Arc::new(RecoveryProbeFactory::default());
    let second_rebound = Arc::clone(&second_factory.rebound);
    let second = SubagentOrchestrator::new(
        SubagentLimits {
            max_depth: 2,
            ..SubagentLimits::default()
        },
        second_factory,
        Arc::new(ToolRegistry::new()),
    )
    .expect("second orchestrator");
    second.bind_metadata_store(metadata.clone());
    let second_registry = orchestration_registry();
    second.bind_tools(Arc::clone(&second_registry));
    let restarted_root_events = root_sink.load().expect("restarted root events");
    recover_subagent_tree(
        &storage,
        &parent,
        &root_sink,
        &restarted_root_events,
        std::slice::from_ref(&workspace),
        2,
        &second,
        metadata.as_ref(),
        None,
    )
    .await
    .expect("idempotent second tree recovery");
    assert_eq!(
        second_rebound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [grandchild_session.clone(), child_session.clone()]
    );
    assert_follow_up(&second, &parent, &child_id, &child_session).await;
    assert_follow_up(&second, &child_session, &grandchild_id, &grandchild_session).await;
    assert_eq!(
        root_sink
            .load()
            .expect("root events after second recovery")
            .iter()
            .filter(|event| matches!(event, EngineEvent::SubagentFinished { .. }))
            .count(),
        1
    );
    assert_eq!(
        load_session_events(
            &SessionEventLog::open(&storage, &child_session.0).expect("child log after restart")
        )
        .expect("child events after restart")
        .iter()
        .filter(|event| matches!(event, EngineEvent::SubagentFinished { .. }))
        .count(),
        1
    );
}

#[test]
fn recovery_root_gate_rejects_noncanonical_missing_file_and_symlink_paths() {
    let fixture = TempDir::new().expect("fixture");
    let local = fixture.path().join("local");
    let hosted = fixture.path().join("hosted");
    let outside = fixture.path().join("outside");
    std::fs::create_dir(&local).expect("local root");
    std::fs::create_dir(&hosted).expect("hosted root");
    std::fs::create_dir(&outside).expect("outside root");
    let local = std::fs::canonicalize(local).expect("canonical local");
    let hosted = std::fs::canonicalize(hosted).expect("canonical hosted");
    let outside = std::fs::canonicalize(outside).expect("canonical outside");
    let mut record = rw_core::SubagentRecoveryRecord {
        parent_session_id: SessionId("parent".to_owned()),
        handle: rw_core::SubagentHandle {
            subagent_id: rw_types::SubagentId("child".to_owned()),
            session_id: SessionId("child-session".to_owned()),
        },
        task: "fixture task".to_owned(),
        agent: "fixture agent".to_owned(),
        depth: 1,
        workspace_root: local.clone(),
        isolation: rw_types::SubagentIsolation::Shared,
        worktree: None,
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
        std::slice::from_ref(&local)
    ));
    record.workspace_root.clone_from(&hosted);
    assert!(recovery_workspace_authorized(
        &record,
        std::slice::from_ref(&hosted)
    ));

    record.workspace_root = local.join("..").join("outside");
    assert!(!recovery_workspace_authorized(
        &record,
        std::slice::from_ref(&local)
    ));
    record.workspace_root = local.join("missing");
    assert!(!recovery_workspace_authorized(
        &record,
        std::slice::from_ref(&local)
    ));
    let file = local.join("file");
    std::fs::write(&file, b"not a directory").expect("file");
    record.workspace_root = file;
    assert!(!recovery_workspace_authorized(
        &record,
        std::slice::from_ref(&local)
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let alias = local.join("outside-alias");
        symlink(&outside, &alias).expect("outside symlink");
        record.workspace_root = alias;
        assert!(!recovery_workspace_authorized(
            &record,
            std::slice::from_ref(&local)
        ));
    }
}
