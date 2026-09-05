use super::*;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn cancelled_worktree_lease_rebinds_and_accepts_follow_up() {
    use std::process::Command;

    let repository = tempfile::tempdir().expect("repository");
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(repository.path())
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
    };
    git(&["init", "--quiet"]);
    std::fs::write(repository.path().join("tracked.txt"), b"base\n").expect("tracked");
    git(&["add", "tracked.txt"]);
    git(&["commit", "--quiet", "-m", "base"]);
    let private = tempfile::tempdir().expect("private");
    let isolation = Arc::new(
        WorktreeIsolation::new(
            repository.path(),
            private.path(),
            rw_tools::WorktreeLimits::default(),
            CancellationToken::default(),
        )
        .await
        .expect("isolation"),
    );
    let inner: Arc<dyn SubagentSessionFactory> = Arc::new(FakeFactory::default());
    let factory = WorktreeSubagentSessionFactory::new(inner, Arc::clone(&isolation));
    let handle = SubagentHandle {
        subagent_id: SubagentId("cancelled".to_owned()),
        session_id: SessionId("cancelled-session".to_owned()),
    };
    let mut child_request = request("first");
    child_request.isolation = SubagentIsolation::Worktree;
    child_request.workspace_root = repository.path().to_path_buf();
    let session = factory
        .create(SubagentLaunch {
            handle: handle.clone(),
            parent_session_id: SessionId("parent".to_owned()),
            depth: 1,
            request: child_request,
            tools: Arc::new(ToolRegistry::new()),
            max_turns: 4,
            workspace_root: repository.path().to_path_buf(),
            cancellation: CancellationToken::default(),
        })
        .await
        .expect("create worktree child");
    let record = session.worktree_record().expect("durable lease");
    session.cancel().await.expect("cancel child only");
    isolation
        .rebind(&record, CancellationToken::default())
        .await
        .expect("cancel preserved lease");
    drop(session);

    let rebound = factory
        .rebind(
            &handle.session_id,
            Some(repository.path()),
            Some(&record),
            None,
            &SubagentRecoveryPolicy {
                model_alias: "fast".to_owned(),
                system_prompt: None,
                permission_mode: SessionMode::Execute,
                max_turns: 4,
            },
        )
        .await
        .expect("rebind")
        .expect("rebound session");
    let result = rebound
        .run_turn(
            "follow-up".to_owned(),
            CancellationToken::default(),
            Arc::new(NoopProgress),
        )
        .await
        .expect("follow-up turn");
    assert_eq!(result.status, SubagentStatus::Completed);
    rebound.close(None).await.expect("close rebound child");
    assert!(
        isolation
            .rebind(&record, CancellationToken::default())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn children_overlap_and_concurrency_limit_fails_closed() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
    let recorded = Arc::new(RecordingObserver::default());
    let observer: Arc<dyn SubagentObserver> = recorded.clone();
    let parent = SessionId("parent".to_owned());
    let mut handles = Vec::new();
    for _ in 0..4 {
        handles.push(
            orchestrator
                .start(
                    parent.clone(),
                    request("delay:100"),
                    Arc::clone(&observer),
                    CancellationToken::default(),
                )
                .await
                .expect("start"),
        );
    }
    let exceeded = orchestrator
        .start(
            parent,
            request("delay:1"),
            Arc::clone(&observer),
            CancellationToken::default(),
        )
        .await
        .expect_err("fifth child must be rejected");
    assert!(matches!(
        exceeded,
        OrchestrationError::ConcurrencyExceeded { maximum: 4 }
    ));
    for handle in &handles {
        orchestrator.wait(handle).await.expect("result");
    }
    assert_eq!(factory.peak.load(Ordering::Acquire), 4);
}

#[tokio::test]
async fn recovery_metadata_preserves_exact_child_policy_before_the_first_turn() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), factory);
    let metadata = Arc::new(RecordingMetadataStore::default());
    orchestrator.bind_metadata_store(metadata.clone());
    let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
    let mut launch = request("delay:1");
    launch.model = "subscription-fast".to_owned();
    launch.system_prompt = Some("exact recovered prompt".to_owned());
    launch.permission_mode = SessionMode::Plan;
    launch.max_turns = Some(3);

    let handle = orchestrator
        .start(
            SessionId("parent".to_owned()),
            launch,
            observer,
            CancellationToken::default(),
        )
        .await
        .expect("start");
    let record = metadata
        .record
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("metadata saved before child turn");
    assert_eq!(record.policy.model_alias, "subscription-fast");
    assert_eq!(
        record.policy.system_prompt.as_deref(),
        Some("exact recovered prompt")
    );
    assert_eq!(record.policy.permission_mode, SessionMode::Plan);
    assert_eq!(record.policy.max_turns, 3);
    assert_eq!(record.phase, SubagentRecoveryPhase::Active);
    orchestrator.wait(&handle).await.expect("result");
}

