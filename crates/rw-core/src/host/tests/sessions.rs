use super::*;

#[tokio::test]
async fn concurrent_resume_opens_one_actor_and_capacity_is_atomic() {
    let (host, factory) = host(1);
    let session = SessionId("shared".to_owned());
    let left = host.dispatch(
        BoundClient {
            client_id: ClientId("left".to_owned()),
        },
        ClientCommand::ResumeSession {
            meta: meta("spoofed", "left-resume"),
            session_id: session.clone(),
            last_seen_sequence: None,
            role: ClientRole::Observer,
        },
    );
    let right = host.dispatch(
        BoundClient {
            client_id: ClientId("right".to_owned()),
        },
        ClientCommand::ResumeSession {
            meta: meta("spoofed", "right-resume"),
            session_id: session.clone(),
            last_seen_sequence: None,
            role: ClientRole::Observer,
        },
    );
    let (left, right) = tokio::join!(left, right);
    assert_eq!(left.outcome, CommandOutcome::Accepted {});
    assert_eq!(right.outcome, CommandOutcome::Accepted {});
    assert_eq!(factory.resumes.load(Ordering::Relaxed), 1);

    let rejected = host
        .dispatch(
            BoundClient {
                client_id: ClientId("third".to_owned()),
            },
            ClientCommand::ResumeSession {
                meta: meta("third", "capacity"),
                session_id: SessionId("second".to_owned()),
                last_seen_sequence: None,
                role: ClientRole::Observer,
            },
        )
        .await
        .outcome;
    assert!(matches!(
        rejected,
        CommandOutcome::Rejected { error } if error.code == "session_capacity"
    ));
}

#[tokio::test]
async fn fork_requires_idle_driver_and_returns_typed_child_descriptor() {
    let (host, _factory) = host(3);
    let parent = SessionId("fork-parent".to_owned());
    let driver = BoundClient {
        client_id: ClientId("fork-driver".to_owned()),
    };
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::ResumeSession {
                meta: meta("spoofed", "fork-resume"),
                session_id: parent.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let mut events = host
        .subscribe(driver.clone(), None, None)
        .await
        .expect("host events");
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::Fork {
                meta: meta("spoofed", "fork-now"),
                session_id: parent.clone(),
                at_turn: None,
                operation_id: "fork-now-operation".to_owned(),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let child = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let EngineEvent::SessionForked {
                parent_session_id,
                child,
                at_turn,
                ..
            } = events
                .recv()
                .await
                .expect("fork event")
                .expect("fork result")
            {
                break (parent_session_id, child, at_turn);
            }
        }
    })
    .await
    .expect("typed fork result");
    assert_eq!(child.0, parent);
    assert_eq!(child.1.session_id, SessionId("created-1".to_owned()));
    assert_eq!(child.1.driver_client_id, Some(driver.client_id.clone()));
    assert_eq!(child.2, TurnId("0".to_owned()));
    assert!(host.session(&parent).await.is_some());
    assert!(host.session(&child.1.session_id).await.is_some());

    let rejected = host
        .dispatch(
            driver,
            ClientCommand::Fork {
                meta: meta("spoofed", "fork-invalid-turn"),
                session_id: parent,
                at_turn: Some(TurnId("1".to_owned())),
                operation_id: "fork-invalid-operation".to_owned(),
            },
        )
        .await
        .outcome;
    assert!(matches!(
        rejected,
        CommandOutcome::Rejected { error }
            if error.message.contains("not a completed parent boundary")
    ));
}

