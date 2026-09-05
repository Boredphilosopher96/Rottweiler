use super::*;

struct SettlementModel {
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
    fail: bool,
}

impl SettlementModel {
    fn new(fail: bool) -> Arc<Self> {
        Arc::new(Self {
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
            fail,
        })
    }
}

#[async_trait]
impl ModelDriver for SettlementModel {
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        self.entered.add_permits(1);
        self.release.acquire().await.expect("release").forget();
        if self.fail {
            Err(AgentLoopError::EffectsUnsettled(
                "injected effect failure".to_owned(),
            ))
        } else {
            Ok(())
        }
    }

    fn stream(
        &self,
        _alias: &str,
        _request: ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        Ok(Box::pin(stream::empty()))
    }

    fn has_model_alias(&self, _alias: &str) -> bool {
        true
    }
}

fn host_with_settlement(model: Arc<SettlementModel>) -> (EngineHost, Arc<StubFactory>) {
    let factory = Arc::new(StubFactory::with_model(model));
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 2,
            max_deduplicated_requests: 32,
        },
        factory.clone(),
        Arc::new(StubQueries::default()),
    )
    .expect("host");
    (host, factory)
}

async fn entered(model: &SettlementModel, count: u32) {
    tokio::time::timeout(Duration::from_secs(2), model.entered.acquire_many(count))
        .await
        .expect("every close attempted")
        .expect("entered")
        .forget();
}