#[tokio::test]
async fn ambiguous_spawn_observer_failure_closes_child_and_retains_closed_receipt() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
    let metadata = Arc::new(RecordingMetadataStore::default());
    orchestrator.bind_metadata_store(metadata.clone());
    let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver {
        fail_spawned: true,
        ..RecordingObserver::default()
    });
    let error = orchestrator
        .start(
            SessionId("parent".to_owned()),
            request("must-not-run"),
            observer,
            CancellationToken::default(),
        )
        .await
        .expect_err("spawn observer failure");
    assert!(error.to_string().contains("spawn fixture failure"));
    assert_eq!(factory.active.load(Ordering::Acquire), 0);
    assert_eq!(factory.cancelled.load(Ordering::Acquire), 1);
    assert_eq!(metadata.removes.load(Ordering::Acquire), 0);
    assert_eq!(
        metadata
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|record| record.phase),
        Some(SubagentRecoveryPhase::Closed)
    );
    assert_eq!(
        factory
            .closed_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
}

#[test]
fn crash_after_spawn_gets_one_artifact_free_terminal_before_continuation() {
    let first = SubagentId("first".to_owned());
    let second = SubagentId("second".to_owned());
    let mut events = vec![
        EngineEvent::SubagentSpawned {
            meta: test_event_meta(1),
            subagent_id: first.clone(),
            child_session_id: SessionId("first-session".to_owned()),
            task: "first".to_owned(),
        },
        EngineEvent::SubagentFinished {
            meta: test_event_meta(2),
            subagent_id: first.clone(),
            result: SubagentResult {
                subagent_id: first,
                session_id: SessionId("first-session".to_owned()),
                status: SubagentStatus::Completed,
                final_text: "done".to_owned(),
                touched_files: Vec::new(),
                diff_artifact: None,
                usage: zero_usage(),
                cost: Cost::Unavailable {
                    reason: "fixture".to_owned(),
                },
                turns: 1,
                duration_millis: 1,
            },
        },
        EngineEvent::SubagentSpawned {
            meta: test_event_meta(3),
            subagent_id: second.clone(),
            child_session_id: SessionId("second-session".to_owned()),
            task: "second".to_owned(),
        },
    ];
    let incomplete = incomplete_subagent_lifecycles(&events).expect("scan");
    assert_eq!(incomplete.len(), 1);
    assert_eq!(incomplete[0].subagent_id, second);
    let repair = interrupted_subagent_recovery_result(&incomplete[0]);
    assert_eq!(repair.status, SubagentStatus::Failed);
    assert!(repair.diff_artifact.is_none());
    events.push(EngineEvent::SubagentFinished {
        meta: test_event_meta(4),
        subagent_id: repair.subagent_id.clone(),
        result: repair,
    });
    assert!(
        incomplete_subagent_lifecycles(&events)
            .expect("repaired scan")
            .is_empty()
    );
}

#[tokio::test]
async fn invalid_diff_is_rejected_before_the_durable_finished_observer() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), factory);
    let recorded = Arc::new(RecordingObserver::default());
    let observer: Arc<dyn SubagentObserver> = recorded.clone();
    let result = orchestrator
        .spawn(
            SessionId("parent".to_owned()),
            request("invalid-artifact"),
            observer,
            CancellationToken::default(),
        )
        .await
        .expect("terminal result");
    assert_eq!(result.status, SubagentStatus::Failed);
    assert!(result.diff_artifact.is_none());
    let durable_results = recorded
        .results
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(durable_results.as_slice(), [result]);
}

#[tokio::test]
async fn unacknowledged_artifact_has_no_authority() {
    let orchestrator = orchestrator(SubagentLimits::default(), Arc::new(FakeFactory::default()));
    let artifact = test_artifact();
    assert!(
        orchestrator
            .diff_artifact_authority()
            .resolve(&SessionId("parent".into()), &artifact.id)
            .await
            .expect("source")
            .is_none()
    );
}

