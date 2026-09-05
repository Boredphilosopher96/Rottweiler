use super::*;

#[tokio::test]
async fn active_read_identity_survives_ledger_eviction_pressure() {
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 1,
            max_deduplicated_requests: 1,
        },
        Arc::new(StubFactory::new()),
        Arc::new(StubQueries::default()),
    )
    .expect("host with bounded ledger");
    let bound = BoundClient {
        client_id: ClientId("retained-read".into()),
    };
    let retained = host
        .dispatch(
            bound.clone(),
            ClientCommand::ListSessions {
                meta: meta("spoofed", "protected"),
            },
        )
        .await;
    for index in 0..20 {
        let reply = host
            .dispatch(
                BoundClient {
                    client_id: ClientId("churn".into()),
                },
                ClientCommand::ListSessions {
                    meta: meta("spoofed", &format!("churn-{index}")),
                },
            )
            .await;
        assert_eq!(reply.outcome, CommandOutcome::Accepted {});
    }
    let conflict = host
        .dispatch(
            bound,
            ClientCommand::ListModels {
                meta: meta("spoofed", "protected"),
                session_id: None,
                refresh: false,
            },
        )
        .await;
    assert!(
        matches!(conflict.outcome, CommandOutcome::Rejected { error } if error.code == "request_id_conflict")
    );
    drop(retained);
    assert!(host.dedupe.lock().expect("ledger").entries.len() <= 1);
}

#[tokio::test]
async fn malformed_metadata_never_enters_the_request_ledger() {
    let (host, _) = host(1);
    for request in [String::new(), "x".repeat(257), "control\ncharacter".into()] {
        let reply = host
            .dispatch(
                BoundClient {
                    client_id: ClientId("bound".into()),
                },
                ClientCommand::ListSessions {
                    meta: meta("spoofed", &request),
                },
            )
            .await;
        assert!(
            matches!(reply.outcome, CommandOutcome::Rejected { error } if error.code == "command_metadata")
        );
    }
    let mut metadata = meta("spoofed", "unsupported-version");
    metadata.protocol_version += 1;
    let reply = host
        .dispatch(
            BoundClient {
                client_id: ClientId("bound".into()),
            },
            ClientCommand::ListSessions { meta: metadata },
        )
        .await;
    assert!(
        matches!(reply.outcome, CommandOutcome::Rejected { error } if error.code == "command_metadata")
    );
    assert!(host.dedupe.lock().expect("ledger").entries.is_empty());
}

#[tokio::test]
async fn reads_are_direct_fresh_and_keep_admission_until_body_clones_drop() {
    let (host, _) = host(2);
    let bound = BoundClient {
        client_id: ClientId("read-owner".into()),
    };
    let mut stream = host
        .subscribe(bound.clone(), None, None)
        .await
        .expect("stream");
    let query = ClientCommand::ListSessions {
        meta: meta("spoofed", "same-read"),
    };
    let first = host.dispatch(bound.clone(), query.clone()).await;
    let rw_types::CommandReply::Read {
        outcome: CommandOutcome::Accepted {},
        events,
    } = serde_json::from_slice(&first.bytes).expect("typed reply")
    else {
        panic!("read")
    };
    assert!(
        matches!(&events[..], [EngineEvent::SessionsListed { sessions, meta }] if sessions.is_empty() && meta.client_id == bound.client_id)
    );
    assert!(stream.try_recv().is_err(), "reads never enter SSE");
    let retained = first.bytes.clone();
    drop(first);
    let second = host.dispatch(bound.clone(), query.clone()).await;
    assert_eq!(second.outcome, CommandOutcome::Accepted {});
    let busy = host.dispatch(bound.clone(), query.clone()).await;
    assert!(
        matches!(busy.outcome, CommandOutcome::Rejected { error } if error.code == "read_busy")
    );
    let control = host
        .dispatch(
            bound.clone(),
            ClientCommand::ResumeSession {
                meta: meta("spoofed", "control-while-read-busy"),
                session_id: SessionId("read-created".into()),
                last_seen_sequence: None,
                role: ClientRole::Observer,
            },
        )
        .await;
    assert_eq!(
        control.outcome,
        CommandOutcome::Accepted {},
        "read pressure reserves control progress"
    );
    drop(retained);
    let fresh = host.dispatch(bound.clone(), query.clone()).await;
    let rw_types::CommandReply::Read { events, .. } =
        serde_json::from_slice(&fresh.bytes).expect("read")
    else {
        panic!("read")
    };
    assert!(
        matches!(&events[..], [EngineEvent::SessionsListed { sessions, .. }] if sessions.len() == 1),
        "same read identity re-queries without cached payload"
    );
    drop(second);
    drop(fresh);
    let conflict = host
        .dispatch(
            bound.clone(),
            ClientCommand::ListModels {
                meta: meta("spoofed", "same-read"),
                session_id: None,
                refresh: false,
            },
        )
        .await;
    assert!(
        matches!(conflict.outcome, CommandOutcome::Rejected { error } if error.code == "request_id_conflict")
    );
    let control_conflict = host
        .dispatch(
            bound,
            ClientCommand::ShutdownHost {
                meta: meta("spoofed", "same-read"),
            },
        )
        .await;
    assert!(
        matches!(control_conflict.outcome, CommandOutcome::Rejected { error } if error.code == "request_id_conflict")
    );
}

