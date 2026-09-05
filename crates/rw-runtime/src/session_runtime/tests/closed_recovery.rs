#![cfg(test)]
use super::*;

#[tokio::test]
async fn closed_children_never_rebind_missing_workspaces_or_invent_publication() {
    for published in [false, true] {
        let fixture = TempDir::new().expect("fixture");
        let storage = fixture.path().join("storage");
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir(&storage).expect("storage");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::set_permissions(
            &storage,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("private storage");
        let workspace = workspace.canonicalize().expect("workspace");
        let parent = SessionId("closed-parent".to_owned());
        let child = rw_core::SubagentHandle {
            subagent_id: rw_types::SubagentId("closed-child".to_owned()),
            session_id: SessionId("closed-child-session".to_owned()),
        };
        let metadata = Arc::new(
            crate::subagent_metadata::PrivateSubagentMetadataStore::open(&storage)
                .expect("metadata"),
        );
        metadata
            .save(closed_record(&parent, &child, &workspace))
            .await
            .expect("closed record");
        let log = SessionEventLog::open(&storage, &parent.0).expect("log");
        let sink = DurableEventSink::new(
            log,
            storage.clone(),
            parent.0.clone(),
            JournalReads::new(&storage).expect("reads"),
        )
        .expect("sink");
        let meta = |sequence| EventMeta {
            protocol_version: SESSION_EVENT_VERSION,
            session_id: parent.clone(),
            sequence_id: SequenceId(sequence),
            emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
            caused_by: None,
        };
        if published {
            sink.append(EngineEvent::TurnStarted {
                meta: meta(0),
                turn_id: TurnId("1".to_owned()),
            })
            .await
            .expect("turn");
            sink.append(EngineEvent::SubagentSpawned {
                meta: meta(1),
                subagent_id: child.subagent_id.clone(),
                child_session_id: child.session_id.clone(),
                task: "closed before first turn".to_owned(),
            })
            .await
            .expect("spawn");
        }
        let factory = Arc::new(RecoveryProbeFactory::default());
        let rebound = Arc::clone(&factory.rebound);
        let orchestrator = SubagentOrchestrator::new(
            SubagentLimits::default(),
            factory,
            Arc::new(ToolRegistry::new()),
        )
        .expect("orchestrator");
        orchestrator.bind_metadata_store(metadata.clone());
        let events = sink.load().expect("events");
        recover_subagent_tree(
            &storage,
            &parent,
            &sink,
            &events,
            std::slice::from_ref(&workspace),
            2,
            &orchestrator,
            metadata.as_ref(),
            None,
        )
        .await
        .expect("closed recovery");
        assert!(rebound.lock().expect("rebound").is_empty());
        assert!(
            metadata
                .load_parent(&parent)
                .expect("remaining metadata")
                .is_empty()
        );
        let events = sink.load().expect("events after repair");
        if published {
            assert_eq!(events.len(), 3);
            assert!(
                matches!(events.last(), Some(EngineEvent::SubagentFinished {result, ..}) if result.session_id == child.session_id && result.status == rw_types::SubagentStatus::Failed)
            );
        } else {
            assert!(events.is_empty());
        }
        assert!(!storage.join("sessions").join(&child.session_id.0).exists());
    }
}

fn closed_record(
    parent: &SessionId,
    child: &rw_core::SubagentHandle,
    workspace: &Path,
) -> rw_core::SubagentRecoveryRecord {
    rw_core::SubagentRecoveryRecord {
        parent_session_id: parent.clone(),
        handle: child.clone(),
        task: "closed before first turn".to_owned(),
        agent: "fixture".to_owned(),
        depth: 1,
        workspace_root: workspace.join("already-removed-workspace"),
        isolation: rw_types::SubagentIsolation::Shared,
        worktree: None,
        capabilities: CapabilityManifest::default(),
        tool_names: Vec::new(),
        policy: rw_core::SubagentRecoveryPolicy {
            model_alias: "fast".to_owned(),
            system_prompt: None,
            permission_mode: rw_types::SessionMode::Execute,
            max_turns: 1,
        },
        phase: rw_core::SubagentRecoveryPhase::Closed,
    }
}
