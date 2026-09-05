#![cfg(test)]
use super::AbortOnDropTask;
use super::ActivatedHostedProvider;
use super::AgentLoopError;
use super::Arc;
use super::AtomicBool;
use super::AtomicUsize;
use super::ClientId;
use super::Config;
use super::ConfigLoader;
use super::DropProbe;
use super::Duration;
use super::ExistingRouteModel;
use super::FailModelChangedSink;
use super::FixedProviderCatalogSource;
use super::HostedProviderActivator;
use super::HostedRuntimeInitializer;
use super::ModelCatalogSource;
use super::ModelDriver;
use super::Mutex;
use super::Ordering;
use super::PermissionDecision;
use super::PermissionGate;
use super::PersistingHostedCatalogSource;
use super::PreparedHostedSelection;
use super::ProviderConfig;
use super::ProviderEvent;
use super::ProviderModelCatalogSource;
use super::QuickCatalogSource;
use super::QuickConnectedModel;
use super::RecomposableHostedModel;
use super::RejectingPrepareModel;
use super::ScopedCatalogSource;
use super::SessionActor;
use super::SessionActorConfig;
use super::SessionId;
use super::SystemEventClock;
use super::ThinkingLevel;
use super::ToolRegistry;
use super::UnavailableHostedModel;
use super::builtin_command_registry;
use super::builtin_hook_dispatcher;
use super::load_model_catalog_cache;
use super::prepare_provider_activation_config;
use super::quick_connect_request;
use super::quick_connect_stream;
use super::tempdir;
use super::test_provider_admission;
use super::test_provider_invocation;
use super::unavailable_hosted_model;
use super::unused_hosted_activator;
use futures_util::StreamExt;

#[tokio::test]
async fn provider_scoped_auth_refresh_survives_process_restart_cache_load() {
    let storage = tempdir().expect("storage");
    let cache_path = storage.path().join("model-catalog.json");
    let mut config = Config::default();
    config.providers.insert(
        "github_copilot".to_owned(),
        ProviderConfig {
            kind: "github_copilot".to_owned(),
            ..ProviderConfig::default()
        },
    );
    config.models.aliases.insert(
        "copilot".to_owned(),
        vec!["github_copilot/gpt-5-mini".to_owned()],
    );
    let initial = ProviderModelCatalogSource::placeholder(&config);
    let mut update = initial.clone();
    let provider = update
        .providers
        .iter_mut()
        .find(|provider| provider.name == "github_copilot")
        .expect("copilot provider row");
    provider.authenticated = true;
    provider.reachable = true;
    provider.model_count = 1;
    provider.status = None;

    let source = PersistingHostedCatalogSource {
        inner: Arc::new(FixedProviderCatalogSource(update)),
        cache_path: cache_path.clone(),
        initial,
    };
    source
        .discover_provider("github_copilot")
        .await
        .expect("provider refresh");

    let restarted = load_model_catalog_cache(&cache_path)
        .expect("cache read")
        .expect("durable catalog");
    let provider = restarted
        .providers
        .iter()
        .find(|provider| provider.name == "github_copilot")
        .expect("persisted copilot row");
    assert!(provider.authenticated);
    assert!(provider.reachable);
    assert_eq!(provider.model_count, 1);
}

#[tokio::test]
async fn overlapping_startup_task_is_aborted_when_its_owner_returns_early() {
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task_dropped = Arc::clone(&dropped);
    let (started, running) = tokio::sync::oneshot::channel();
    let task = AbortOnDropTask::new(tokio::spawn(async move {
        let _probe = DropProbe(task_dropped);
        let _ = started.send(());
        std::future::pending::<()>().await;
    }));
    running.await.expect("startup task should begin");
    drop(task);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !dropped.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("aborted startup task should drop its in-flight resources");
}