#[tokio::test]
async fn read_admission_is_global_and_independent_of_the_request_ledger() {
    let (host, _) = host(1);
    let mut retained = Vec::new();
    for index in 0..8 {
        let reply = host
            .dispatch(
                BoundClient {
                    client_id: ClientId(format!("reader-{index}")),
                },
                ClientCommand::ListSessions {
                    meta: meta("spoofed", "read"),
                },
            )
            .await;
        assert_eq!(reply.outcome, CommandOutcome::Accepted {});
        retained.push(reply.bytes);
    }
    let bound = BoundClient {
        client_id: ClientId("reader-nine".into()),
    };
    let query = ClientCommand::ListSessions {
        meta: meta("spoofed", "read"),
    };
    assert!(
        matches!(host.dispatch(bound.clone(), query.clone()).await.outcome,
        CommandOutcome::Rejected { error } if error.code == "read_busy")
    );
    retained.pop();
    assert_eq!(
        host.dispatch(bound, query).await.outcome,
        CommandOutcome::Accepted {}
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn client_event_fanout_cleans_up_each_subscription() {
    async fn expect_results(
        events: &mut mpsc::Receiver<Result<HostEvent, HostError>>,
        request_id: &str,
    ) {
        let acknowledgement = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("command acknowledgement")
            .expect("open subscription")
            .expect("host result");
        assert!(matches!(
            decode_host_event(acknowledgement),
            EngineEvent::CommandAcknowledged { meta, .. }
                if meta.request_id.0 == request_id
        ));
        let result = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("command result")
            .expect("open subscription")
            .expect("host result");
        assert!(matches!(
            decode_host_event(result),
            EngineEvent::SessionsListed { meta, .. }
                if meta.request_id.0 == request_id
        ));
    }

    let (host, _factory) = host(1);
    let client = BoundClient {
        client_id: ClientId("fanout-client".to_owned()),
    };
    let mut first = host
        .subscribe(client.clone(), None, None)
        .await
        .expect("first subscription");
    let mut second = host
        .subscribe(client.clone(), None, None)
        .await
        .expect("second subscription");

    assert_eq!(
        host.dispatch(
            client.clone(),
            ClientCommand::ResumeSession {
                session_id: SessionId("fanout-session".into()),
                last_seen_sequence: None,
                role: ClientRole::Observer,
                meta: meta("spoofed", "fanout-both"),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    expect_results(&mut first, "fanout-both").await;
    expect_results(&mut second, "fanout-both").await;

    drop(first);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let subscriber_count = host
                .client_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clients
                .get(&client.client_id)
                .map(|channel| {
                    channel
                        .subscribers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .senders
                        .len()
                });
            if subscriber_count == Some(1) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first subscription cleanup");

    assert_eq!(
        host.dispatch(
            client.clone(),
            ClientCommand::ResumeSession {
                session_id: SessionId("fanout-session".into()),
                last_seen_sequence: None,
                role: ClientRole::Observer,
                meta: meta("spoofed", "fanout-second"),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    expect_results(&mut second, "fanout-second").await;

    drop(second);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let registered = host
                .client_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clients
                .contains_key(&client.client_id);
            if !registered {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("final subscription cleanup");
}

#[tokio::test]
async fn connection_results_survive_slow_subscriber_backpressure() {
    const COMMANDS: usize = 600;

    let factory = Arc::new(StubFactory::new());
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 1,
            max_deduplicated_requests: COMMANDS + 1,
        },
        factory,
        Arc::new(StubQueries::default()),
    )
    .expect("host");
    let client = BoundClient {
        client_id: ClientId("slow-subscriber".to_owned()),
    };
    let mut events = host
        .subscribe(client.clone(), None, None)
        .await
        .expect("host subscription");
    let started = Instant::now();
    let in_flight = Arc::new(tokio::sync::Semaphore::new(4));
    let dispatches = (0..COMMANDS)
        .map(|index| {
            let host = host.clone();
            let client = client.clone();
            let in_flight = Arc::clone(&in_flight);
            tokio::spawn(async move {
                let _permit = in_flight.acquire().await.expect("fixture admission");
                host.dispatch(
                    client,
                    ClientCommand::ResumeSession {
                        session_id: SessionId("fanout-session".into()),
                        last_seen_sequence: None,
                        role: ClientRole::Observer,
                        meta: meta("spoofed", &format!("slow-{index}")),
                    },
                )
                .await
                .outcome
            })
        })
        .collect::<Vec<_>>();

    // Model a temporarily paused SSE consumer. The host may backpressure
    // these commands, but it must not discard either connection result.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut acknowledgements = HashSet::new();
    let mut session_lists = HashSet::new();
    let mut acknowledgement_events = 0;
    let mut session_list_events = 0;
    while acknowledgements.len() < COMMANDS || session_lists.len() < COMMANDS {
        let Ok(Some(Ok(event))) =
            tokio::time::timeout(Duration::from_millis(250), events.recv()).await
        else {
            break;
        };
        match decode_host_event(event) {
            EngineEvent::CommandAcknowledged { meta, .. }
                if meta.request_id.0.starts_with("slow-") =>
            {
                acknowledgement_events += 1;
                acknowledgements.insert(meta.request_id);
            }
            EngineEvent::SessionsListed { meta, .. } if meta.request_id.0.starts_with("slow-") => {
                session_list_events += 1;
                session_lists.insert(meta.request_id);
            }
            _ => {}
        }
    }

    for dispatch in dispatches {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), dispatch)
                .await
                .expect("dispatch completed after subscriber resumed")
                .expect("dispatch task"),
            CommandOutcome::Accepted {}
        );
    }
    eprintln!(
        "slow subscriber delivered {}/{} acknowledgements and {}/{} results in {:?}",
        acknowledgements.len(),
        COMMANDS,
        session_lists.len(),
        COMMANDS,
        started.elapsed()
    );
    assert_eq!(acknowledgements.len(), COMMANDS);
    assert_eq!(session_lists.len(), COMMANDS);
    assert_eq!(acknowledgement_events, COMMANDS);
    assert_eq!(session_list_events, COMMANDS);
}

#[tokio::test]
async fn stalled_subscription_does_not_block_active_sibling() {
    const COMMANDS: usize = 400;

    let factory = Arc::new(StubFactory::new());
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 1,
            max_deduplicated_requests: COMMANDS + 1,
        },
        factory,
        Arc::new(StubQueries::default()),
    )
    .expect("host");
    let client = BoundClient {
        client_id: ClientId("stalled-sibling".to_owned()),
    };
    let mut stalled = host
        .subscribe(client.clone(), None, None)
        .await
        .expect("stalled subscription");
    let mut active = host
        .subscribe(client.clone(), None, None)
        .await
        .expect("active subscription");

    let in_flight = Arc::new(tokio::sync::Semaphore::new(4));
    let dispatches = (0..COMMANDS)
        .map(|index| {
            let host = host.clone();
            let client = client.clone();
            let in_flight = Arc::clone(&in_flight);
            tokio::spawn(async move {
                let _permit = in_flight.acquire().await.expect("fixture admission");
                host.dispatch(
                    client,
                    ClientCommand::ResumeSession {
                        session_id: SessionId("fanout-session".into()),
                        last_seen_sequence: None,
                        role: ClientRole::Observer,
                        meta: meta("spoofed", &format!("sibling-{index}")),
                    },
                )
                .await
                .outcome
            })
        })
        .collect::<Vec<_>>();

    let started = Instant::now();
    let delivered = tokio::time::timeout(Duration::from_secs(2), async {
        let mut acknowledgements = HashSet::new();
        let mut session_lists = HashSet::new();
        while acknowledgements.len() < COMMANDS || session_lists.len() < COMMANDS {
            let event = active
                .recv()
                .await
                .expect("active subscription remains open")
                .expect("host result");
            match decode_host_event(event) {
                EngineEvent::CommandAcknowledged { meta, .. }
                    if meta.request_id.0.starts_with("sibling-") =>
                {
                    acknowledgements.insert(meta.request_id);
                }
                EngineEvent::SessionsListed { meta, .. }
                    if meta.request_id.0.starts_with("sibling-") =>
                {
                    session_lists.insert(meta.request_id);
                }
                _ => {}
            }
        }
        (acknowledgements.len(), session_lists.len())
    })
    .await;
    eprintln!(
        "active sibling completion: {delivered:?} in {:?}",
        started.elapsed()
    );

    let stalled_events = tokio::time::timeout(Duration::from_secs(2), async {
        let mut count = 0;
        while let Some(event) = stalled.recv().await {
            event.expect("queued stalled result");
            count += 1;
        }
        count
    })
    .await
    .expect("stalled subscription closes after its deadline");
    drop(active);
    for dispatch in dispatches {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), dispatch)
                .await
                .expect("dispatch completed after subscriptions closed")
                .expect("dispatch task"),
            CommandOutcome::Accepted {}
        );
    }
    assert_eq!(delivered, Ok((COMMANDS, COMMANDS)));
    assert!(stalled_events < COMMANDS * 2);
}
