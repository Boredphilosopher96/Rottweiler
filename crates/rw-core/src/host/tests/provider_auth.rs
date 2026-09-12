use super::*;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn provider_auth_completion_is_async_and_stale_cancel_keeps_real_attempt() {
    let fixture = AuthFixture::pending();
    let factory = Arc::new(StubFactory::new());
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 1,
            max_deduplicated_requests: 32,
        },
        factory,
        Arc::new(StubQueries {
            auth: Some(Arc::clone(&fixture)),
            ..StubQueries::default()
        }),
    )
    .expect("host");
    let session_id = SessionId("provider-auth".to_owned());
    host.prepare_session(
        CreateSessionRequest {
            session_id: session_id.clone(),
            workspace: "workspace".to_owned(),
            model: None,
        },
        false,
    )
    .await
    .expect("session");
    let driver = BoundClient {
        client_id: ClientId("auth-driver".to_owned()),
    };
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::TakeDriver {
                meta: meta("spoofed", "auth-take"),
                session_id: session_id.clone(),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let mut events = host
        .subscribe(driver.clone(), Some(session_id.clone()), None)
        .await
        .expect("events");
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::BeginProviderAuth {
                meta: meta("spoofed", "auth-begin"),
                session_id: session_id.clone(),
                provider: "github_copilot".to_owned(),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let attempt_id = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let EngineEvent::ProviderAuthStarted { attempt_id, .. } = decode_host_event(
                events
                    .recv()
                    .await
                    .expect("auth event")
                    .expect("auth result"),
            ) {
                break attempt_id;
            }
        }
    })
    .await
    .expect("auth prompt");
    assert_eq!(
        tokio::time::timeout(
            Duration::from_millis(100),
            host.dispatch(
                driver.clone(),
                ClientCommand::CompleteProviderAuth {
                    meta: meta("spoofed", "auth-complete"),
                    session_id: session_id.clone(),
                    provider: "github_copilot".to_owned(),
                    attempt_id: attempt_id.clone(),
                },
            ),
        )
        .await
        .expect("completion command must not await device polling")
        .outcome,
        CommandOutcome::Accepted {}
    );
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::CompleteProviderAuth {
                meta: meta("spoofed", "auth-complete-replayed"),
                session_id: session_id.clone(),
                provider: "github_copilot".to_owned(),
                attempt_id: attempt_id.clone(),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {},
        "a replayed durable auth prompt must join the in-flight completion"
    );
    assert!(matches!(
        host.dispatch(
            driver.clone(),
            ClientCommand::CancelProviderAuth {
                meta: meta("spoofed", "auth-stale-cancel"),
                session_id: session_id.clone(),
                provider: "github_copilot".to_owned(),
                attempt_id: ProviderAuthAttemptId("stale".to_owned()),
            },
        )
        .await
        .outcome,
        CommandOutcome::Rejected { .. }
    ));
    assert!(!fixture.cancelled.load(Ordering::Acquire));
    assert_eq!(
        host.dispatch(
            driver,
            ClientCommand::CancelProviderAuth {
                meta: meta("spoofed", "auth-cancel"),
                session_id,
                provider: "github_copilot".to_owned(),
                attempt_id,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    assert!(fixture.cancelled.load(Ordering::Acquire));
}

#[tokio::test]
async fn provider_auth_poll_is_cancelled_when_another_driver_takes_over() {
    let fixture = AuthFixture::pending();
    let factory = Arc::new(StubFactory::new());
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 1,
            max_deduplicated_requests: 32,
        },
        factory,
        Arc::new(StubQueries {
            auth: Some(Arc::clone(&fixture)),
            ..StubQueries::default()
        }),
    )
    .expect("host");
    let session_id = SessionId("provider-auth-takeover".to_owned());
    host.prepare_session(
        CreateSessionRequest {
            session_id: session_id.clone(),
            workspace: "workspace".to_owned(),
            model: None,
        },
        false,
    )
    .await
    .expect("session");
    let original = BoundClient {
        client_id: ClientId("original-driver".to_owned()),
    };
    for command in [
        ClientCommand::TakeDriver {
            meta: meta("spoofed", "take-original"),
            session_id: session_id.clone(),
        },
        ClientCommand::BeginProviderAuth {
            meta: meta("spoofed", "begin-original"),
            session_id: session_id.clone(),
            provider: "github_copilot".to_owned(),
        },
    ] {
        assert_eq!(
            host.dispatch(original.clone(), command).await.outcome,
            CommandOutcome::Accepted {}
        );
    }
    let attempt_id = {
        let entries = host
            .provider_auth
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending_provider_auth_id(entries.values().next().expect("pending auth")).clone()
    };
    assert_eq!(
        host.dispatch(
            original,
            ClientCommand::CompleteProviderAuth {
                meta: meta("spoofed", "complete-original"),
                session_id: session_id.clone(),
                provider: "github_copilot".to_owned(),
                attempt_id,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    assert_eq!(
        host.dispatch(
            BoundClient {
                client_id: ClientId("replacement-driver".to_owned()),
            },
            ClientCommand::TakeDriver {
                meta: meta("spoofed", "take-replacement"),
                session_id,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while !fixture.cancelled.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("takeover cancellation");
}

#[tokio::test]
async fn cancelled_begin_future_drops_its_opening_reservation() {
    let pending = Arc::new(PendingProviderAuths::default());
    let owner = ProviderAuthOwner {
        client_id: ClientId("cancelled-begin".to_owned()),
        session_id: SessionId("cancelled-begin-session".to_owned()),
        provider: "github_copilot".to_owned(),
    };
    let attempt_id = ProviderAuthAttemptId("cancelled-opening".to_owned());
    pending
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            owner.clone(),
            PendingProviderAuth::Opening {
                attempt_id: attempt_id.clone(),
            },
        );
    let task = tokio::spawn({
        let pending = Arc::clone(&pending);
        async move {
            let _guard = ProviderAuthOpeningGuard {
                pending,
                owner,
                attempt_id,
                armed: true,
            };
            std::future::pending::<()>().await;
        }
    });
    tokio::task::yield_now().await;
    task.abort();
    assert!(task.await.expect_err("cancelled begin").is_cancelled());
    assert!(
        pending
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
}

#[tokio::test]
async fn cancelled_api_key_request_cannot_interrupt_store_or_overtake_lifecycle() {
    let mutation = BlockingCredentialMutation::new();
    let store = {
        let mutation = Arc::clone(&mutation);
        Arc::new(move |_provider: String, _api_key: ProviderApiKey| mutation.run())
            as Arc<ProviderApiKeyStore>
    };
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 1,
            max_deduplicated_requests: 32,
        },
        Arc::new(StubFactory::new()),
        Arc::new(StubQueries::default()),
    )
    .expect("host")
    .with_provider_api_key_store(store);
    let session_id = SessionId("api-key-cancellation".to_owned());
    host.prepare_session(
        CreateSessionRequest {
            session_id: session_id.clone(),
            workspace: "workspace".to_owned(),
            model: None,
        },
        false,
    )
    .await
    .expect("session");
    let original = BoundClient {
        client_id: ClientId("api-key-owner".to_owned()),
    };
    assert_eq!(
        host.dispatch(
            original.clone(),
            ClientCommand::TakeDriver {
                meta: meta("spoofed", "take-api-key-owner"),
                session_id: session_id.clone(),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let request = tokio::spawn({
        let host = host.clone();
        let session_id = session_id.clone();
        async move {
            host.submit_provider_api_key(
                original,
                &session_id,
                "openai",
                ProviderApiKey::from_terminal_input("request-only-secret".to_owned()).expect("key"),
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(1), mutation.started.notified())
        .await
        .expect("store started");
    request.abort();
    assert!(
        request
            .await
            .expect_err("request cancellation")
            .is_cancelled()
    );

    let mut takeover = tokio::spawn({
        let host = host.clone();
        let session_id = session_id.clone();
        async move {
            host.dispatch(
                BoundClient {
                    client_id: ClientId("api-key-replacement".to_owned()),
                },
                ClientCommand::TakeDriver {
                    meta: meta("spoofed", "take-api-key-replacement"),
                    session_id,
                },
            )
            .await
            .outcome
        }
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut takeover)
            .await
            .is_err(),
        "takeover must wait while the irreversible store owns lifecycle"
    );
    mutation.release();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), takeover)
            .await
            .expect("takeover completed")
            .expect("takeover task"),
        CommandOutcome::Accepted {}
    );
    assert!(mutation.persisted.load(Ordering::Acquire));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn oauth_and_api_key_mutations_share_one_global_store_boundary() {
    let oauth_mutation = BlockingCredentialMutation::new();
    let api_mutation = BlockingCredentialMutation::new();
    let fixture = AuthFixture::with_persistence(Arc::clone(&oauth_mutation));
    let store = {
        let api_mutation = Arc::clone(&api_mutation);
        Arc::new(move |_provider: String, _api_key: ProviderApiKey| api_mutation.run())
            as Arc<ProviderApiKeyStore>
    };
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 2,
            max_deduplicated_requests: 64,
        },
        Arc::new(StubFactory::new()),
        Arc::new(StubQueries {
            auth: Some(Arc::clone(&fixture)),
            ..StubQueries::default()
        }),
    )
    .expect("host")
    .with_provider_api_key_store(store);
    let auth_session = SessionId("oauth-mutation".to_owned());
    let api_session = SessionId("api-mutation".to_owned());
    let driver = BoundClient {
        client_id: ClientId("mutation-driver".to_owned()),
    };
    for session_id in [&auth_session, &api_session] {
        host.prepare_session(
            CreateSessionRequest {
                session_id: session_id.clone(),
                workspace: format!("workspace-{}", session_id.0),
                model: None,
            },
            false,
        )
        .await
        .expect("session");
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::TakeDriver {
                    meta: meta("spoofed", &format!("take-{}", session_id.0)),
                    session_id: session_id.clone(),
                },
            )
            .await
            .outcome,
            CommandOutcome::Accepted {}
        );
    }
    let mut auth_events = host
        .subscribe(driver.clone(), Some(auth_session.clone()), None)
        .await
        .expect("auth events");
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::BeginProviderAuth {
                meta: meta("spoofed", "begin-global-auth"),
                session_id: auth_session.clone(),
                provider: "github_copilot".to_owned(),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let attempt_id = {
        let entries = host
            .provider_auth
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending_provider_auth_id(entries.values().next().expect("pending auth")).clone()
    };
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::CompleteProviderAuth {
                meta: meta("spoofed", "complete-global-auth"),
                session_id: auth_session,
                provider: "github_copilot".to_owned(),
                attempt_id,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    fixture.completion.send_replace(true);
    tokio::time::timeout(Duration::from_secs(1), oauth_mutation.started.notified())
        .await
        .expect("OAuth persistence started");

    let api_request = tokio::spawn({
        let host = host.clone();
        async move {
            host.submit_provider_api_key(
                driver,
                &api_session,
                "openai",
                ProviderApiKey::from_terminal_input("another-request-secret".to_owned())
                    .expect("key"),
            )
            .await
        }
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), api_mutation.started.notified())
            .await
            .is_err(),
        "API-key persistence must wait for OAuth persistence globally"
    );
    oauth_mutation.release();
    tokio::time::timeout(Duration::from_secs(1), api_mutation.started.notified())
        .await
        .expect("API-key persistence started after OAuth release");
    api_mutation.release();
    let submission = tokio::time::timeout(Duration::from_secs(1), api_request)
        .await
        .expect("API-key request completed")
        .expect("API-key task")
        .expect("API-key submission");
    assert!(submission.stored);
    assert!(oauth_mutation.persisted.load(Ordering::Acquire));
    assert!(api_mutation.persisted.load(Ordering::Acquire));
    let (success, message, warnings) = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let EngineEvent::ProviderAuthFinished {
                success,
                message,
                warnings,
                ..
            } = decode_host_event(
                auth_events
                    .recv()
                    .await
                    .expect("auth event")
                    .expect("auth result"),
            ) {
                break (success, message, warnings);
            }
        }
    })
    .await
    .expect("auth completion event");
    assert!(success, "stored credentials complete authentication");
    assert_eq!(message, "provider authentication completed");
    assert!(warnings.is_empty());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn provider_catalog_refresh_failure_does_not_delay_or_relabel_login() {
    let fixture = AuthFixture::pending();
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 1,
            max_deduplicated_requests: 32,
        },
        Arc::new(StubFactory::with_model(Arc::new(ActivatableModel))),
        Arc::new(StubQueries {
            auth: Some(Arc::clone(&fixture)),
            fail_model_catalog: true,
            ..StubQueries::default()
        }),
    )
    .expect("host");
    let session_id = SessionId("provider-catalog-warning".to_owned());
    host.prepare_session(
        CreateSessionRequest {
            session_id: session_id.clone(),
            workspace: "workspace".to_owned(),
            model: None,
        },
        false,
    )
    .await
    .expect("session");
    let driver = BoundClient {
        client_id: ClientId("catalog-warning-driver".to_owned()),
    };
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::TakeDriver {
                meta: meta("spoofed", "catalog-warning-take"),
                session_id: session_id.clone(),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let mut events = host
        .subscribe(driver.clone(), Some(session_id.clone()), None)
        .await
        .expect("events");
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::BeginProviderAuth {
                meta: meta("spoofed", "catalog-warning-begin"),
                session_id: session_id.clone(),
                provider: "github_copilot".to_owned(),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let attempt_id = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let EngineEvent::ProviderAuthStarted { attempt_id, .. } = decode_host_event(
                events
                    .recv()
                    .await
                    .expect("auth event")
                    .expect("auth result"),
            ) {
                break attempt_id;
            }
        }
    })
    .await
    .expect("auth prompt");
    assert_eq!(
        host.dispatch(
            driver,
            ClientCommand::CompleteProviderAuth {
                meta: meta("spoofed", "catalog-warning-complete"),
                session_id,
                provider: "github_copilot".to_owned(),
                attempt_id,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    fixture.completion.send_replace(true);

    let (success, message, warnings) = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let EngineEvent::ProviderAuthFinished {
                success,
                message,
                warnings,
                ..
            } = decode_host_event(
                events
                    .recv()
                    .await
                    .expect("auth event")
                    .expect("auth result"),
            ) {
                break (success, message, warnings);
            }
        }
    })
    .await
    .expect("auth completion event");
    assert!(success, "catalog refresh does not redefine login success");
    assert_eq!(message, "provider authentication completed");
    assert!(warnings.is_empty());
    let (ready, message) = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let EngineEvent::ProviderActivationFinished {
                success, message, ..
            } = decode_host_event(
                events
                    .recv()
                    .await
                    .expect("readiness event")
                    .expect("readiness result"),
            ) {
                break (success, message);
            }
        }
    })
    .await
    .expect("provider readiness event");
    assert!(!ready);
    assert!(message.contains("catalog could not be refreshed"));
}