#[tokio::test]
async fn hosted_model_forwards_provider_scoped_catalog_discovery() {
    let source = Arc::new(ScopedCatalogSource {
        full_discoveries: AtomicUsize::new(0),
        provider_discoveries: Mutex::new(Vec::new()),
    });
    let catalog: Arc<dyn ModelCatalogSource> = source.clone();
    let model = RecomposableHostedModel::new_with_active_callback(
        unavailable_hosted_model("fast"),
        catalog,
        unused_hosted_activator(),
        None,
    );

    let discovered = model
        .discover_provider("github_copilot")
        .await
        .expect("provider-scoped discovery");

    assert!(discovered.truncated);
    assert_eq!(source.full_discoveries.load(Ordering::Acquire), 0);
    assert_eq!(
        *source
            .provider_discoveries
            .lock()
            .expect("provider discovery log"),
        vec!["github_copilot"]
    );
}

#[tokio::test]
async fn hosted_model_initialization_is_idle_until_first_prepare_and_streams_afterward() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let initialize_calls = Arc::clone(&calls);
    let initialize: Arc<HostedRuntimeInitializer> = Arc::new(move |alias| {
        assert_eq!(alias, "openai/live-model");
        initialize_calls.fetch_add(1, Ordering::AcqRel);
        Ok(ActivatedHostedProvider {
            replacement_model: Arc::new(QuickConnectedModel),
            pre_commit: None,
            post_commit: None,
        })
    });
    let model = RecomposableHostedModel::new_lazy(
        unavailable_hosted_model("openai/live-model"),
        "openai/live-model".to_owned(),
        Arc::new(QuickCatalogSource(false)),
        unused_hosted_activator(),
        initialize,
    );

    assert_eq!(calls.load(Ordering::Acquire), 0);
    assert!(model.has_provider_for_alias("openai/live-model", "openai"));
    assert!(!model.has_provider_for_alias("openai/live-model", "github_copilot"));
    assert!(
        quick_connect_stream(&model).is_err(),
        "idle construction must not silently initialize at stream time"
    );
    assert_eq!(calls.load(Ordering::Acquire), 0);

    model
        .prepare_model("openai/live-model")
        .await
        .expect("first model use should initialize the provider runtime");
    assert_eq!(calls.load(Ordering::Acquire), 1);
    let events = model
        .stream(
            "openai/live-model",
            quick_connect_request(),
            test_provider_invocation(),
        )
        .expect("the already-durable initial model should stream after preparation")
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|event| {
        matches!(event, Ok(ProviderEvent::TextDelta { text }) if text == "quick-connect-ok")
    }));
}

#[tokio::test]
async fn staged_first_load_cannot_be_mistaken_for_the_active_runtime() {
    let calls = Arc::new(AtomicUsize::new(0));
    let initialize_calls = Arc::clone(&calls);
    let initialize: Arc<HostedRuntimeInitializer> = Arc::new(move |alias| {
        assert_eq!(alias, "openai/live-model");
        initialize_calls.fetch_add(1, Ordering::AcqRel);
        Ok(ActivatedHostedProvider {
            replacement_model: Arc::new(QuickConnectedModel),
            pre_commit: None,
            post_commit: None,
        })
    });
    let model = RecomposableHostedModel::new_lazy(
        unavailable_hosted_model("openai/live-model"),
        "openai/live-model".to_owned(),
        Arc::new(QuickCatalogSource(false)),
        unused_hosted_activator(),
        initialize,
    );
    model.prepared.write().expect("prepared selections").insert(
        "openai/live-model".to_owned(),
        PreparedHostedSelection {
            provider: None,
            replacement_model: Arc::new(QuickConnectedModel),
            post_commit: None,
            completes_initialization: true,
        },
    );

    model
        .prepare_model("openai/live-model")
        .await
        .expect("a staged selection must not short-circuit initial activation");
    assert_eq!(calls.load(Ordering::Acquire), 1);
    let events = model
        .stream(
            "openai/live-model",
            quick_connect_request(),
            test_provider_invocation(),
        )
        .expect("the first turn must use the active connected runtime")
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|event| {
        matches!(event, Ok(ProviderEvent::TextDelta { text }) if text == "quick-connect-ok")
    }));
}

