#![cfg(test)]
use super::ActivatedHostedProvider;
use super::Arc;
use super::BTreeMap;
use super::ClientId;
use super::Config;
use super::Duration;
use super::ExistingRouteModel;
use super::HostedProviderActivator;
use super::HostedProviderMode;
use super::HostedSessionComposition;
use super::JournalService;
use super::ModelCatalogSource;
use super::Mutex;
use super::PermissionDecision;
use super::PermissionGate;
use super::PermissionMode;
use super::ProviderConfig;
use super::QuickCatalogSource;
use super::QuickConnectedModel;
use super::RecomposableHostedModel;
use super::SessionActor;
use super::SessionActorConfig;
use super::SessionId;
use super::SystemEventClock;
use super::TempDir;
use super::ThinkingLevel;
use super::ToolRegistry;
use super::builtin_command_registry;
use super::builtin_hook_dispatcher;
use super::compose_hosted_actor;
use super::merge_reloaded_provider_config;
use super::prepare_isolated_model_initialization_config;
use super::prepare_isolated_provider_activation_config;
use super::prepare_provider_activation_config;
use super::tempdir;
use super::test_provider_admission;
use rw_core::ModelDriver;

#[tokio::test]
async fn provider_activation_is_independent_from_the_previous_model_selection() {
    let activate: Arc<HostedProviderActivator> = Arc::new(move |_| {
        Ok(ActivatedHostedProvider {
            replacement_model: Arc::new(QuickConnectedModel),
            pre_commit: None,
            post_commit: None,
        })
    });
    let model = RecomposableHostedModel::new(
        Arc::new(ExistingRouteModel),
        Arc::new(QuickCatalogSource(false)),
        activate,
    );

    model
        .activate_provider("openai", Some("local/base"))
        .await
        .expect("connecting a provider must not validate or switch the selected model");
    assert!(model.has_model_alias("openai/live-model"));
    assert!(model.has_provider_for_alias("openai/live-model", "openai"));
    assert!(!model.has_provider_for_alias("openai/live-model", "github_copilot"));
    assert!(model.has_model_alias("local/base"));
    model
        .prepare_model("openai/live-model")
        .await
        .expect("explicit model selection prepares the staged provider runtime");
    assert!(
        model.has_model_alias("local/base"),
        "preparation must not change the active runtime before persistence"
    );
    model
        .prepare_model("local/base")
        .await
        .expect("concurrent title preparation stays on the current runtime");
    model.commit_prepared_model("openai/live-model");
    assert!(model.has_model_alias("openai/live-model"));
    assert!(
        model.has_model_alias("local/base"),
        "the retained previous generation must remain selectable"
    );
    model
        .prepare_model("local/base")
        .await
        .expect("the previous provider generation remains selectable");
    model.commit_prepared_model("local/base");
    assert!(model.has_model_alias("local/base"));
    assert!(
        model.has_model_alias("openai/live-model"),
        "the prior provider generation must remain selectable after switching back"
    );
}