#[tokio::test]
async fn concurrent_fork_waits_for_takeover_and_revalidates_actor_driver() {
    let (host, factory) = host(4);
    let parent = SessionId("fork-race-parent".to_owned());
    let first = BoundClient {
        client_id: ClientId("first-driver".to_owned()),
    };
    let second = BoundClient {
        client_id: ClientId("second-driver".to_owned()),
    };
    assert_eq!(
        host.dispatch(
            first.clone(),
            ClientCommand::ResumeSession {
                meta: meta("spoofed", "race-resume"),
                session_id: parent.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    factory.block_fork.store(true, Ordering::Release);
    let first_fork = tokio::spawn({
        let host = host.clone();
        let first = first.clone();
        let parent = parent.clone();
        async move {
            host.dispatch(
                first,
                ClientCommand::Fork {
                    meta: meta("spoofed", "first-fork"),
                    session_id: parent,
                    at_turn: None,
                    operation_id: "first-fork-operation".to_owned(),
                },
            )
            .await
            .outcome
        }
    });
    factory.fork_started.notified().await;
    let takeover = tokio::spawn({
        let host = host.clone();
        let second = second.clone();
        let parent = parent.clone();
        async move {
            host.dispatch(
                second,
                ClientCommand::TakeDriver {
                    meta: meta("spoofed", "takeover"),
                    session_id: parent,
                },
            )
            .await
            .outcome
        }
    });
    tokio::task::yield_now().await;
    assert!(!takeover.is_finished());
    factory.block_fork.store(false, Ordering::Release);
    factory.fork_release.notify_one();
    assert_eq!(
        first_fork.await.expect("first fork task"),
        CommandOutcome::Accepted {}
    );
    assert_eq!(
        takeover.await.expect("takeover task"),
        CommandOutcome::Accepted {}
    );
    assert_eq!(
        host.dispatch(
            second,
            ClientCommand::Fork {
                meta: meta("spoofed", "second-fork"),
                session_id: parent,
                at_turn: None,
                operation_id: "second-fork-operation".to_owned(),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    assert_eq!(
        factory
            .fork_turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        2
    );
}

#[tokio::test]
async fn empty_log_rejects_sequence_zero_cursor_and_null_cursor_completes_promptly() {
    let (host, _factory) = host(1);
    let session = SessionId("empty-log-cursor".to_owned());
    let bound = BoundClient {
        client_id: ClientId("empty-log-observer".to_owned()),
    };
    assert_eq!(
        host.dispatch(
            bound.clone(),
            ClientCommand::ResumeSession {
                meta: meta("spoofed", "resume-empty-log"),
                session_id: session.clone(),
                last_seen_sequence: None,
                role: ClientRole::Observer,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );

    let error = host
        .subscribe(bound.clone(), Some(session.clone()), Some(SequenceId(0)))
        .await
        .expect_err("sequence zero is synchronously rejected before subscription");
    assert!(matches!(error, HostError::ReplayCursorAhead));

    let mut valid = host
        .subscribe(bound, Some(session.clone()), None)
        .await
        .expect("null cursor subscription");
    let completed = tokio::time::timeout(Duration::from_millis(250), valid.recv())
        .await
        .expect("null cursor replay must complete")
        .expect("replay completion item")
        .expect("valid replay completion");
    assert!(matches!(
        &completed,
        EngineEvent::SessionReplayCompleted {
            session_id,
            through_sequence: None,
            ..
        } if session_id == &session
    ));
    let wire = serde_json::to_value(completed).expect("schema-safe replay completion");
    assert_eq!(wire["through_sequence"], serde_json::Value::Null);
}

#[tokio::test]
async fn failed_resume_removes_reservation_and_retry_succeeds() {
    let (host, factory) = host(1);
    factory.fail_resume_once.store(true, Ordering::Release);
    let command = |request: &str| ClientCommand::ResumeSession {
        meta: meta("client", request),
        session_id: SessionId("retry".to_owned()),
        last_seen_sequence: None,
        role: ClientRole::Observer,
    };
    assert!(matches!(
        host.dispatch(
            BoundClient {
                client_id: ClientId("client".to_owned())
            },
            command("first")
        )
        .await
        .outcome,
        CommandOutcome::Rejected { .. }
    ));
    assert_eq!(
        host.dispatch(
            BoundClient {
                client_id: ClientId("client".to_owned())
            },
            command("second")
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    assert_eq!(factory.resumes.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn bound_identity_and_request_deduplication_fail_closed() {
    let (host, _factory) = host(2);
    let bound = BoundClient {
        client_id: ClientId("bound".to_owned()),
    };
    let command = ClientCommand::CreateSession {
        meta: meta("spoofed-driver", "create-once"),
        cwd: "/workspace".to_owned(),
        model: None,
    };
    assert_eq!(
        host.dispatch(bound.clone(), command.clone()).await.outcome,
        CommandOutcome::Accepted {}
    );
    assert_eq!(
        host.dispatch(bound.clone(), command).await.outcome,
        CommandOutcome::Accepted {}
    );
    let sessions = host.factory.persisted_sessions().await.expect("sessions");
    assert!(sessions.is_empty());
    let registry = host.registry.lock().await;
    assert_eq!(registry.sessions.len(), 1);
    let descriptor = match registry.sessions.values().next() {
        Some(SessionSlot::Ready(session)) => session.descriptor(),
        Some(SessionSlot::Opening(_)) | None => panic!("ready session"),
    };
    assert_eq!(descriptor.driver_client_id, Some(bound.client_id.clone()));
    drop(registry);

    let conflict = host
        .dispatch(
            bound,
            ClientCommand::ListModels {
                meta: meta("spoofed-driver", "create-once"),
                session_id: None,
                refresh: false,
            },
        )
        .await
        .outcome;
    assert!(matches!(
        conflict,
        CommandOutcome::Rejected { error } if error.code == "request_id_conflict"
    ));
}