#[tokio::test]
async fn concurrent_first_prepares_initialize_once_and_failed_initialization_is_retryable() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let initialize_calls = Arc::clone(&calls);
    let initialize: Arc<HostedRuntimeInitializer> = Arc::new(move |alias| {
        assert_eq!(alias, "openai/live-model");
        let attempt = initialize_calls.fetch_add(1, Ordering::AcqRel);
        if attempt == 0 {
            return Err(AgentLoopError::Provider(
                "temporary credential access failure".to_owned(),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
        Ok(ActivatedHostedProvider {
            replacement_model: Arc::new(QuickConnectedModel),
            pre_commit: None,
            post_commit: None,
        })
    });
    let model = Arc::new(RecomposableHostedModel::new_lazy(
        unavailable_hosted_model("openai/live-model"),
        "openai/live-model".to_owned(),
        Arc::new(QuickCatalogSource(false)),
        unused_hosted_activator(),
        initialize,
    ));

    assert!(model.prepare_model("openai/live-model").await.is_err());
    assert_eq!(calls.load(Ordering::Acquire), 1);
    let first = model.prepare_model("openai/live-model");
    let second = model.prepare_model("openai/live-model");
    let (first, second) = tokio::join!(first, second);
    first.expect("retry should initialize");
    second.expect("concurrent waiter should observe initialized runtime");
    assert_eq!(calls.load(Ordering::Acquire), 2);
    for _ in 0..2 {
        let events = model
            .stream(
                "openai/live-model",
                quick_connect_request(),
                test_provider_invocation(),
            )
            .expect("every concurrent first-turn waiter must see the connected runtime")
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| {
            matches!(event, Ok(ProviderEvent::TextDelta { text }) if text == "quick-connect-ok")
        }));
    }
}

