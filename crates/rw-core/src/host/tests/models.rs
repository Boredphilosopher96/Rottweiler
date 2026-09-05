use super::*;

#[tokio::test]
async fn accepted_alias_and_concrete_model_switches_persist_in_dispatch_order() {
    let factory = Arc::new(StubFactory::new());
    let queries = Arc::new(StubQueries::default());
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 1,
            max_deduplicated_requests: 32,
        },
        factory,
        queries.clone(),
    )
    .expect("host");
    let session_id = SessionId("ordered-model-switches".to_owned());
    let driver = BoundClient {
        client_id: ClientId("driver".to_owned()),
    };
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::ResumeSession {
                meta: meta("spoofed", "resume"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted
    );
    for (request, model) in [("switch-a", "big"), ("switch-b", "openai/b")] {
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::SwitchModel {
                    meta: meta("spoofed", request),
                    session_id: session_id.clone(),
                    model: ModelAlias(model.to_owned()),
                    provider: None,
                },
            )
            .await
            .outcome,
            CommandOutcome::Accepted
        );
        assert_eq!(
            queries
                .persisted_models
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .last()
                .map(String::as_str),
            Some(model),
            "dispatch must not complete before the committed preference is persisted"
        );
    }
    assert_eq!(
        *queries
            .persisted_models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec!["big".to_owned(), "openai/b".to_owned()]
    );
    assert_eq!(
        host.session(&session_id)
            .await
            .expect("session")
            .descriptor()
            .model,
        ModelAlias("openai/b".to_owned())
    );
}

#[tokio::test]
async fn model_switch_persistence_failure_is_visible_after_the_session_commit() {
    let factory = Arc::new(StubFactory::new());
    let queries = Arc::new(StubQueries {
        fail_model_persistence: true,
        ..StubQueries::default()
    });
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 1,
            max_deduplicated_requests: 32,
        },
        factory,
        queries.clone(),
    )
    .expect("host");
    let session_id = SessionId("failed-model-preference".to_owned());
    let driver = BoundClient {
        client_id: ClientId("driver".to_owned()),
    };
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::ResumeSession {
                meta: meta("spoofed", "resume"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted
    );
    assert!(matches!(
        host.dispatch(
            driver,
            ClientCommand::SwitchModel {
                meta: meta("spoofed", "switch"),
                session_id: session_id.clone(),
                model: ModelAlias("big".to_owned()),
                provider: None,
            },
        )
        .await.outcome,
        CommandOutcome::Rejected { error } if error.code == "host_query_failure"
    ));

    let session = host.session(&session_id).await.expect("session");
    assert_eq!(
        session
            .handle()
            .snapshot()
            .await
            .expect("snapshot")
            .model_alias,
        "big",
        "the journaled session switch remains correct when preference caching fails"
    );
    assert_eq!(session.descriptor().model, ModelAlias("big".to_owned()));
    assert!(
        queries
            .persisted_models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pending_switch_is_not_persisted_and_each_context_choice_persists_on_commit() {
    #[allow(clippy::too_many_lines)]
    async fn run(strategy: &str, model: Arc<dyn ModelDriver>) {
        let factory = Arc::new(StubFactory::with_model(model));
        let queries = Arc::new(StubQueries::default());
        let host = EngineHost::new(
            EngineHostConfig {
                max_sessions: 1,
                max_deduplicated_requests: 32,
            },
            factory,
            queries.clone(),
        )
        .expect("host");
        let session_id = SessionId(format!("model-context-{strategy}"));
        let driver = BoundClient {
            client_id: ClientId("driver".to_owned()),
        };
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::ResumeSession {
                    meta: meta("spoofed", "resume"),
                    session_id: session_id.clone(),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                },
            )
            .await
            .outcome,
            CommandOutcome::Accepted
        );
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::UserShellStarted {
                    meta: meta("spoofed", "shell-start"),
                    session_id: session_id.clone(),
                    command: "printf durable-context".to_owned(),
                },
            )
            .await
            .outcome,
            CommandOutcome::Accepted
        );
        let session = host.session(&session_id).await.expect("session");
        let shell_id = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(shell) = session
                    .handle()
                    .snapshot()
                    .await
                    .expect("shell snapshot")
                    .active_shell
                {
                    break shell.shell_id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shell became active");
        host.complete_user_shell(&session_id, shell_id, 0, Some("durable-context".to_owned()))
            .await
            .expect("shell context committed");

        let tail = session.handle().last_sequence().await.expect("tail");
        let mut events = session
            .handle()
            .subscribe_client(driver.client_id.clone(), tail)
            .expect("subscription");
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::SwitchModel {
                    meta: meta("spoofed", "switch"),
                    session_id: session_id.clone(),
                    model: ModelAlias("big".to_owned()),
                    provider: None,
                },
            )
            .await
            .outcome,
            CommandOutcome::Accepted
        );
        let question_id = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let EngineEvent::QuestionAsked { question_id, .. } =
                    events.recv().await.expect("question event")
                {
                    break question_id;
                }
            }
        })
        .await
        .expect("model context question");
        tokio::task::yield_now().await;
        assert!(
            queries
                .persisted_models
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "opening a context-transfer question must not persist the target"
        );
        assert_eq!(session.descriptor().model, ModelAlias("fast".to_owned()));

        assert_eq!(
            host.dispatch(
                driver,
                ClientCommand::AnswerQuestion {
                    meta: meta("spoofed", "answer"),
                    session_id,
                    question_id: question_id.clone(),
                    answers: vec![rw_types::Answer {
                        question_id,
                        values: vec![strategy.to_owned()],
                    }],
                },
            )
            .await
            .outcome,
            CommandOutcome::Accepted
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let persisted = queries
                    .persisted_models
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if persisted == ["big"] {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("committed model persisted");
        assert_eq!(
            *queries
                .persisted_models
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!["big".to_owned()]
        );
        assert_eq!(session.descriptor().model, ModelAlias("big".to_owned()));
    }

    run("pass_summary", Arc::new(SummaryModel)).await;
    run("pass_full_context", Arc::new(IdleModel)).await;
    run("start_without_context", Arc::new(IdleModel)).await;
}