#[test]
fn provider_readiness_requires_the_target_catalog_row_to_be_usable() {
    let descriptor = |name: &str, reachable: bool, model_count: u32| rw_types::ProviderDescriptor {
        name: name.to_owned(),
        auth_kind: rw_types::ProviderAuthKind::DeviceFlow,
        next_action: rw_types::ProviderNextAction::SelectModels,
        configured: true,
        authenticated: true,
        reachable,
        model_count,
        status: None,
    };
    let catalog = ModelCatalogSnapshot {
        aliases: Vec::new(),
        models: Vec::new(),
        providers: vec![
            descriptor("openai_codex", true, 3),
            descriptor("github_copilot", false, 0),
        ],
        cached: false,
        truncated: false,
    };

    assert!(!provider_catalog_is_ready(&catalog, "github_copilot"));
    assert!(provider_catalog_is_ready(&catalog, "openai_codex"));
    let empty_target = ModelCatalogSnapshot {
        providers: vec![descriptor("github_copilot", true, 0)],
        ..catalog
    };
    assert!(!provider_catalog_is_ready(&empty_target, "github_copilot"));
}

#[test]
fn provider_auth_prompts_and_connection_events_are_bounded_and_non_durable() {
    let oversized = ProviderAuthAttempt::new(
        ProviderAuthChallenge::Oauth {
            authorization_url: format!("https://example.test/{}", "x".repeat(4_096)),
            redirect_uri: "http://127.0.0.1/callback".to_owned(),
        },
        Vec::new(),
        Box::pin(std::future::pending()),
        Arc::new(|| {}),
    );
    assert!(bounded_provider_auth_prompt(&oversized).is_err());

    let warning_flood = ProviderAuthAttempt::new(
        ProviderAuthChallenge::DeviceFlow {
            verification_uri: "https://example.test/device".to_owned(),
            user_code: "ABCD-1234".to_owned(),
        },
        vec!["warning".to_owned(); MAX_PROVIDER_AUTH_WARNINGS + 1],
        Box::pin(std::future::pending()),
        Arc::new(|| {}),
    );
    assert!(bounded_provider_auth_prompt(&warning_flood).is_err());

    let event = EngineEvent::ProviderAuthStarted {
        meta: CommandAckMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId("client".to_owned()),
            request_id: RequestId("request".to_owned()),
            emitted_at: "now".to_owned(),
        },
        session_id: SessionId("session".to_owned()),
        attempt_id: ProviderAuthAttemptId("attempt".to_owned()),
        provider: "github_copilot".to_owned(),
        challenge: ProviderAuthChallenge::DeviceFlow {
            verification_uri: "https://example.test/device".to_owned(),
            user_code: "ABCD-1234".to_owned(),
        },
        warnings: Vec::new(),
    };
    assert!(event.meta().is_none());
}