#[tokio::test]
async fn lazy_first_model_switch_does_not_activate_when_persistence_fails() {
    let initialize_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&initialize_calls);
    let post_commit_ran = Arc::new(AtomicBool::new(false));
    let callback_ran = Arc::clone(&post_commit_ran);
    let initialize: Arc<HostedRuntimeInitializer> = Arc::new(move |alias| {
        assert_eq!(alias, "openai/live-model");
        calls.fetch_add(1, Ordering::AcqRel);
        let callback_ran = Arc::clone(&callback_ran);
        Ok(ActivatedHostedProvider {
            replacement_model: Arc::new(QuickConnectedModel),
            pre_commit: None,
            post_commit: Some(Arc::new(move || {
                callback_ran.store(true, Ordering::Release);
            })),
        })
    });
    let model = Arc::new(RecomposableHostedModel::new_lazy(
        unavailable_hosted_model("local/base"),
        "local/base".to_owned(),
        Arc::new(QuickCatalogSource(false)),
        unused_hosted_activator(),
        initialize,
    ));
    let workspace = tempdir().expect("workspace");
    let session_id = SessionId("failed-lazy-model-switch".to_owned());
    let actor = SessionActor::spawn(SessionActorConfig {
        ui: std::sync::Arc::new(rw_core::ui::EmptyUiRegistry),
        ui_tool_source: std::sync::Arc::new(rw_core::ui::UnavailableUiToolSource),
        budget_session_id: session_id.clone(),
        session_id: session_id.clone(),
        workspace_root: workspace.path().to_path_buf(),
        additional_workspace_roots: Vec::new(),
        workspace_generation: 0,
        initial_session_context: Vec::new(),
        startup_notifications: Vec::new(),
        model_alias: "local/base".to_owned(),
        model: model.clone(),
        tools: Arc::new(ToolRegistry::new()),
        permissions: Arc::new(PermissionGate::new(PermissionDecision::Allow)),
        hooks: Arc::new(builtin_hook_dispatcher().expect("hooks")),
        commands: Arc::new(builtin_command_registry().expect("commands")),
        modes: Arc::new(rw_ext::ModeRegistry::builtins().expect("built-in modes")),
        event_sink: Arc::new(FailModelChangedSink {
            inner: Arc::new(rw_core::NoopSessionEventSink::default()),
        }),
        event_clock: Arc::new(SystemEventClock),
        provider_admission: test_provider_admission(),
        secret_redactor: Arc::new(rw_core::NoopSecretRedactor),
        checkpoints: Arc::new(rw_core::NoopMutationCheckpointCoordinator),
        folder_trust: Arc::new(rw_core::NoopFolderTrustController),
        workspace_roots: Arc::new(rw_core::NoopWorkspaceRootController),
        extension_development: Arc::new(rw_core::NoopSessionExtensionController),
        resources: Arc::new(rw_core::NoopSessionResources),
        recovered: rw_core::SessionRecoveredState {
            driver_client_id: Some(ClientId("driver".to_owned())),
            ..rw_core::SessionRecoveredState::default()
        },
        max_turns: 2,
        identical_tool_failure_limit: 2,
        max_output_tokens: 512,
        thinking: ThinkingLevel::Off,
        event_capacity: 32,
    })
    .expect("actor");

    assert_eq!(
        actor
            .dispatch(rw_core::ClientCommand::SwitchModel {
                meta: rw_core::CommandMeta {
                    protocol_version: rw_core::PROTOCOL_VERSION,
                    client_id: ClientId("driver".to_owned()),
                    request_id: rw_core::RequestId("switch".to_owned()),
                },
                session_id,
                model: rw_core::ModelAlias("openai/live-model".to_owned()),
                provider: Some("openai".to_owned()),
            })
            .await
            .expect("command acknowledgement"),
        rw_core::CommandOutcome::Accepted {}
    );
    assert_eq!(initialize_calls.load(Ordering::Acquire), 1);
    assert_eq!(
        actor.snapshot().await.expect("snapshot").model_alias,
        "local/base"
    );
    assert!(!post_commit_ran.load(Ordering::Acquire));
    assert!(
        quick_connect_stream(&model).is_err(),
        "the unavailable initial runtime must remain active"
    );
    model
        .prepare_model("openai/live-model")
        .await
        .expect("failed persistence must leave initialization retryable");
    assert_eq!(initialize_calls.load(Ordering::Acquire), 2);
    assert!(
        quick_connect_stream(&model).is_err(),
        "a retry must also remain staged until its durable commit"
    );
    model.discard_prepared_model("openai/live-model");
}

#[tokio::test]
async fn first_use_registers_redaction_before_model_preparation() {
    let callbacks = Arc::new(Mutex::new(Vec::new()));
    let initialize_callbacks = Arc::clone(&callbacks);
    let initialize: Arc<HostedRuntimeInitializer> = Arc::new(move |alias| {
        assert_eq!(alias, "openai/live-model");
        let pre_callbacks = Arc::clone(&initialize_callbacks);
        Ok(ActivatedHostedProvider {
            replacement_model: Arc::new(RejectingPrepareModel(Arc::clone(&initialize_callbacks))),
            pre_commit: Some(Arc::new(move || {
                pre_callbacks.lock().expect("callback log").push("redact");
            })),
            post_commit: None,
        })
    });
    let model = RecomposableHostedModel::new_lazy(
        unavailable_hosted_model("openai/live-model"),
        "openai/live-model".to_owned(),
        Arc::new(QuickCatalogSource(false)),
        unused_hosted_activator(),
        initialize,
    );

    let error = model
        .prepare_model("openai/live-model")
        .await
        .expect_err("preparation should fail");
    assert!(error.to_string().contains("sanitized preparation failure"));
    assert_eq!(
        *callbacks.lock().expect("callback log"),
        vec!["redact", "prepare"]
    );
}

