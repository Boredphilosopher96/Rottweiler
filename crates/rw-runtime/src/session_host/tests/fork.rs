use super::*;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn production_factory_fork_composes_and_resumes_child() {
    use rw_core::{
        ClientCommand, ClientId, ClientRole, CommandMeta, CommandOutcome, ForkOperationKey,
        PROTOCOL_VERSION, PreparedForkOperation, RequestId, TurnId,
    };
    use rw_providers::{FinishReason, ProviderEvent};

    let root = tempdir().expect("root");
    let workspace = private_test_directory(&root.path().join("workspace"));
    let storage_root = private_test_directory(&root.path().join("state"));
    let factory = RuntimeSessionFactory::new(RuntimeHostOptions {
        credentials_path: storage_root.join("credentials.json"),
        storage_root: storage_root.clone(),
        config: Config::default(),
        allowed_workspaces: vec![workspace.clone()],
        permission_mode: Some(PermissionMode::Strict),
        max_turns: 2,
        provider_mode: HostedProviderMode::DeterministicReplay {
            provider_name: "fork-production-offline".to_owned(),
            scripts: vec![vec![
                ProviderEvent::TextDelta {
                    text: "completed parent turn".to_owned(),
                },
                ProviderEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ]],
            event_delay_ms: 0,
        },
        dangerously_trust: false,
        wait_for_execution_lease: false,
    })
    .await
    .expect("factory");
    let parent_id = SessionId("production-fork-parent".to_owned());
    let driver = ClientId("production-driver".to_owned());
    let parent = factory
        .create(CreateSessionRequest {
            session_id: parent_id.clone(),
            workspace: workspace.display().to_string(),
            model: None,
        })
        .await
        .expect("parent composes");
    let mut events = parent.handle().subscribe().expect("subscription");
    assert_eq!(
        parent
            .handle()
            .dispatch(ClientCommand::AttachSession {
                meta: CommandMeta {
                    protocol_version: PROTOCOL_VERSION,
                    client_id: driver.clone(),
                    request_id: RequestId("production-attach".to_owned()),
                },
                session_id: parent_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach dispatch"),
        CommandOutcome::Accepted {}
    );
    assert_eq!(
        parent
            .handle()
            .dispatch(ClientCommand::SendMessage {
                meta: CommandMeta {
                    protocol_version: PROTOCOL_VERSION,
                    client_id: driver.clone(),
                    request_id: RequestId("production-message".to_owned()),
                },
                session_id: parent_id.clone(),
                content: "complete one durable turn".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("parent message"),
        CommandOutcome::Accepted {}
    );
    loop {
        if matches!(
            events.recv().await.expect("parent event"),
            rw_core::EngineEvent::TurnFinished { .. }
        ) {
            break;
        }
    }
    let switched_model = ModelAlias("historical-parent-later-model".to_owned());
    assert_eq!(
        parent
            .handle()
            .dispatch(ClientCommand::SwitchModel {
                meta: CommandMeta {
                    protocol_version: PROTOCOL_VERSION,
                    client_id: driver.clone(),
                    request_id: RequestId("production-switch-after-boundary".to_owned()),
                },
                session_id: parent_id.clone(),
                model: switched_model.clone(),
                provider: None,
            })
            .await
            .expect("switch parent model after fork boundary"),
        CommandOutcome::Accepted {}
    );
    let model_question_id = loop {
        if let rw_core::EngineEvent::QuestionAsked {
            question_id,
            questions,
            ..
        } = events.recv().await.expect("parent model question")
            && questions.iter().any(|question| {
                question
                    .model_switch
                    .as_ref()
                    .is_some_and(|target| target.model == switched_model)
            })
        {
            break question_id;
        }
    };
    assert_eq!(
        parent
            .handle()
            .dispatch(ClientCommand::AnswerQuestion {
                meta: CommandMeta {
                    protocol_version: PROTOCOL_VERSION,
                    client_id: driver.clone(),
                    request_id: RequestId("production-switch-context".to_owned()),
                },
                session_id: parent_id.clone(),
                question_id: model_question_id.clone(),
                answers: vec![rw_core::Answer {
                    question_id: model_question_id,
                    values: vec!["pass_full_context".to_owned()],
                }],
            })
            .await
            .expect("answer parent model context question"),
        CommandOutcome::Accepted {}
    );
    loop {
        if matches!(
            events.recv().await.expect("parent model event"),
            rw_core::EngineEvent::ModelChanged { ref model, .. } if *model == switched_model
        ) {
            break;
        }
    }
    let parent_path = storage_root
        .join("sessions")
        .join(&parent_id.0)
        .join("journal")
        .join("active.jsonl");
    let parent_before = fs::read(&parent_path).expect("parent bytes");
    let child_id = SessionId("production-fork-child".to_owned());
    let fork_turn = TurnId("1".to_owned());
    let fork_payload_hash = blake3::hash(
        &serde_json::to_vec(&(&parent_id, &Some(fork_turn.clone()))).expect("stable fork payload"),
    )
    .to_hex()
    .to_string();
    let operation_key = ForkOperationKey {
        operation_id: "production-fork-operation".to_owned(),
        client_id: driver.clone(),
        request_id: RequestId("production-fork".to_owned()),
        payload_hash: fork_payload_hash.clone(),
    };
    let fork_request = ForkSessionRequest {
        operation_key: operation_key.clone(),
        parent: SessionDescriptor {
            driver_client_id: Some(driver.clone()),
            model: switched_model,
            ..parent.descriptor()
        },
        child_session_id: child_id.clone(),
        at_turn: fork_turn.clone(),
        through_sequence: None,
        include_idle_tail: false,
        driver_client_id: driver.clone(),
    };
    factory
        .prepare_fork_operation(PreparedForkOperation {
            key: operation_key,
            request: fork_request.clone(),
        })
        .await
        .expect("prepare production fork");
    let child = factory
        .fork(fork_request)
        .await
        .expect("production fork composes");
    assert_eq!(child.descriptor().session_id, child_id);
    assert_eq!(child.descriptor().model, ModelAlias("fast".to_owned()));
    let snapshot = child.handle().snapshot().await.expect("child snapshot");
    assert_eq!(snapshot.completed_turns, 1);
    assert_eq!(snapshot.driver_client_id, Some(driver));
    assert_eq!(
        fs::read(parent_path).expect("parent after fork"),
        parent_before
    );
    assert!(
        rw_store::session::AccountingLedger::open(&storage_root)
            .expect("accounting ledger")
            .entries_bounded(Some(&child.descriptor().session_id.0), 4096)
            .expect("child accounting")
            .is_empty()
    );
    assert!(
        storage_root
            .join("sessions")
            .join(&child_id.0)
            .join("metadata.json")
            .is_file()
    );

    let durable_key = ForkOperationKey {
        operation_id: "production-fork-operation".to_owned(),
        client_id: ClientId("production-driver".to_owned()),
        request_id: RequestId("production-fork".to_owned()),
        payload_hash: fork_payload_hash,
    };
    let mut journal = factory
        .load_fork_journal(&durable_key)
        .expect("load storage-committed journal")
        .expect("journal exists");
    assert!(matches!(journal.state, ForkJournalState::StorageCommitted));
    // Simulate a kill after metadata fsync but before the phase rewrite.
    journal.state = ForkJournalState::Prepared;
    factory
        .force_replace_fork_journal_for_test(&journal)
        .expect("simulate prepared crash state");
    let restart_options = (*factory.options).clone();
    drop(events);
    drop(child);
    drop(parent);
    drop(factory);
    tokio::task::yield_now().await;
    let restarted = Arc::new(
        RuntimeSessionFactory::new(restart_options.clone())
            .await
            .expect("restart recovery"),
    );
    let promoted = restarted
        .load_fork_journal(&durable_key)
        .expect("load promoted journal")
        .expect("promoted journal exists");
    assert!(matches!(promoted.state, ForkJournalState::StorageCommitted));
    assert_eq!(
        RuntimeSessionFactory::journal_operation(&promoted)
            .request
            .child_session_id,
        child_id
    );
    let restarted_client_key = ForkOperationKey {
        operation_id: durable_key.operation_id.clone(),
        client_id: ClientId("replacement-driver".to_owned()),
        request_id: RequestId("retry-after-process-restart".to_owned()),
        payload_hash: durable_key.payload_hash.clone(),
    };
    let host = rw_core::EngineHost::new(
        rw_core::EngineHostConfig::default(),
        restarted.clone(),
        restarted.clone(),
    )
    .expect("restart host");
    let replacement = rw_core::BoundClient {
        client_id: restarted_client_key.client_id.clone(),
    };
    let mut replacement_events = host
        .subscribe(replacement.clone(), None, None)
        .await
        .expect("replacement event stream");
    assert_eq!(
        host.dispatch(
            replacement,
            rw_core::ClientCommand::Fork {
                meta: rw_core::CommandMeta {
                    protocol_version: rw_core::PROTOCOL_VERSION,
                    client_id: ClientId("wire-spoof-is-replaced".to_owned()),
                    request_id: restarted_client_key.request_id.clone(),
                },
                session_id: parent_id.clone(),
                at_turn: Some(fork_turn.clone()),
                operation_id: restarted_client_key.operation_id.clone(),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let replayed_child = loop {
        if let EngineEvent::SessionForked { child, meta, .. } =
            serde_json::from_slice::<EngineEvent>(
                &replacement_events
                    .recv()
                    .await
                    .expect("replayed fork event")
                    .expect("replayed fork result")
                    .json,
            )
            .expect("encoded fork event")
        {
            assert_eq!(meta.client_id, restarted_client_key.client_id);
            assert_eq!(meta.request_id, restarted_client_key.request_id);
            break child;
        }
    };
    assert_eq!(replayed_child.session_id, child_id);
    assert_eq!(
        replayed_child.driver_client_id,
        Some(restarted_client_key.client_id.clone())
    );
    let completion = match restarted
        .load_fork_operation(&restarted_client_key)
        .await
        .expect("load completion after stable retry")
    {
        ForkOperationState::Completed(completion) => completion,
        state => panic!("stable retry did not complete: {state:?}"),
    };
    assert_eq!(completion.child, replayed_child);
    let mut racing_completion = completion.clone();
    racing_completion.command_ack_emitted_at = "2026-07-11T00:00:02.000Z".to_owned();
    racing_completion.fork_event_emitted_at = "2026-07-11T00:00:03.000Z".to_owned();
    assert_eq!(
        restarted
            .complete_fork_operation(&restarted_client_key, &racing_completion)
            .await
            .expect("racing completion returns authoritative result"),
        completion
    );
    journal.state = ForkJournalState::StorageCommitted;
    let monotonic = restarted
        .transition_fork_journal_for_test(&journal)
        .expect("stale transition is read-modify-write guarded");
    assert!(matches!(
        monotonic.state,
        ForkJournalState::Completed { .. }
    ));
    drop(replacement_events);
    drop(host);
    drop(restarted);
    tokio::task::yield_now().await;
    let mut grown_child = SessionEventLog::open(&storage_root, &child_id.0)
        .expect("open completed child for post-fork growth");
    let target_events = crate::history::MAX_HISTORY_EVENTS + 1;
    while usize::try_from(grown_child.next_sequence()).expect("child sequence") < target_events {
        let start = grown_child.next_sequence();
        let remaining =
            target_events.saturating_sub(usize::try_from(start).expect("child sequence index"));
        let count = remaining.min(10_000);
        let batch = (0..count)
            .map(|offset| {
                let sequence = start + u64::try_from(offset).expect("batch offset");
                EngineEvent::ModeChanged {
                    meta: rw_core::EventMeta {
                        protocol_version: rw_core::PROTOCOL_VERSION,
                        session_id: child_id.clone(),
                        sequence_id: rw_core::SequenceId(sequence),
                        emitted_at: "2026-07-11T00:00:04Z".to_owned(),
                        caused_by: None,
                    },
                    mode: rw_core::ModeId("execute".to_owned()),
                    definition_fingerprint: "fixture".to_owned(),
                }
            })
            .collect::<Vec<_>>();
        grown_child
            .append_batch(batch)
            .expect("append valid post-fork child events");
    }
    drop(grown_child);
    assert!(matches!(
        SessionEventLog::load_existing_bounded::<EngineEvent>(
            &storage_root,
            &child_id.0,
            crate::history::MAX_HISTORY_BYTES,
            crate::history::MAX_HISTORY_EVENTS,
        ),
        Err(rw_store::session::SessionStoreError::EventCountTooLarge { .. })
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            storage_root
                .join("sessions")
                .join(&child_id.0)
                .join("journal")
                .join("active.jsonl"),
            fs::Permissions::from_mode(0o000),
        )
        .expect("install completed-child no-read canary");
        fs::set_permissions(
            crate::session_runtime::checkpoint_root(&storage_root, &workspace, &child_id.0)
                .join("workspace-roots.json"),
            fs::Permissions::from_mode(0o000),
        )
        .expect("install completed-child root-journal no-read canary");
    }
    let reloaded = Arc::new(
        RuntimeSessionFactory::new(restart_options)
            .await
            .expect("completed restart"),
    );
    assert_eq!(
        reloaded
            .load_fork_operation(&durable_key)
            .await
            .expect("reload completed result"),
        ForkOperationState::Completed(completion.clone())
    );
    let second_restart_key = ForkOperationKey {
        operation_id: durable_key.operation_id.clone(),
        client_id: ClientId("second-replacement-driver".to_owned()),
        request_id: RequestId("retry-after-second-restart".to_owned()),
        payload_hash: durable_key.payload_hash.clone(),
    };
    assert_eq!(
        reloaded
            .load_fork_operation(&second_restart_key)
            .await
            .expect("stable operation id survives client and request replacement"),
        ForkOperationState::Completed(completion.clone())
    );
    let host = rw_core::EngineHost::new(
        rw_core::EngineHostConfig::default(),
        reloaded.clone(),
        reloaded.clone(),
    )
    .expect("restart host");
    let replacement = rw_core::BoundClient {
        client_id: second_restart_key.client_id.clone(),
    };
    let mut replacement_events = host
        .subscribe(replacement.clone(), None, None)
        .await
        .expect("replacement event stream");
    assert_eq!(
        host.dispatch(
            replacement,
            rw_core::ClientCommand::Fork {
                meta: rw_core::CommandMeta {
                    protocol_version: rw_core::PROTOCOL_VERSION,
                    client_id: ClientId("wire-spoof-is-replaced".to_owned()),
                    request_id: second_restart_key.request_id.clone(),
                },
                session_id: completion.parent_session_id.clone(),
                at_turn: Some(completion.at_turn.clone()),
                operation_id: second_restart_key.operation_id.clone(),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let replayed_child = loop {
        if let EngineEvent::SessionForked { child, meta, .. } =
            serde_json::from_slice::<EngineEvent>(
                &replacement_events
                    .recv()
                    .await
                    .expect("replayed fork event")
                    .expect("replayed fork result")
                    .json,
            )
            .expect("encoded fork event")
        {
            assert_eq!(meta.client_id, second_restart_key.client_id);
            assert_eq!(meta.request_id, second_restart_key.request_id);
            break child;
        }
    };
    assert_eq!(replayed_child.session_id, completion.child.session_id);
    let conflict = ForkOperationKey {
        payload_hash: "b".repeat(64),
        ..durable_key
    };
    assert_eq!(
        reloaded
            .load_fork_operation(&conflict)
            .await
            .expect_err("payload conflict"),
        HostError::RequestConflict
    );
}

#[tokio::test]
async fn prepared_fork_recovery_cleans_partial_trees_and_keeps_child_identity() {
    let root = tempdir().expect("root");
    let workspace = private_test_directory(&root.path().join("workspace"));
    let factory = factory(root.path(), &workspace).await;
    let parent = factory
        .create(CreateSessionRequest {
            session_id: SessionId("journal-parent".to_owned()),
            workspace: workspace.display().to_string(),
            model: None,
        })
        .await
        .expect("parent");
    let key = ForkOperationKey {
        operation_id: "journal-operation".to_owned(),
        client_id: rw_core::ClientId("journal-client".to_owned()),
        request_id: rw_core::RequestId("journal-request".to_owned()),
        payload_hash: "c".repeat(64),
    };
    let child = SessionId("journal-child".to_owned());
    let operation = PreparedForkOperation {
        key: key.clone(),
        request: ForkSessionRequest {
            operation_key: key.clone(),
            parent: parent.descriptor(),
            child_session_id: child.clone(),
            at_turn: rw_core::TurnId("0".to_owned()),
            through_sequence: None,
            include_idle_tail: false,
            driver_client_id: key.client_id.clone(),
        },
    };
    factory
        .prepare_fork_operation(operation.clone())
        .await
        .expect("prepare");
    let session_tree = factory.options.storage_root.join("sessions").join(&child.0);
    fs::create_dir_all(session_tree.join("journal")).expect("partial session tree");
    fs::write(session_tree.join("journal/active.jsonl"), b"partial").expect("partial log");
    fs::write(session_tree.join(".metadata-crash.tmp"), br#"{"version":1"#)
        .expect("unpublished metadata temporary");
    let digest = blake3::hash(workspace.as_os_str().as_encoded_bytes())
        .to_hex()
        .to_string();
    let checkpoint_tree = factory
        .options
        .storage_root
        .join("workspaces")
        .join(digest)
        .join("sessions")
        .join(&child.0);
    fs::create_dir_all(&checkpoint_tree).expect("partial checkpoint tree");
    fs::write(checkpoint_tree.join("partial"), b"partial").expect("partial checkpoint");
    let restarted = RuntimeSessionFactory::new((*factory.options).clone())
        .await
        .expect("recover");
    assert!(!session_tree.exists());
    assert!(!checkpoint_tree.exists());
    assert_eq!(
        restarted
            .load_fork_operation(&key)
            .await
            .expect("load prepared"),
        ForkOperationState::Pending(operation)
    );
}

#[tokio::test]
async fn session_capacity_rejection_abandons_prepared_fork_journal() {
    use rw_core::{
        BoundClient, ClientCommand, ClientId, ClientRole, CommandMeta, CommandOutcome, EngineHost,
        EngineHostConfig, PROTOCOL_VERSION, RequestId,
    };

    let root = tempdir().expect("root");
    let workspace = private_test_directory(&root.path().join("workspace"));
    let factory = Arc::new(factory(root.path(), &workspace).await);
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 1,
            max_deduplicated_requests: 64,
        },
        factory.clone(),
        factory.clone(),
    )
    .expect("host");
    let parent = SessionId("capacity-parent".to_owned());
    host.prepare_session(
        CreateSessionRequest {
            session_id: parent.clone(),
            workspace: workspace.display().to_string(),
            model: None,
        },
        false,
    )
    .await
    .expect("parent");
    let driver = BoundClient {
        client_id: ClientId("capacity-driver".to_owned()),
    };
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::AttachSession {
                meta: CommandMeta {
                    protocol_version: PROTOCOL_VERSION,
                    client_id: driver.client_id.clone(),
                    request_id: RequestId("capacity-attach".to_owned()),
                },
                session_id: parent.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let outcome = host
        .dispatch(
            driver.clone(),
            ClientCommand::Fork {
                meta: CommandMeta {
                    protocol_version: PROTOCOL_VERSION,
                    client_id: driver.client_id,
                    request_id: RequestId("capacity-fork".to_owned()),
                },
                session_id: parent,
                at_turn: None,
                operation_id: "capacity-fork-operation".to_owned(),
            },
        )
        .await
        .outcome;
    assert!(matches!(
        outcome,
        CommandOutcome::Rejected { error } if error.code == "session_capacity"
    ));
    assert!(
        fs::read_dir(factory.fork_journal_directory())
            .expect("journal directory")
            .all(|entry| entry
                .expect("journal entry")
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("json"))
    );
}

#[tokio::test]
#[cfg(unix)]
async fn fork_journal_cross_process_lock_helper() {
    let Ok(root) = std::env::var("RW_TEST_FORK_LOCK_ROOT") else {
        return;
    };
    let workspace =
        PathBuf::from(std::env::var("RW_TEST_FORK_LOCK_WORKSPACE").expect("helper workspace"));
    let ready = PathBuf::from(std::env::var("RW_TEST_FORK_LOCK_READY").expect("helper ready"));
    let release =
        PathBuf::from(std::env::var("RW_TEST_FORK_LOCK_RELEASE").expect("helper release"));
    let factory = factory(Path::new(&root), &workspace).await;
    let _lock = factory
        .acquire_fork_journal_lock()
        .expect("helper acquires lock");
    fs::write(ready, b"ready").expect("helper ready marker");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !release.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(release.exists(), "parent releases helper lock");
}

#[tokio::test]
#[cfg(unix)]
async fn fork_recovery_waits_for_cross_process_journal_lock() {
    let root = tempdir().expect("root");
    let workspace = private_test_directory(&root.path().join("workspace"));
    let factory = factory(root.path(), &workspace).await;
    let options = (*factory.options).clone();
    let ready = root.path().join("lock-ready");
    let release = root.path().join("lock-release");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg("session_host::tests::fork::fork_journal_cross_process_lock_helper")
        .arg("--nocapture")
        .env("RW_TEST_FORK_LOCK_ROOT", root.path())
        .env("RW_TEST_FORK_LOCK_WORKSPACE", &workspace)
        .env("RW_TEST_FORK_LOCK_READY", &ready)
        .env("RW_TEST_FORK_LOCK_RELEASE", &release)
        .spawn()
        .expect("lock helper");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "helper acquired cross-process lock");

    let (send, receive) = std::sync::mpsc::channel();
    let recovery = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("recovery runtime");
        let result = runtime.block_on(RuntimeSessionFactory::new(options));
        send.send(result.is_ok()).expect("recovery result");
        drop(result);
    });
    assert!(
        receive.recv_timeout(Duration::from_millis(100)).is_err(),
        "recovery must wait while another process owns the journal lock"
    );
    fs::write(&release, b"release").expect("release marker");
    assert!(child.wait().expect("helper exit").success());
    assert!(
        receive
            .recv_timeout(Duration::from_secs(5))
            .expect("recovery completes")
    );
    recovery.join().expect("recovery thread");
}

#[tokio::test]
#[cfg(unix)]
async fn fork_journal_rejects_unexpected_symlink_hardlink_and_oversized_entries() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let root = tempdir().expect("root");
    let workspace = private_test_directory(&root.path().join("workspace"));
    let factory = factory(root.path(), &workspace).await;
    let options = (*factory.options).clone();
    let directory = factory.fork_journal_directory();

    let unpublished = directory.join(".fork-crash.tmp");
    fs::write(&unpublished, br#"{"version":1"#).expect("unpublished journal temporary");
    fs::set_permissions(&unpublished, fs::Permissions::from_mode(0o600))
        .expect("private unpublished journal");
    RuntimeSessionFactory::new(options.clone())
        .await
        .expect("orphan temporary is recoverable");
    assert!(!unpublished.exists());

    fs::write(directory.join("unexpected"), b"x").expect("unexpected entry");
    assert!(RuntimeSessionFactory::new(options.clone()).await.is_err());
    fs::remove_file(directory.join("unexpected")).expect("remove unexpected");

    let outside = root.path().join("outside");
    fs::write(&outside, b"{}").expect("outside");
    symlink(&outside, directory.join(format!("{}.json", "a".repeat(64)))).expect("symlink");
    assert!(RuntimeSessionFactory::new(options.clone()).await.is_err());
    fs::remove_file(directory.join(format!("{}.json", "a".repeat(64)))).expect("remove symlink");

    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).expect("private source");
    fs::hard_link(&outside, directory.join(format!("{}.json", "b".repeat(64)))).expect("hardlink");
    assert!(RuntimeSessionFactory::new(options.clone()).await.is_err());
    fs::remove_file(directory.join(format!("{}.json", "b".repeat(64)))).expect("remove hardlink");

    let oversized = directory.join(format!("{}.json", "c".repeat(64)));
    fs::write(&oversized, vec![b'x'; MAX_FORK_JOURNAL_BYTES + 1]).expect("oversized");
    fs::set_permissions(&oversized, fs::Permissions::from_mode(0o600)).expect("private file");
    assert!(RuntimeSessionFactory::new(options).await.is_err());
}