#[tokio::test]
async fn recovery_rejects_capability_drift_and_duplicate_identities() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), factory);
    let mut drifted = recovery_record("drift", "drift-session");
    drifted.capabilities = CapabilityManifest::new([ToolCapability::WriteFilesystem]);
    assert!(orchestrator.recover_record(drifted).await.is_err());

    let record = recovery_record("child", "child-session");
    orchestrator
        .recover_record(record.clone())
        .await
        .expect("first recovery");
    assert!(orchestrator.recover_record(record).await.is_err());
    assert!(
        orchestrator
            .recover_record(recovery_record("other", "child-session"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn metadata_remove_failure_keeps_child_closing_and_retry_does_not_finalize_twice() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
    let metadata = Arc::new(FailOnceRemoveMetadataStore::default());
    orchestrator.bind_metadata_store(metadata.clone());
    let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
    let handle = orchestrator
        .start(
            SessionId("parent".to_owned()),
            request("done"),
            Arc::clone(&observer),
            CancellationToken::default(),
        )
        .await
        .expect("start");
    orchestrator.wait(&handle).await.expect("finish");
    let parent = SessionId("parent".to_owned());
    assert!(
        orchestrator
            .close(&parent, &handle.subagent_id)
            .await
            .is_err()
    );
    assert!(matches!(
        orchestrator
            .follow_up(
                &parent,
                &handle.subagent_id,
                "must not run".to_owned(),
                observer,
                CancellationToken::default(),
            )
            .await,
        Err(OrchestrationError::AlreadyRunning(_))
    ));
    orchestrator
        .close(&parent, &handle.subagent_id)
        .await
        .expect("metadata cleanup retry");
    assert_eq!(factory.cancelled.load(Ordering::Acquire), 1);
    assert_eq!(metadata.removes.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn concurrent_close_calls_finalize_the_child_exactly_once() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
    let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
    let handle = orchestrator
        .start(
            SessionId("parent".to_owned()),
            request("done"),
            observer,
            CancellationToken::default(),
        )
        .await
        .expect("start");
    orchestrator.wait(&handle).await.expect("finish");
    let first = orchestrator.clone();
    let second = orchestrator.clone();
    let first_id = handle.subagent_id.clone();
    let second_id = handle.subagent_id;
    let parent = SessionId("parent".to_owned());
    let (first_result, second_result) = tokio::join!(
        first.close(&parent, &first_id),
        second.close(&parent, &second_id),
    );
    assert!(first_result.is_ok() ^ second_result.is_ok());
    assert_eq!(factory.cancelled.load(Ordering::Acquire), 1);
    assert_eq!(
        factory
            .closed_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
}

#[tokio::test]
async fn clean_follow_up_clears_the_previous_durable_artifact_before_close() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
    let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
    let handle = orchestrator
        .start(
            SessionId("parent".to_owned()),
            request("valid-artifact"),
            Arc::clone(&observer),
            CancellationToken::default(),
        )
        .await
        .expect("start");
    assert!(
        orchestrator
            .wait(&handle)
            .await
            .expect("dirty result")
            .diff_artifact
            .is_some()
    );
    let follow_up = orchestrator
        .follow_up(
            &SessionId("parent".to_owned()),
            &handle.subagent_id,
            "clean".to_owned(),
            observer,
            CancellationToken::default(),
        )
        .await
        .expect("follow-up");
    assert!(
        orchestrator
            .wait(&follow_up)
            .await
            .expect("clean result")
            .diff_artifact
            .is_none()
    );
    orchestrator
        .close(&SessionId("parent".to_owned()), &handle.subagent_id)
        .await
        .expect("close clean child");
    assert_eq!(
        factory
            .closed_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [None]
    );
}

#[tokio::test]
async fn recovered_dirty_child_closes_with_the_full_durable_artifact() {
    let factory = Arc::new(FakeFactory::default());
    let source = Arc::new(super::TestArtifactSource::default());
    let orchestrator = SubagentOrchestrator::new(
        SubagentLimits::default(),
        factory.clone(),
        Arc::new(ToolRegistry::new()),
        source.clone(),
    )
    .expect("orchestrator");
    let parent = SessionId("parent".to_owned());
    let artifact = test_artifact();
    let artifact_id = artifact.id.clone();
    let event = EngineEvent::SubagentFinished {
        meta: rw_types::EventMeta {
            protocol_version: rw_types::PROTOCOL_VERSION,
            session_id: parent.clone(),
            sequence_id: rw_types::SequenceId(1),
            emitted_at: "2026-01-01T00:00:00Z".to_owned(),
            caused_by: None,
        },
        subagent_id: SubagentId("child".to_owned()),
        result: SubagentResult {
            subagent_id: SubagentId("child".to_owned()),
            session_id: SessionId("child-session".to_owned()),
            status: SubagentStatus::Completed,
            final_text: "dirty".to_owned(),
            touched_files: vec!["src/lib.rs".to_owned()],
            diff_artifact: Some(artifact),
            usage: zero_usage(),
            cost: Cost::Unavailable {
                reason: "fixture".to_owned(),
            },
            turns: 1,
            duration_millis: 1,
        },
    };
    let EngineEvent::SubagentFinished { result, .. } = event else {
        panic!("terminal fixture");
    };
    source
        .verify_result(&parent, &result)
        .await
        .expect("fixture terminal source");
    orchestrator
        .recover_record(recovery_record("child", "child-session"))
        .await
        .expect("recover child");
    orchestrator
        .close(&parent, &SubagentId("child".to_owned()))
        .await
        .expect("close recovered dirty child");
    assert_eq!(
        factory
            .closed_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [Some(artifact_id)]
    );
}

#[tokio::test]
async fn hung_cancel_is_bounded_and_the_permit_is_eventually_released() {
    let factory = Arc::new(FakeFactory {
        hang_cancel: true,
        ..FakeFactory::default()
    });
    let limits = SubagentLimits {
        max_concurrency: 1,
        max_duration: Duration::from_millis(20),
        ..SubagentLimits::default()
    };
    let orchestrator = orchestrator(limits, factory);
    let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
    let handle = orchestrator
        .start(
            SessionId("parent".to_owned()),
            request("delay:1000"),
            Arc::clone(&observer),
            CancellationToken::default(),
        )
        .await
        .expect("start");
    assert!(
        orchestrator
            .cancel(&SessionId("parent".to_owned()), &handle.subagent_id)
            .await
            .is_err()
    );
    let _ = orchestrator.wait(&handle).await;
    let next = orchestrator
        .start(
            SessionId("parent".to_owned()),
            request("done"),
            observer,
            CancellationToken::default(),
        )
        .await
        .expect("permit released after bounded cleanup");
    let _ = orchestrator.wait(&next).await;
}

#[tokio::test]
async fn depth_limit_and_completed_child_continuity_are_enforced() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), factory);
    let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
    let root = SessionId("parent".to_owned());
    let first = orchestrator
        .start(
            root,
            request("first"),
            Arc::clone(&observer),
            CancellationToken::default(),
        )
        .await
        .expect("first");
    assert_eq!(
        orchestrator
            .wait(&first)
            .await
            .expect("first result")
            .final_text,
        "history:1"
    );
    let continued = orchestrator
        .follow_up(
            &SessionId("parent".to_owned()),
            &first.subagent_id,
            "follow-up".to_owned(),
            Arc::clone(&observer),
            CancellationToken::default(),
        )
        .await
        .expect("follow-up");
    assert_eq!(
        orchestrator
            .wait(&continued)
            .await
            .expect("continued result")
            .final_text,
        "history:2"
    );
    let second = orchestrator
        .start(
            first.session_id,
            request("second depth"),
            Arc::clone(&observer),
            CancellationToken::default(),
        )
        .await
        .expect("second depth");
    orchestrator.wait(&second).await.expect("second result");
    let exceeded = orchestrator
        .start(
            second.session_id,
            request("third depth"),
            observer,
            CancellationToken::default(),
        )
        .await
        .expect_err("depth three must fail");
    assert!(matches!(
        exceeded,
        OrchestrationError::DepthExceeded {
            requested: 3,
            maximum: 2
        }
    ));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn child_control_is_scoped_to_the_exact_parent_session() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
    let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
    let parent = SessionId("parent".to_owned());
    let victim = orchestrator
        .start(
            parent.clone(),
            request("victim"),
            Arc::clone(&observer),
            CancellationToken::default(),
        )
        .await
        .expect("start victim");
    orchestrator.wait(&victim).await.expect("finish victim");

    let nested_attacker = orchestrator
        .start(
            parent.clone(),
            request("attacker"),
            Arc::clone(&observer),
            CancellationToken::default(),
        )
        .await
        .expect("start nested attacker");
    orchestrator
        .wait(&nested_attacker)
        .await
        .expect("finish nested attacker");

    let listed = orchestrator.list_for_parent(&parent);
    assert_eq!(listed.len(), 2);
    let victim_descriptor = listed
        .iter()
        .find(|descriptor| descriptor.subagent_id == victim.subagent_id)
        .expect("victim descriptor");
    assert_eq!(victim_descriptor.task, "victim");
    assert_eq!(victim_descriptor.agent, "fixture");
    assert_eq!(victim_descriptor.model, "fast");
    assert_eq!(victim_descriptor.activity, SubagentActivity::Idle);
    assert!(
        orchestrator
            .list_for_parent(&SessionId("sibling-parent".to_owned()))
            .is_empty()
    );
    assert!(matches!(
        orchestrator
            .descriptor_for_parent(&SessionId("sibling-parent".to_owned()), &victim.subagent_id),
        Err(OrchestrationError::UnknownSubagent(_))
    ));

    for attacker in [
        SessionId("sibling-parent".to_owned()),
        nested_attacker.session_id.clone(),
    ] {
        let follow_up = orchestrator
            .follow_up(
                &attacker,
                &victim.subagent_id,
                "steal child".to_owned(),
                Arc::clone(&observer),
                CancellationToken::default(),
            )
            .await;
        assert!(matches!(
            follow_up,
            Err(OrchestrationError::UnknownSubagent(_))
        ));
        assert!(matches!(
            orchestrator.cancel(&attacker, &victim.subagent_id).await,
            Err(OrchestrationError::UnknownSubagent(_))
        ));
        assert!(matches!(
            orchestrator.close(&attacker, &victim.subagent_id).await,
            Err(OrchestrationError::UnknownSubagent(_))
        ));
    }

    let guessed = SubagentId("guessed-child-id".to_owned());
    assert!(matches!(
        orchestrator
            .follow_up(
                &parent,
                &guessed,
                "probe".to_owned(),
                Arc::clone(&observer),
                CancellationToken::default(),
            )
            .await,
        Err(OrchestrationError::UnknownSubagent(_))
    ));
    assert!(matches!(
        orchestrator.cancel(&parent, &guessed).await,
        Err(OrchestrationError::UnknownSubagent(_))
    ));
    assert!(matches!(
        orchestrator.close(&parent, &guessed).await,
        Err(OrchestrationError::UnknownSubagent(_))
    ));

    let continued = orchestrator
        .follow_up(
            &parent,
            &victim.subagent_id,
            "authorized".to_owned(),
            observer,
            CancellationToken::default(),
        )
        .await
        .expect("owner retains child control");
    assert_eq!(
        orchestrator
            .wait(&continued)
            .await
            .expect("authorized follow-up")
            .final_text,
        "history:2"
    );
    orchestrator
        .close(&parent, &victim.subagent_id)
        .await
        .expect("owner closes child");
    assert!(
        orchestrator
            .descriptor_for_parent(&parent, &victim.subagent_id)
            .is_err()
    );
    assert_eq!(factory.cancelled.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn durable_finished_failure_is_returned_to_parent() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), factory);
    let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver {
        fail_finished: true,
        ..RecordingObserver::default()
    });
    let handle = orchestrator
        .start(
            SessionId("parent".to_owned()),
            request("finish"),
            observer,
            CancellationToken::default(),
        )
        .await
        .expect("start");
    let error = orchestrator
        .wait(&handle)
        .await
        .expect_err("persistence failure");
    assert!(error.to_string().contains("fixture failure"));
}