#[tokio::test]
async fn shutdown_retains_ready_sessions_and_survives_its_callers_drop() {
    let model = SettlementModel::new(false);
    let (host, factory) = host_with_settlement(model.clone());
    let id = SessionId("closing-ready".to_owned());
    host.resume_session(&id).await.expect("session");
    let caller = tokio::spawn({
        let host = host.clone();
        async move { host.shutdown_sessions().await }
    });
    entered(&model, 1).await;
    assert!(!caller.is_finished());
    assert_eq!(
        factory.shutdowns.load(Ordering::Acquire),
        0,
        "shared services remain available during session cleanup"
    );
    assert!(
        host.session(&id).await.is_some(),
        "retain actual session before proof"
    );
    caller.abort();
    assert!(caller.await.expect_err("cancelled caller").is_cancelled());
    model.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), host.shutdown_sessions())
        .await
        .expect("owned shutdown completes")
        .expect("closure proof");
    assert!(host.session(&id).await.is_none());
    assert_eq!(factory.shutdowns.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn shutdown_attempts_every_session_and_never_accepts_failed_proof() {
    let model = SettlementModel::new(true);
    let (host, factory) = host_with_settlement(model.clone());
    for id in ["failed-one", "failed-two"] {
        host.resume_session(&SessionId(id.to_owned()))
            .await
            .expect("session");
    }
    let caller = tokio::spawn({
        let host = host.clone();
        async move {
            host.dispatch(
                BoundClient {
                    client_id: ClientId("closer".to_owned()),
                },
                ClientCommand::ShutdownHost {
                    meta: meta("spoofed", "failed-shutdown"),
                },
            )
            .await
        }
    });
    entered(&model, 2).await;
    model.release.add_permits(2);
    let reply = tokio::time::timeout(Duration::from_secs(2), caller)
        .await
        .expect("failure returns")
        .expect("caller");
    assert!(matches!(reply.outcome, CommandOutcome::Rejected { .. }));
    {
        let dedupe = host.dedupe.lock().expect("request ledger");
        let DedupeState::Complete { dispatch, .. } = dedupe
            .entries
            .get(&(
                ClientId("closer".to_owned()),
                RequestId("failed-shutdown".to_owned()),
            ))
            .expect("completed shutdown")
        else {
            panic!("completed shutdown");
        };
        assert!(
            !dispatch
                .events
                .iter()
                .any(|event| matches!(event, EngineEvent::HostShutdown { .. }))
        );
    }
    assert_eq!(
        host.registry.lock().await.sessions.len(),
        2,
        "failed owners stay charged"
    );
    assert!(host.shutdown_sessions().await.is_err(), "failure is sticky");
    assert_eq!(
        factory.shutdowns.load(Ordering::Acquire),
        0,
        "failed actors retain their shared services"
    );
}

fn fork_request(child: SessionId) -> ForkSessionRequest {
    ForkSessionRequest {
        operation_key: ForkOperationKey {
            operation_id: "fork-owner".to_owned(),
            client_id: ClientId("driver".to_owned()),
            request_id: RequestId("fork".to_owned()),
            payload_hash: "payload".to_owned(),
        },
        parent: SessionDescriptor {
            session_id: SessionId("parent".to_owned()),
            title: "parent".to_owned(),
            workspace_name: "workspace".to_owned(),
            model: ModelAlias("fast".to_owned()),
            driver_client_id: None,
            shell_active: false,
        },
        child_session_id: child,
        at_turn: TurnId("turn".to_owned()),
        through_sequence: None,
        include_idle_tail: false,
        driver_client_id: ClientId("driver".to_owned()),
    }
}

#[tokio::test]
async fn dropped_create_resume_and_fork_callers_keep_reserved_factory_work_owned() {
    for kind in ["create", "resume", "fork"] {
        let (host, factory) = host(1);
        let id = SessionId(format!("abandoned-{kind}"));
        let (blocked, started, release) = match kind {
            "create" => (
                &factory.block_create,
                &factory.create_started,
                &factory.create_release,
            ),
            "resume" => (
                &factory.block_resume,
                &factory.resume_started,
                &factory.resume_release,
            ),
            _ => (
                &factory.block_fork,
                &factory.fork_started,
                &factory.fork_release,
            ),
        };
        blocked.store(true, Ordering::Release);
        let caller = tokio::spawn({
            let host = host.clone();
            let id = id.clone();
            async move {
                match kind {
                    "create" => {
                        host.create_session(CreateSessionRequest {
                            session_id: id,
                            workspace: "workspace".to_owned(),
                            model: None,
                        })
                        .await
                    }
                    "resume" => host.resume_session(&id).await,
                    _ => host.fork_session(fork_request(id)).await,
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("factory entered");
        caller.abort();
        assert!(caller.await.expect_err("caller cancelled").is_cancelled());
        assert!(matches!(
            host.resume_session(&SessionId("other".to_owned())).await,
            Err(HostError::SessionCapacity)
        ));
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while host.session(&id).await.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned factory publishes session");
        host.shutdown_sessions().await.expect("session closes");
    }
}

#[tokio::test]
async fn late_fork_closes_before_shutdown_acknowledgement() {
    let model = SettlementModel::new(false);
    let (host, factory) = host_with_settlement(model.clone());
    factory.block_fork.store(true, Ordering::Release);
    let id = SessionId("late-fork".to_owned());
    let fork = tokio::spawn({
        let host = host.clone();
        let id = id.clone();
        async move { host.fork_session(fork_request(id)).await }
    });
    tokio::time::timeout(Duration::from_secs(2), factory.fork_started.notified())
        .await
        .expect("fork started");
    let shutdown = tokio::spawn({
        let host = host.clone();
        async move { host.shutdown_sessions().await }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !host.shutting_down.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown started");
    factory.fork_release.notify_one();
    entered(&model, 1).await;
    assert!(!shutdown.is_finished());
    assert!(!fork.is_finished());
    assert!(host.session(&id).await.is_none());
    model.release.add_permits(1);
    assert!(matches!(
        fork.await.expect("fork"),
        Err(HostError::ShuttingDown)
    ));
    shutdown
        .await
        .expect("shutdown task")
        .expect("shutdown proof");
    assert!(host.registry.lock().await.sessions.is_empty());
}

#[tokio::test]
async fn invalid_factory_identity_closes_the_returned_actor_before_releasing_capacity() {
    let model = SettlementModel::new(false);
    let factory = Arc::new(StubFactory {
        corrupt_identity: true,
        ..StubFactory::with_model(model.clone())
    });
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 1,
            max_deduplicated_requests: 32,
        },
        factory,
        Arc::new(StubQueries::default()),
    )
    .expect("host");
    let open = tokio::spawn({
        let host = host.clone();
        async move { host.resume_session(&SessionId("expected".to_owned())).await }
    });
    entered(&model, 1).await;
    assert!(!open.is_finished());
    assert!(matches!(
        host.resume_session(&SessionId("other".to_owned())).await,
        Err(HostError::SessionCapacity)
    ));
    model.release.add_permits(1);
    assert!(matches!(
        open.await.expect("open task"),
        Err(HostError::SessionIdentityMismatch)
    ));
    assert!(host.registry.lock().await.sessions.is_empty());
    host.shutdown_sessions()
        .await
        .expect("settled invalid session");
}

#[tokio::test]
async fn factory_panic_retains_factory_owner_and_fails_shutdown_without_hanging() {
    let factory = Arc::new(StubFactory {
        panic_resume: true,
        ..StubFactory::new()
    });
    let retained = Arc::downgrade(&factory);
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 1,
            max_deduplicated_requests: 32,
        },
        factory.clone(),
        Arc::new(StubQueries::default()),
    )
    .expect("host");
    assert!(matches!(
        host.resume_session(&SessionId("panicked".to_owned())).await,
        Err(HostError::Persistence(_))
    ));
    assert!(
        tokio::time::timeout(Duration::from_secs(2), host.shutdown_sessions())
            .await
            .expect("bounded failure")
            .is_err()
    );
    assert_eq!(factory.shutdowns.load(Ordering::Acquire), 0);
    assert_eq!(
        host.registry.lock().await.sessions.len(),
        1,
        "unproven opening remains charged"
    );
    drop(host);
    drop(factory);
    assert!(
        retained.upgrade().is_some(),
        "quarantine owns the actual factory beyond callers"
    );
}
