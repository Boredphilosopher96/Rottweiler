use super::*;

#[derive(Default)]
pub(super) struct ExportGate {
    pub(super) entered: Notify,
    pub(super) release: Notify,
}

fn bound() -> BoundClient {
    BoundClient {
        client_id: ClientId("control-owner".into()),
    }
}

#[tokio::test]
async fn duplicate_completion_survives_cache_eviction_before_delivery() {
    let factory = Arc::new(StubFactory::new());
    factory.block_create.store(true, Ordering::Release);
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 2,
            max_deduplicated_requests: 1,
        },
        factory.clone(),
        Arc::new(StubQueries::default()),
    )
    .expect("host");
    let _events = host
        .subscribe(bound(), None, None)
        .await
        .expect("host events");
    let channel = host
        .client_events
        .lock()
        .expect("clients")
        .clients
        .get(&bound().client_id)
        .expect("channel")
        .clone();
    let delivery = channel.delivery.lock().await;
    let command = ClientCommand::CreateSession {
        meta: meta("spoof", "create-once"),
        cwd: "workspace".into(),
        model: None,
    };
    let first = tokio::spawn({
        let host = host.clone();
        let command = command.clone();
        async move { host.dispatch(bound(), command).await.outcome }
    });
    factory.create_started.notified().await;
    let duplicate = tokio::spawn({
        let host = host.clone();
        async move { host.dispatch(bound(), command).await.outcome }
    });
    let key = (bound().client_id, RequestId("create-once".into()));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(host.dedupe.lock().expect("ledger").entries.get(&key),
                Some(DedupeState::Running { completion, .. }) if completion.receiver_count() == 2)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("duplicate awaits same completion");
    factory.create_release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                host.dedupe.lock().expect("ledger").entries.get(&key),
                Some(DedupeState::Complete { .. })
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("result prepared before delivery");
    host.dispatch(
        bound(),
        ClientCommand::ListModels {
            meta: meta("spoof", "evict-receipt"),
            session_id: None,
            refresh: false,
        },
    )
    .await;
    assert!(
        !host
            .dedupe
            .lock()
            .expect("ledger")
            .entries
            .contains_key(&key)
    );
    drop(delivery);
    for caller in [first, duplicate] {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), caller)
                .await
                .expect("completion remains reachable")
                .expect("caller"),
            CommandOutcome::Accepted {}
        );
    }
    assert_eq!(
        factory.next.load(Ordering::Acquire),
        2,
        "allocated exactly one child identity"
    );
    host.shutdown_sessions().await.expect("closed");
}

#[tokio::test]
async fn shutdown_waits_for_accepted_export_after_its_caller_disappears() {
    let factory = Arc::new(StubFactory::new());
    let gate = Arc::new(ExportGate::default());
    let queries = Arc::new(StubQueries {
        export_gate: Some(gate.clone()),
        ..StubQueries::default()
    });
    let host = EngineHost::new(
        EngineHostConfig::default(),
        factory.clone(),
        queries.clone(),
    )
    .expect("host");
    let session = SessionId("export-owned".into());
    assert_eq!(
        host.dispatch(
            bound(),
            ClientCommand::ResumeSession {
                meta: meta("spoof", "attach"),
                session_id: session.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            }
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let caller = tokio::spawn({
        let host = host.clone();
        async move {
            host.dispatch(
                bound(),
                ClientCommand::ExportSession {
                    meta: meta("spoof", "export"),
                    session_id: session,
                    format: TranscriptFormat::Markdown,
                    output_path: "/export.md".into(),
                    force: false,
                },
            )
            .await
        }
    });
    gate.entered.notified().await;
    caller.abort();
    assert!(caller.await.expect_err("caller cancelled").is_cancelled());
    let shutdown = tokio::spawn({
        let host = host.clone();
        async move {
            host.dispatch(
                bound(),
                ClientCommand::ShutdownHost {
                    meta: meta("spoof", "shutdown"),
                },
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !host.shutting_down.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("closure began");
    assert!(!shutdown.is_finished());
    assert_eq!(factory.shutdowns.load(Ordering::Acquire), 0);
    assert!(queries.exports.lock().expect("exports").is_empty());
    gate.release.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), shutdown)
            .await
            .expect("shutdown excludes its own barrier")
            .expect("shutdown task")
            .outcome,
        CommandOutcome::Accepted {}
    );
    assert_eq!(queries.exports.lock().expect("exports").len(), 1);
    assert_eq!(factory.shutdowns.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn panicking_control_returns_a_failure_and_poisoned_host_cannot_claim_closure() {
    let factory = Arc::new(StubFactory {
        panic_allocate: true,
        ..StubFactory::new()
    });
    let host = EngineHost::new(
        EngineHostConfig::default(),
        factory.clone(),
        Arc::new(StubQueries::default()),
    )
    .expect("host");
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        host.dispatch(
            bound(),
            ClientCommand::CreateSession {
                meta: meta("spoof", "panic"),
                cwd: "workspace".into(),
                model: None,
            },
        ),
    )
    .await
    .expect("panic cannot strand request completion");
    assert!(
        matches!(result.outcome, CommandOutcome::Rejected { error } if error.code == "control_panicked")
    );
    assert!(host.shutting_down.load(Ordering::Acquire));
    assert!(host.shutdown_sessions().await.is_err());
    assert_eq!(factory.shutdowns.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn panic_while_constructing_failure_still_settles_duplicate_completion() {
    struct PanickingClock;
    impl EventClock for PanickingClock {
        fn emitted_at(&self) -> String {
            panic!("injected clock failure")
        }
    }
    let host = EngineHost::new(
        EngineHostConfig::default(),
        Arc::new(StubFactory {
            panic_allocate: true,
            ..StubFactory::new()
        }),
        Arc::new(StubQueries::default()),
    )
    .expect("host")
    .with_clock(Arc::new(PanickingClock));
    let command = ClientCommand::CreateSession {
        meta: meta("spoof", "panic-completion"),
        cwd: "workspace".into(),
        model: None,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        host.dispatch(bound(), command.clone()),
    )
    .await
    .expect("unwind releases original waiter");
    assert!(
        matches!(result.outcome, CommandOutcome::Rejected { error } if error.code == "control_completion_failed")
    );
    let duplicate = tokio::time::timeout(Duration::from_secs(2), host.dispatch(bound(), command))
        .await
        .expect("unwind releases duplicate waiter");
    assert!(matches!(duplicate.outcome, CommandOutcome::Rejected { .. }));
    assert!(host.control_owner.settle().await.is_err());
}

#[tokio::test]
async fn command_capacity_is_rejected_before_factory_effects() {
    let factory = Arc::new(StubFactory::new());
    let host = EngineHost::new(
        EngineHostConfig::default(),
        factory.clone(),
        Arc::new(StubQueries::default()),
    )
    .expect("host");
    let mut cwd = String::with_capacity(16 * 1024 * 1024);
    cwd.push_str("workspace");
    let result = host
        .dispatch(
            bound(),
            ClientCommand::CreateSession {
                meta: meta("spoof", "oversized-capacity"),
                cwd,
                model: None,
            },
        )
        .await;
    assert!(
        matches!(result.outcome, CommandOutcome::Rejected { error } if error.code == "control_busy")
    );
    assert_eq!(
        factory.next.load(Ordering::Acquire),
        1,
        "no session identity allocated"
    );
    host.shutdown_sessions().await.expect("closed");
}