#[tokio::test]
async fn close_permanently_removes_continuation_handle() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), factory);
    let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
    let handle = orchestrator
        .start(
            SessionId("parent".to_owned()),
            request("finish"),
            Arc::clone(&observer),
            CancellationToken::default(),
        )
        .await
        .expect("start");
    orchestrator.wait(&handle).await.expect("finish");
    orchestrator
        .close(&SessionId("parent".to_owned()), &handle.subagent_id)
        .await
        .expect("close");
    let error = orchestrator
        .follow_up(
            &SessionId("parent".to_owned()),
            &handle.subagent_id,
            "too late".to_owned(),
            observer,
            CancellationToken::default(),
        )
        .await
        .expect_err("closed child cannot continue");
    assert!(matches!(error, OrchestrationError::UnknownSubagent(_)));
}

#[tokio::test]
async fn pending_metadata_failure_happens_before_durable_spawn() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
    orchestrator.bind_metadata_store(Arc::new(FailingMetadataStore));
    let recorded = Arc::new(RecordingObserver::default());
    let observer: Arc<dyn SubagentObserver> = recorded.clone();
    let error = orchestrator
        .start(
            SessionId("parent".to_owned()),
            request("must-not-run"),
            observer,
            CancellationToken::default(),
        )
        .await
        .expect_err("metadata failure");
    assert!(error.to_string().contains("metadata persistence failed"));
    assert_eq!(factory.active.load(Ordering::Acquire), 0);
    assert_eq!(factory.cancelled.load(Ordering::Acquire), 1);
    assert_eq!(
        factory
            .closed_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
    let events = recorded
        .events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(events.is_empty());
}