#[tokio::test]
async fn engine_switches_to_an_exact_model_route_staged_by_provider_activation() {
    let activate: Arc<HostedProviderActivator> = Arc::new(move |_| {
        Ok(ActivatedHostedProvider {
            replacement_model: Arc::new(QuickConnectedModel),
            pre_commit: None,
            post_commit: None,
        })
    });
    let model = Arc::new(RecomposableHostedModel::new(
        Arc::new(ExistingRouteModel),
        Arc::new(QuickCatalogSource(false)),
        activate,
    ));
    model
        .activate_provider("openai", Some("local/base"))
        .await
        .expect("provider activation");

    let workspace = tempdir().expect("workspace");
    let session_id = SessionId("staged-provider-switch".to_owned());
    let actor = SessionActor::spawn(SessionActorConfig {
        budget_session_id: session_id.clone(),
        session_id: session_id.clone(),
        workspace_root: workspace.path().to_path_buf(),
        additional_workspace_roots: Vec::new(),
        workspace_generation: 0,
        initial_session_context: Vec::new(),
        startup_notifications: Vec::new(),
        model_alias: "local/base".to_owned(),
        model,
        tools: Arc::new(ToolRegistry::new()),
        permissions: Arc::new(PermissionGate::new(PermissionDecision::Allow)),
        hooks: Arc::new(builtin_hook_dispatcher().expect("hooks")),
        commands: Arc::new(builtin_command_registry().expect("commands")),
        modes: Arc::new(rw_ext::ModeRegistry::builtins().expect("built-in modes")),
        event_sink: Arc::new(rw_core::NoopSessionEventSink::default()),
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
    let command_meta = |request: &str| rw_core::CommandMeta {
        protocol_version: rw_core::PROTOCOL_VERSION,
        client_id: ClientId("driver".to_owned()),
        request_id: rw_core::RequestId(request.to_owned()),
    };
    assert_eq!(
        actor
            .dispatch(rw_core::ClientCommand::SwitchModel {
                meta: command_meta("switch"),
                session_id,
                model: rw_core::ModelAlias("openai/live-model".to_owned()),
                provider: Some("openai".to_owned()),
            })
            .await
            .expect("switch"),
        rw_core::CommandOutcome::Accepted {}
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if actor.snapshot().await.expect("snapshot").model_alias == "openai/live-model" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("staged model committed");
}

#[test]
fn provider_activation_config_preserves_the_configured_alias_graph() {
    let mut config = Config::default();
    config.providers.insert(
        "github_copilot".to_owned(),
        ProviderConfig {
            kind: "github_copilot".to_owned(),
            ..ProviderConfig::default()
        },
    );
    config.providers.insert(
        "broken".to_owned(),
        ProviderConfig {
            kind: "not-a-provider".to_owned(),
            ..ProviderConfig::default()
        },
    );
    config.models.default = "current".to_owned();
    config
        .models
        .aliases
        .insert("current".to_owned(), vec!["broken/not-real".to_owned()]);
    config.models.aliases.insert(
        "copilot".to_owned(),
        vec![
            "broken/not-real".to_owned(),
            "github_copilot/gpt-5-mini".to_owned(),
        ],
    );

    let expected_aliases = config.models.aliases.clone();
    let activation =
        prepare_provider_activation_config(config, "github_copilot").expect("activation config");
    assert_eq!(activation.models.default, "current");
    assert_eq!(activation.models.aliases, expected_aliases);
    assert!(activation.providers.contains_key("broken"));
    assert_eq!(
        activation.providers["github_copilot"].kind,
        "github_copilot"
    );
}

#[test]
fn isolated_provider_activation_retains_only_selected_provider_routes() {
    let mut config = Config::default();
    config.providers.insert(
        "github_copilot".to_owned(),
        ProviderConfig {
            kind: "github_copilot".to_owned(),
            ..ProviderConfig::default()
        },
    );
    config.providers.insert(
        "broken".to_owned(),
        ProviderConfig {
            kind: "not-a-provider".to_owned(),
            ..ProviderConfig::default()
        },
    );
    config.models.default = "current".to_owned();
    config
        .models
        .aliases
        .insert("current".to_owned(), vec!["broken/not-real".to_owned()]);
    config.models.aliases.insert(
        "already-connected".to_owned(),
        vec!["github_copilot/gpt-4.1".to_owned()],
    );

    let recovery = prepare_isolated_provider_activation_config(config, "github_copilot")
        .expect("isolated activation config");
    assert_eq!(
        recovery.models.aliases,
        BTreeMap::from([(
            "already-connected".to_owned(),
            vec!["github_copilot/gpt-4.1".to_owned()]
        )])
    );
    assert_eq!(recovery.models.default, "already-connected");
    assert_eq!(
        recovery.providers.keys().collect::<Vec<_>>(),
        vec!["github_copilot"]
    );
    assert!(recovery.models.thinking.is_empty());
}

#[test]
fn isolated_model_initialization_retains_only_selected_alias_routes() {
    let mut config = Config::default();
    for provider in ["openai", "anthropic", "github_copilot"] {
        config.providers.insert(
            provider.to_owned(),
            ProviderConfig {
                kind: provider.to_owned(),
                ..ProviderConfig::default()
            },
        );
    }
    config.models.default = "other".to_owned();
    config.models.aliases = BTreeMap::from([
        (
            "fast".to_owned(),
            vec!["openai/gpt-5-mini".to_owned(), "anthropic/haiku".to_owned()],
        ),
        (
            "other".to_owned(),
            vec!["github_copilot/gpt-4.1".to_owned()],
        ),
    ]);
    config.models.thinking = BTreeMap::from([
        ("fast".to_owned(), ThinkingLevel::Low),
        ("other".to_owned(), ThinkingLevel::High),
    ]);

    let isolated = prepare_isolated_model_initialization_config(config, "fast")
        .expect("selected alias should isolate");

    assert_eq!(isolated.models.default, "fast");
    assert_eq!(
        isolated.models.aliases,
        BTreeMap::from([(
            "fast".to_owned(),
            vec!["openai/gpt-5-mini".to_owned(), "anthropic/haiku".to_owned()]
        )])
    );
    assert_eq!(
        isolated.providers.keys().cloned().collect::<Vec<_>>(),
        vec!["anthropic".to_owned(), "openai".to_owned()]
    );
    assert_eq!(
        isolated.models.thinking,
        BTreeMap::from([("fast".to_owned(), ThinkingLevel::Low)])
    );
}

#[test]
fn isolated_model_initialization_builds_only_concrete_provider_route() {
    let mut config = Config::default();
    config.providers.insert(
        "github_copilot".to_owned(),
        ProviderConfig {
            kind: "github_copilot".to_owned(),
            ..ProviderConfig::default()
        },
    );
    config.providers.insert(
        "openai".to_owned(),
        ProviderConfig {
            kind: "openai".to_owned(),
            ..ProviderConfig::default()
        },
    );
    config
        .models
        .aliases
        .insert("fast".to_owned(), vec!["openai/gpt-5-mini".to_owned()]);
    config
        .models
        .thinking
        .insert("fast".to_owned(), ThinkingLevel::Low);

    let isolated = prepare_isolated_model_initialization_config(config, "github_copilot/gpt-4.1")
        .expect("concrete model should isolate");

    assert_eq!(isolated.models.default, "__selected_model");
    assert_eq!(
        isolated.models.aliases,
        BTreeMap::from([(
            "__selected_model".to_owned(),
            vec!["github_copilot/gpt-4.1".to_owned()]
        )])
    );
    assert_eq!(
        isolated.providers.keys().collect::<Vec<_>>(),
        vec!["github_copilot"]
    );
    assert!(isolated.models.thinking.is_empty());
}

#[test]
fn reloaded_provider_merge_preserves_startup_precedence() {
    let mut base = Config::default();
    base.providers.insert(
        "openai".to_owned(),
        ProviderConfig {
            kind: "openai".to_owned(),
            base_url: Some("https://startup.invalid/v1/responses".to_owned()),
            ..ProviderConfig::default()
        },
    );
    let mut loaded = Config::default();
    loaded.providers.insert(
        "openai".to_owned(),
        ProviderConfig {
            kind: "anthropic".to_owned(),
            base_url: Some("https://file.invalid/v1/messages".to_owned()),
            ..ProviderConfig::default()
        },
    );
    let merged = merge_reloaded_provider_config(base, loaded);
    let provider = &merged.providers["openai"];
    assert_eq!(provider.kind, "openai");
    assert_eq!(
        provider.base_url.as_deref(),
        Some("https://startup.invalid/v1/responses")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn stalled_provider_activation_is_bounded_and_keeps_runtime() {
    let activate: Arc<HostedProviderActivator> = Arc::new(move |_| {
        std::thread::sleep(Duration::from_millis(150));
        Ok(ActivatedHostedProvider {
            replacement_model: Arc::new(QuickConnectedModel),
            pre_commit: None,
            post_commit: None,
        })
    });
    let model = Arc::new(RecomposableHostedModel::with_deadline(
        Arc::new(ExistingRouteModel),
        Arc::new(QuickCatalogSource(false)),
        activate,
        Duration::from_millis(10),
    ));
    let activation_model = Arc::clone(&model);
    let activation = tokio::spawn(async move {
        activation_model
            .activate_provider("openai", Some("openai/live-model"))
            .await
    });
    tokio::time::timeout(
        Duration::from_millis(50),
        tokio::time::sleep(Duration::from_millis(1)),
    )
    .await
    .expect("blocking activation must not stall the runtime worker");
    let error = activation
        .await
        .expect("activation task")
        .expect_err("stalled activation must time out");
    assert!(error.to_string().contains("timed out"));
    let retry_started = std::time::Instant::now();
    let retry = model
        .activate_provider("openai", None)
        .await
        .expect_err("a detached activation must bound concurrent retries");
    assert!(retry.to_string().contains("already in progress"));
    assert!(retry_started.elapsed() < Duration::from_millis(50));
    assert!(model.has_model_alias("local/base"));
    assert!(
        !ModelCatalogSource::discover(model.as_ref())
            .await
            .expect("old catalog remains available")
            .truncated
    );
    tokio::time::sleep(Duration::from_millis(160)).await;
}

#[tokio::test]
async fn staged_activation_runs_callbacks_at_connection_then_explicit_selection() {
    let callbacks = Arc::new(Mutex::new(Vec::new()));
    let initial_callbacks = Arc::clone(&callbacks);
    let initial_post_commit: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        initial_callbacks
            .lock()
            .expect("initial callback log")
            .push("initial");
    });
    initial_post_commit();
    let pre_callbacks = Arc::clone(&callbacks);
    let post_callbacks = Arc::clone(&callbacks);
    let activate: Arc<HostedProviderActivator> = Arc::new(move |_| {
        Ok(ActivatedHostedProvider {
            replacement_model: Arc::new(QuickConnectedModel),
            pre_commit: Some(Arc::new({
                let callbacks = Arc::clone(&pre_callbacks);
                move || callbacks.lock().expect("pre callback log").push("pre")
            })),
            post_commit: Some(Arc::new({
                let callbacks = Arc::clone(&post_callbacks);
                move || callbacks.lock().expect("post callback log").push("post")
            })),
        })
    });
    let model = Arc::new(RecomposableHostedModel::new_with_active_callback(
        Arc::new(ExistingRouteModel),
        Arc::new(QuickCatalogSource(false)),
        activate,
        Some(initial_post_commit),
    ));
    let activation_model = Arc::clone(&model);
    let activation = tokio::spawn(async move {
        activation_model
            .activate_provider("openai", Some("openai/live-model"))
            .await
    });
    activation
        .await
        .expect("activation task")
        .expect("activation succeeds");
    assert_eq!(
        *callbacks.lock().expect("callback log"),
        vec!["initial", "pre"]
    );
    assert!(model.has_model_alias("local/base"));
    assert!(
        model.has_model_alias("openai/live-model"),
        "staged route visibility must not run the post-commit callback"
    );
    model
        .prepare_model("openai/live-model")
        .await
        .expect("explicit selection prepares the staged runtime");
    assert_eq!(
        *callbacks.lock().expect("callback log"),
        vec!["initial", "pre"]
    );
    model.discard_prepared_model("openai/live-model");
    assert_eq!(
        *callbacks.lock().expect("callback log"),
        vec!["initial", "pre"]
    );
    assert!(model.has_model_alias("local/base"));
    model
        .prepare_model("openai/live-model")
        .await
        .expect("selection can be prepared again after rejection");
    model.commit_prepared_model("openai/live-model");
    assert_eq!(
        *callbacks.lock().expect("callback log"),
        vec!["initial", "pre", "post"]
    );
    assert!(
        model.has_model_alias("local/base"),
        "committing a provider switch must retain the previous selectable generation"
    );
    assert!(model.has_model_alias("openai/live-model"));
    model
        .prepare_model("local/base")
        .await
        .expect("switching back prepares the retained initial generation");
    model.commit_prepared_model("local/base");
    assert_eq!(
        *callbacks.lock().expect("callback log"),
        vec!["initial", "pre", "post", "initial"]
    );
    assert!(
        !ModelCatalogSource::discover(model.as_ref())
            .await
            .expect("catalog remains stable")
            .truncated
    );
}

#[tokio::test]
async fn hosted_resume_with_unavailable_concrete_model_keeps_control_plane_usable() {
    let root = TempDir::new().expect("root");
    let storage = root.path().join("storage");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&storage).expect("storage");
    std::fs::create_dir(&workspace).expect("workspace");
    #[cfg(unix)]
    std::fs::set_permissions(
        &storage,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("private storage");
    let workspace = workspace.canonicalize().expect("canonical workspace");
    let mut config = Config::default();
    config.models.default = "fast".to_owned();
    config
        .models
        .aliases
        .insert("fast".to_owned(), vec!["fixture/base".to_owned()]);
    for provider in ["fixture", "extra"] {
        config.providers.insert(
            provider.to_owned(),
            ProviderConfig {
                kind: "openai_compatible".to_owned(),
                base_url: Some("http://127.0.0.1:1/v1/chat/completions".to_owned()),
                ..ProviderConfig::default()
            },
        );
    }
    let session_id = SessionId("unavailable-concrete-resume".to_owned());
    let provider_admission = Arc::new(
        crate::provider_admission::DurableProviderAdmission::open(storage.clone())
            .await
            .expect("test authority"),
    );
    let options = |resume, requested_model| HostedSessionComposition {
        provider_admission: Arc::clone(&provider_admission),
        plugin_activation: Arc::new(crate::extension_runtime::PluginActivationBudget::default()),
        wasm_workers: rw_ext::WasmWorkerPool::new(),
        index_pool: Arc::new(rw_tools::WorkspaceIndexPool::default()),
        journal_service: JournalService::new(&storage).expect("journal reads"),
        workspace: workspace.clone(),
        additional_workspaces: Vec::new(),
        allowed_workspace_roots: vec![workspace.clone()],
        storage_root: storage.clone(),
        credentials_path: storage.join("credentials.json"),
        config: config.clone(),
        session_id: session_id.clone(),
        requested_model,
        resume,
        permission_mode: Some(PermissionMode::Strict),
        max_turns: 2,
        provider_mode: HostedProviderMode::Live,
        dangerously_trust: false,
        wait_for_execution_lease: false,
    };
    let initial = compose_hosted_actor(options(false, Some("extra/new-model".to_owned())))
        .await
        .expect("initial control plane");
    initial
        .handle
        .context_snapshot()
        .await
        .expect("initial control query");
    drop(initial);
    tokio::task::yield_now().await;

    let resumed = compose_hosted_actor(options(true, None))
        .await
        .expect("resume must remain ready");
    resumed
        .handle
        .context_snapshot()
        .await
        .expect("resumed control plane query");
    drop(resumed);
}
