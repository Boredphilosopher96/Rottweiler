use super::*;

#[tokio::test]
async fn shutdown_wakes_opening_waiters_and_never_inserts_late_resume() {
    let (host, factory) = host(2);
    factory.block_resume.store(true, Ordering::Release);
    let session_id = SessionId("shutdown-resume-race".to_owned());
    let owner = tokio::spawn({
        let host = host.clone();
        let session_id = session_id.clone();
        async move { host.resume_session(&session_id).await }
    });
    tokio::time::timeout(Duration::from_secs(1), factory.resume_started.notified())
        .await
        .expect("resume entered factory");

    let opening = {
        let registry = host.registry.lock().await;
        match registry.sessions.get(&session_id) {
            Some(SessionSlot::Opening(completed)) => completed.clone(),
            Some(SessionSlot::Ready(_)) | None => panic!("opening reservation"),
        }
    };
    let waiter = tokio::spawn({
        let host = host.clone();
        let session_id = session_id.clone();
        async move { host.resume_session(&session_id).await }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while opening.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second resume registered as an opening waiter");

    assert_eq!(
        host.dispatch(
            BoundClient {
                client_id: ClientId("shutdown-client".to_owned()),
            },
            ClientCommand::ShutdownHost {
                meta: meta("spoofed", "shutdown-resume"),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted
    );
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("opening waiter woke")
            .expect("waiter task"),
        Err(HostError::ShuttingDown)
    ));

    factory.resume_release.notify_one();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), owner)
            .await
            .expect("resume owner finished")
            .expect("owner task"),
        Err(HostError::ShuttingDown)
    ));
    assert!(host.session(&session_id).await.is_none());
    assert!(host.registry.lock().await.sessions.is_empty());
    assert_eq!(factory.shutdowns.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn initial_preparation_reserves_identity_before_blocking_factory_work() {
    let (host, factory) = host(2);
    factory.block_create.store(true, Ordering::Release);
    let session_id = SessionId("blocked-authorized-vault".to_owned());
    let readiness_published = Arc::new(AtomicBool::new(false));
    let preparation = tokio::spawn({
        let host = host.clone();
        let session_id = session_id.clone();
        let readiness_published = Arc::clone(&readiness_published);
        async move {
            let inspection_host = host.clone();
            let inspection_session = session_id.clone();
            host.prepare_session_after_reservation(
                CreateSessionRequest {
                    session_id,
                    workspace: "workspace".to_owned(),
                    model: None,
                },
                false,
                move || {
                    let registry = inspection_host
                        .registry
                        .try_lock()
                        .expect("reservation callback runs outside the registry lock");
                    assert!(matches!(
                        registry.sessions.get(&inspection_session),
                        Some(SessionSlot::Opening(_))
                    ));
                    readiness_published.store(true, Ordering::Release);
                },
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(1), factory.create_started.notified())
        .await
        .expect("session preparation entered the blocking credential/composition boundary");
    assert!(
        readiness_published.load(Ordering::Acquire),
        "authenticated readiness must publish after the initial reservation and before factory work"
    );

    let opening = {
        let registry = host.registry.lock().await;
        match registry.sessions.get(&session_id) {
            Some(SessionSlot::Opening(completed)) => completed.clone(),
            Some(SessionSlot::Ready(_)) | None => panic!("exact opening reservation"),
        }
    };
    let reconnect = tokio::spawn({
        let host = host.clone();
        let session_id = session_id.clone();
        async move { host.resume_session(&session_id).await }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while opening.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("early reconnect joined the initial opening");
    assert_eq!(
        factory.resumes.load(Ordering::Acquire),
        0,
        "the reconnect must not start a competing session resume"
    );

    factory.create_release.notify_one();
    preparation
        .await
        .expect("preparation task")
        .expect("prepared session");
    reconnect
        .await
        .expect("reconnect task")
        .expect("reconnect joined prepared session");
    assert!(host.session(&session_id).await.is_some());
    assert_eq!(factory.resumes.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn initial_resume_publishes_readiness_only_after_reserving_reconnect_identity() {
    let (host, factory) = host(2);
    factory.block_resume.store(true, Ordering::Release);
    let session_id = SessionId("blocked-initial-resume".to_owned());
    let readiness_published = Arc::new(AtomicBool::new(false));
    let preparation = tokio::spawn({
        let host = host.clone();
        let session_id = session_id.clone();
        let readiness_published = Arc::clone(&readiness_published);
        async move {
            let inspection_host = host.clone();
            let inspection_session = session_id.clone();
            host.prepare_session_after_reservation(
                CreateSessionRequest {
                    session_id,
                    workspace: "workspace".to_owned(),
                    model: None,
                },
                true,
                move || {
                    let registry = inspection_host
                        .registry
                        .try_lock()
                        .expect("reservation callback runs outside the registry lock");
                    assert!(matches!(
                        registry.sessions.get(&inspection_session),
                        Some(SessionSlot::Opening(_))
                    ));
                    readiness_published.store(true, Ordering::Release);
                },
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(1), factory.resume_started.notified())
        .await
        .expect("initial resume entered the blocking credential/composition boundary");
    assert!(readiness_published.load(Ordering::Acquire));

    let reconnect = tokio::spawn({
        let host = host.clone();
        let session_id = session_id.clone();
        async move { host.resume_session(&session_id).await }
    });
    let opening = {
        let registry = host.registry.lock().await;
        match registry.sessions.get(&session_id) {
            Some(SessionSlot::Opening(completed)) => completed.clone(),
            Some(SessionSlot::Ready(_)) | None => panic!("initial resume reservation"),
        }
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        while opening.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reconnect joined the initial resume reservation");
    assert_eq!(
        factory.resumes.load(Ordering::Acquire),
        1,
        "the reconnect must join the initial resume instead of opening a competitor"
    );

    factory.resume_release.notify_one();
    preparation
        .await
        .expect("preparation task")
        .expect("prepared resumed session");
    reconnect
        .await
        .expect("reconnect task")
        .expect("reconnect joined resumed session");
    assert!(host.session(&session_id).await.is_some());
    assert_eq!(factory.resumes.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn shutdown_never_inserts_a_session_created_after_shutdown_started() {
    let (host, factory) = host(1);
    factory.block_create.store(true, Ordering::Release);
    let session_id = SessionId("shutdown-create-race".to_owned());
    let create = tokio::spawn({
        let host = host.clone();
        let session_id = session_id.clone();
        async move {
            host.create_session(CreateSessionRequest {
                session_id,
                workspace: "workspace".to_owned(),
                model: None,
            })
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(1), factory.create_started.notified())
        .await
        .expect("create entered factory");
    assert_eq!(
        host.dispatch(
            BoundClient {
                client_id: ClientId("shutdown-client".to_owned()),
            },
            ClientCommand::ShutdownHost {
                meta: meta("spoofed", "shutdown-create"),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted
    );
    factory.create_release.notify_one();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), create)
            .await
            .expect("create finished")
            .expect("create task"),
        Err(HostError::ShuttingDown)
    ));
    assert!(host.session(&session_id).await.is_none());
    let registry = host.registry.lock().await;
    assert!(registry.sessions.is_empty());
    assert_eq!(registry.anonymous_openings, 0);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn descriptors_follow_only_durable_driver_model_and_shell_state() {
    let sink = Arc::new(BlockingDescriptorSink::default());
    let factory = Arc::new(StubFactory::with_event_sink(sink.clone()));
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 1,
            max_deduplicated_requests: 32,
        },
        factory,
        Arc::new(StubQueries::default()),
    )
    .expect("host");
    let session_id = SessionId("descriptor-state".to_owned());
    host.prepare_session(
        CreateSessionRequest {
            session_id: session_id.clone(),
            workspace: "workspace".to_owned(),
            model: None,
        },
        false,
    )
    .await
    .expect("prepared session");
    let driver = BoundClient {
        client_id: ClientId("driver".to_owned()),
    };
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::TakeDriver {
                meta: meta("spoofed", "take-driver"),
                session_id: session_id.clone(),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted
    );
    let session = host.session(&session_id).await.expect("ready session");
    assert_eq!(
        session.descriptor().driver_client_id,
        Some(driver.client_id.clone())
    );

    sink.block(BLOCK_MODEL);
    let switch = tokio::spawn({
        let host = host.clone();
        let driver = driver.clone();
        let session_id = session_id.clone();
        async move {
            host.dispatch(
                driver,
                ClientCommand::SwitchModel {
                    meta: meta("spoofed", "switch-model"),
                    session_id,
                    model: ModelAlias("big".to_owned()),
                    provider: None,
                },
            )
            .await
            .outcome
        }
    });
    tokio::time::timeout(Duration::from_secs(1), sink.append_started.notified())
        .await
        .expect("model append blocked");
    assert_eq!(session.descriptor().model, ModelAlias("fast".to_owned()));
    assert!(
        !switch.is_finished(),
        "model-switch acceptance must wait for the durable event and project preference"
    );
    sink.release();
    assert_eq!(switch.await.expect("switch task"), CommandOutcome::Accepted);
    tokio::time::timeout(Duration::from_secs(1), async {
        while session.descriptor().model != ModelAlias("big".to_owned()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("durable model projected");

    let tail = session.handle().last_sequence().await.expect("tail");
    let mut shell_events = session
        .handle()
        .subscribe_client(ClientId("shell-test".to_owned()), tail)
        .expect("subscription");
    sink.block(BLOCK_SHELL_ACTIVE);
    let start = tokio::spawn({
        let host = host.clone();
        let driver = driver.clone();
        let session_id = session_id.clone();
        async move {
            host.dispatch(
                driver,
                ClientCommand::UserShellStarted {
                    meta: meta("spoofed", "shell-start-one"),
                    session_id,
                    command: "python --version".to_owned(),
                },
            )
            .await
            .outcome
        }
    });
    tokio::time::timeout(Duration::from_secs(1), sink.append_started.notified())
        .await
        .expect("shell-active append blocked");
    assert_eq!(start.await.expect("start task"), CommandOutcome::Accepted);
    assert!(!session.descriptor().shell_active);
    sink.release();
    let shell_id = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let EngineEvent::UserShellStateChanged {
                shell_id,
                active: true,
                ..
            } = shell_events.recv().await.expect("shell event")
            {
                break shell_id;
            }
        }
    })
    .await
    .expect("active shell event");
    tokio::time::timeout(Duration::from_secs(1), async {
        while !session.descriptor().shell_active {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("durable active shell projected");

    sink.block(BLOCK_SHELL_INACTIVE);
    let end = tokio::spawn({
        let host = host.clone();
        let driver = driver.clone();
        let session_id = session_id.clone();
        let shell_id = shell_id.clone();
        async move {
            host.dispatch(
                driver,
                ClientCommand::UserShellEnded {
                    meta: meta("spoofed", "shell-end-one"),
                    session_id,
                    shell_id,
                    status: 0,
                    captured_output: None,
                },
            )
            .await
            .outcome
        }
    });
    tokio::time::timeout(Duration::from_secs(1), sink.append_started.notified())
        .await
        .expect("shell-inactive append blocked");
    assert_eq!(end.await.expect("end task"), CommandOutcome::Accepted);
    assert!(session.descriptor().shell_active);
    sink.release();
    tokio::time::timeout(Duration::from_secs(1), async {
        while session.descriptor().shell_active {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("durable inactive shell projected");

    let tail = session.handle().last_sequence().await.expect("tail");
    let mut broker_events = session
        .handle()
        .subscribe_client(ClientId("broker-test".to_owned()), tail)
        .expect("subscription");
    sink.block(0);
    assert_eq!(
        host.dispatch(
            driver,
            ClientCommand::UserShellStarted {
                meta: meta("spoofed", "shell-start-two"),
                session_id: session_id.clone(),
                command: "python --version".to_owned(),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted
    );
    let broker_shell_id = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let EngineEvent::UserShellStateChanged {
                shell_id,
                active: true,
                ..
            } = broker_events.recv().await.expect("broker shell event")
            {
                break shell_id;
            }
        }
    })
    .await
    .expect("broker active shell event");
    tokio::time::timeout(Duration::from_secs(1), async {
        while !session.descriptor().shell_active {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second active shell projected");

    sink.block(BLOCK_SHELL_INACTIVE);
    let completion = tokio::spawn({
        let host = host.clone();
        let session_id = session_id.clone();
        async move {
            host.complete_user_shell(&session_id, broker_shell_id, 0, None)
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(1), sink.append_started.notified())
        .await
        .expect("trusted completion append blocked");
    assert!(session.descriptor().shell_active);
    assert!(!completion.is_finished());
    sink.release();
    completion
        .await
        .expect("completion task")
        .expect("trusted completion");
    assert!(!session.descriptor().shell_active);
}