#[tokio::test]
async fn pending_metadata_failure_surfaces_exact_close_failure_without_spawning() {
    let factory = Arc::new(FakeFactory {
        fail_close: true,
        ..FakeFactory::default()
    });
    let orchestrator = orchestrator(SubagentLimits::default(), factory);
    orchestrator.bind_metadata_store(Arc::new(FailingMetadataStore));
    let recorded = Arc::new(RecordingObserver::default());
    let error = orchestrator
        .start(
            SessionId("parent".to_owned()),
            request("must-not-run"),
            recorded.clone(),
            CancellationToken::default(),
        )
        .await
        .expect_err("metadata and cleanup failure");
    assert!(error.to_string().contains("metadata persistence failed"));
    assert!(error.to_string().contains("fixture close failed"));
    assert!(
        recorded
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
}

#[tokio::test]
async fn promotion_failure_closes_child_commits_terminal_and_removes_metadata() {
    let factory = Arc::new(FakeFactory::default());
    let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
    let metadata = Arc::new(FailingPromotionMetadataStore::default());
    orchestrator.bind_metadata_store(metadata.clone());
    let recorded = Arc::new(RecordingObserver::default());
    let observer: Arc<dyn SubagentObserver> = recorded.clone();
    let error = orchestrator
        .start(
            SessionId("parent".to_owned()),
            request("must-not-run"),
            observer,
            CancellationToken::default(),
        )
        .await
        .expect_err("promotion failure");
    assert!(error.to_string().contains("metadata promotion failed"));
    assert_eq!(factory.active.load(Ordering::Acquire), 0);
    assert_eq!(factory.cancelled.load(Ordering::Acquire), 1);
    assert!(
        metadata
            .retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none()
    );
    assert_eq!(
        factory
            .closed_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
    let events = recorded
        .events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(events.len(), 2);
    assert!(events[0].starts_with("spawn:"));
    assert!(events[1].starts_with("finish:"));
}

#[tokio::test]
async fn continuable_children_hold_capacity_until_their_close_is_proven() {
    let factory = Arc::new(FakeFactory::default());
    let owner = orchestrator(SubagentLimits::default(), factory.clone());
    let parent = SessionId("capacity-parent".into());
    let observer = Arc::new(RecordingObserver::default());
    let mut handles = Vec::new();
    for _ in 0..MAX_RETAINED_SUBAGENTS {
        let handle = owner
            .start(
                parent.clone(),
                request("short"),
                observer.clone(),
                CancellationToken::default(),
            )
            .await
            .expect("admitted child");
        owner.wait(&handle).await.expect("settled turn");
        handles.push(handle);
    }
    assert!(matches!(
        owner
            .start(
                parent.clone(),
                request("overflow"),
                observer.clone(),
                CancellationToken::default()
            )
            .await,
        Err(OrchestrationError::RetainedCapacityExceeded {
            maximum: MAX_RETAINED_SUBAGENTS
        })
    ));
    let released = handles.pop().expect("retained child");
    owner
        .close(&parent, &released.subagent_id)
        .await
        .expect("proven close");
    let replacement = owner
        .start(
            parent.clone(),
            request("replacement"),
            observer,
            CancellationToken::default(),
        )
        .await
        .expect("released slot reused");
    owner.wait(&replacement).await.expect("replacement settles");
    owner
        .close(&parent, &replacement.subagent_id)
        .await
        .expect("close replacement");
    for handle in handles {
        owner
            .close(&parent, &handle.subagent_id)
            .await
            .expect("close retained");
    }
    assert_eq!(
        owner.inner.retained.available_permits(),
        MAX_RETAINED_SUBAGENTS
    );
}