#[tokio::test]
async fn sole_missing_credential_fallback_recomposes_after_quick_connect() {
    let root = tempdir().expect("config root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(workspace.join(".rottweiler")).expect("project config directory");
    std::fs::write(workspace.join(".rottweiler/config.toml"), b"").expect("empty project config");
    let loader = ConfigLoader::new(
        root.path().join("config.toml"),
        workspace.join(".rottweiler/config.toml"),
    )
    .with_project_trust(false);
    let fresh = loader.load().expect("fresh config").config;
    assert!(fresh.providers.is_empty());
    assert!(fresh.models.aliases.is_empty());
    let unavailable: Arc<dyn ModelDriver> = Arc::new(UnavailableHostedModel {
        alias: "openai/live-model".to_owned(),
        reason: "credential was not found".to_owned(),
        compaction: rw_core::CompactionConfig::default(),
        budget: rw_core::BudgetConfig::default(),
    });
    let credential_stored = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stored = Arc::clone(&credential_stored);
    let reload = loader.clone();
    let activate: Arc<HostedProviderActivator> = Arc::new(move |provider| {
        let config = reload
            .load()
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?
            .config;
        let config = prepare_provider_activation_config(config, provider)?;
        if !stored.load(Ordering::Acquire) {
            return Err(AgentLoopError::Provider(
                "provider profile or credential was not found".to_owned(),
            ));
        }
        assert_eq!(config.models.default, "fast");
        assert_eq!(
            config.models.aliases["fast"],
            vec!["openai/catalog-discovery"]
        );
        Ok(ActivatedHostedProvider {
            replacement_model: Arc::new(QuickConnectedModel),
            pre_commit: None,
            post_commit: None,
        })
    });
    let model =
        RecomposableHostedModel::new(unavailable, Arc::new(QuickCatalogSource(false)), activate);
    assert!(quick_connect_stream(&model).is_err());

    loader
        .configure_provider_profile("openai", "openai")
        .expect("fixed built-in profile must persist");
    credential_stored.store(true, Ordering::Release);
    model
        .activate_provider("openai", Some("fast"))
        .await
        .expect("quick-connect must stage the connected provider");
    model
        .prepare_model("openai/live-model")
        .await
        .expect("selected live model must prepare");
    model.commit_prepared_model("openai/live-model");
    assert!(
        !ModelCatalogSource::discover(&model)
            .await
            .expect("stable catalog source must remain available")
            .truncated,
        "credential activation must not replace the catalog source"
    );
    let events = model
        .stream(
            "openai/live-model",
            quick_connect_request(),
            test_provider_invocation(),
        )
        .expect("recomposed model must dispatch")
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|event| {
        matches!(event, Ok(ProviderEvent::TextDelta { text }) if text == "quick-connect-ok")
    }));
}

#[tokio::test]
async fn healthy_runtime_activation_keeps_model_and_catalog_stable() {
    let activated = Arc::new(Mutex::new(Vec::new()));
    let activation_log = Arc::clone(&activated);
    let activate: Arc<HostedProviderActivator> = Arc::new(move |provider| {
        activation_log
            .lock()
            .expect("activation log")
            .push(provider.to_owned());
        Ok(ActivatedHostedProvider {
            replacement_model: Arc::new(ExistingRouteModel),
            pre_commit: None,
            post_commit: None,
        })
    });
    let model = RecomposableHostedModel::new(
        Arc::new(ExistingRouteModel),
        Arc::new(QuickCatalogSource(false)),
        activate,
    );
    assert!(model.has_model_alias("local/base"));

    model
        .activate_provider("github_copilot", Some("local/base"))
        .await
        .expect("provider connection must activate");
    assert_eq!(
        *activated.lock().expect("activation log"),
        vec!["github_copilot"]
    );
    assert!(model.has_model_alias("local/base"));
    assert!(!model.has_model_alias("openai/live-model"));
    assert!(
        !ModelCatalogSource::discover(&model)
            .await
            .expect("catalog remains available")
            .truncated,
        "activation must not replace the catalog source"
    );
}
