use super::accounting_projection::collect_abandoned_empty_sessions;
use super::checkpoint_journal::open_checkpoint_stores;
use super::checkpoint_journal::preview_persisted_workspace_roots;
use super::checkpoint_journal::restore_persisted_workspace_roots;
use super::checkpoints::DurableCheckpointCoordinator;
use super::checkpoints::recover_rewind_transactions;
use super::command_execution::CommandFixtureMode;
use super::credential_resolution::DeferredToolProxy;
use super::credential_resolution::DeferredWebSearchHeaders;
use super::custom_commands::RuntimeCommandRegistry;
use super::custom_commands::compose_runtime_commands;
use super::durable_session::DurableEventSink;
use super::durable_session::TodoRestoreBinding;
use super::durable_session::load_session_events;
use super::extension_discovery::discover_runtime_extensions;
use super::extension_discovery::extension_startup_notifications;
use super::extension_discovery::extension_user_roots;
use super::extension_discovery::skill_index_turn;
use super::folder_trust::RuntimeFolderTrustController;
use super::folder_trust::project_approval_path;
use super::initial_memory::fresh_initial_session_context;
use super::interaction_policy::UnboundQuestionAsker;
use super::native_search::AliasAwareWebSearchModel;
use super::native_search::provider_native_search_available;
use super::nested_instructions::NestedInstructionsModel;
use super::nested_instructions::register_nested_instruction_guard;
use super::plugin_event_fanout::PluginFanoutEventSink;
use super::prompt_model::PromptRecordingModel;
use super::provider_activation::lazy_live_provider_model;
use super::provider_adapter::ProviderModel;
use super::provider_adapter::configured_session_thinking;
use super::provider_catalog::PersistingHostedCatalogSource;
use super::provider_catalog::load_effective_pricing_table;
use super::runtime_options::DEFAULT_DOOM_LOOP_LIMIT;
use super::runtime_options::DEFAULT_EVENT_CAPACITY;
use super::runtime_options::DEFAULT_MAX_OUTPUT_TOKENS;
use super::runtime_options::HostedActorRuntime;
use super::runtime_options::HostedProviderMode;
use super::runtime_options::HostedSessionComposition;
use super::runtime_options::display_agent_error;
use super::script_provider::ScriptProvider;
use super::secret_redaction::SharedCommandFixtureRedactor;
use super::secret_redaction::SharedEngineSecretRedactor;
use super::secret_redaction::register_credential_environment;
use super::session_metadata::load_session_metadata;
use super::session_metadata::persist_session_metadata;
use super::session_metadata::validate_session_id;
use super::session_selection::acquire_shared_execution_lease;
use super::session_selection::checkpoint_root;
use super::session_selection::workspace_execution_lease_path;
use super::subagent_recovery::recover_subagent_tree;
use super::subagent_runtime::ChildActorTemplate;
use super::subagent_runtime::HostedSubagentController;
use super::subagent_runtime::RuntimeSubagentSessionFactory;
use super::todo_restore::restore_todo_state;
use super::tool_composition::BuildToolsInput;
use super::tool_composition::build_tools;
use super::tool_composition::trusted_lsp_roots;
use super::toolchain::RuntimeServiceView;
use super::toolchain::ToolchainRuntime;
use super::wasm_hooks::compose_runtime_hooks_with_extensions;
use super::wasm_hooks::compose_runtime_hooks_with_extensions_validated;
use super::workspace_roots::RuntimeWorkspaceRootController;
use super::workspace_roots::WorkspaceRootAuthorization;
use super::workspace_roots::canonical_workspace_roots;
use crate::storage_root::initialize_private_storage_root;
use miette::IntoDiagnostic;
use miette::Result;
use miette::miette;
use rw_core::ActorSubagentSessionFactory;
use rw_core::CachedModelCatalog;
use rw_core::HostSubagentService;
use rw_core::ModelCatalogSource;
use rw_core::ModelDriver;
use rw_core::PermissionGate;
use rw_core::ProviderFactory;
use rw_core::ProviderModelCatalogSource;
use rw_core::SessionActor;
use rw_core::SessionActorConfig;
use rw_core::SessionEventSink;
use rw_core::SpawnAgentTool;
use rw_core::SubagentLimits;
use rw_core::SubagentOrchestrator;
use rw_core::SubagentSessionFactory;
use rw_core::SystemEventClock;
use rw_core::WorktreeSubagentSessionFactory;
use rw_ext::compose_agent_registry;
use rw_providers::FixtureRedactor;
use rw_providers::Provider;
use rw_store::catalog_cache::load_model_catalog_cache;
use rw_store::config::ConfigLoader;
use rw_store::session::SessionEventLog;
use rw_tools::ApplyWorktreeDiffTool;
use rw_tools::CancellationToken;
use rw_tools::CommandFixtureRedactor;
use rw_tools::CommandSafetyClassifier;
use rw_tools::QuestionAsker;
use rw_tools::WorktreeIsolation;
use rw_tools::WorktreeLimits;
use rw_types::PermissionModeDescriptor as PermissionMode;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::RwLock;

/// Composes one durable hosted actor through the same storage, tool, provider,
/// checkpoint, prompt-shape, accounting, and recovery boundaries used by the
/// CLI runtime. Presentation and transport stay outside this function.
#[allow(clippy::too_many_lines)]
pub(crate) async fn compose_hosted_actor(
    options: HostedSessionComposition,
) -> Result<HostedActorRuntime> {
    if options.max_turns == 0 {
        return Err(miette!("--max-turns must be greater than zero"));
    }
    validate_session_id(&options.session_id.0)?;
    let extension_credentials_path = options.credentials_path.clone();
    let workspace = std::fs::canonicalize(&options.workspace).into_diagnostic()?;
    if workspace != options.workspace {
        return Err(miette!("hosted workspace must already be canonical"));
    }
    let mut workspace_roots =
        canonical_workspace_roots(&workspace, &options.additional_workspaces)?;
    let allowed_workspace_roots =
        canonical_workspace_roots(&workspace, &options.allowed_workspace_roots)?;
    if workspace_roots.iter().any(|root| {
        !allowed_workspace_roots
            .iter()
            .any(|allowed| root == allowed || root.starts_with(allowed))
    }) {
        return Err(miette!(
            "hosted workspace roots must stay inside the host authorization policy"
        ));
    }
    let mut persisted_workspace_generation = 0_u64;
    if options.permission_mode == Some(PermissionMode::Yolo)
        && workspace == Path::new("/")
        && rustix::process::geteuid().is_root()
    {
        return Err(miette!(
            "--permission-mode yolo is refused for root while the workspace is /"
        ));
    }
    initialize_private_storage_root(&options.storage_root).into_diagnostic()?;
    collect_abandoned_empty_sessions(&options.storage_root)?;

    let session_id = options.session_id.0.clone();
    let log = SessionEventLog::open(&options.storage_root, &session_id)
        .map_err(|error| miette!("session log could not open: {error}"))?;
    if options.resume {
        let source = log.read_view();
        let committed = tokio::task::spawn_blocking(move || {
            rw_core::recovery::WorkspaceBootstrap::read(&source)
        })
        .await
        .map_err(|error| miette!("workspace bootstrap worker failed: {error}"))?
        .map_err(|error| miette!("workspace bootstrap failed: {error}"))?;
        if let Some(generation) = preview_persisted_workspace_roots(
            &checkpoint_root(&options.storage_root, &workspace, &session_id),
            &workspace,
            &workspace_roots,
            committed.generation,
        )? {
            persisted_workspace_generation = generation.generation;
            workspace_roots = generation.roots;
        }
        if workspace_roots.iter().any(|root| {
            !allowed_workspace_roots
                .iter()
                .any(|allowed| root == allowed || root.starts_with(allowed))
        }) {
            return Err(miette!(
                "persisted workspace root is outside the current host authorization policy"
            ));
        }
    }
    let (extension_user_home, extension_user_rottweiler) =
        extension_user_roots(&extension_credentials_path);
    let extension_catalog = Arc::new(discover_runtime_extensions(
        &workspace_roots,
        &options.storage_root.join("trust.json"),
        &extension_user_home,
        &extension_user_rottweiler,
        options.dangerously_trust,
    )?);
    let inherited_journal_through = if options.resume {
        super::session_metadata::load_session_metadata_any(&options.storage_root, &session_id)?
            .inherited_journal_through
    } else {
        None
    };
    let runtime_modes = crate::mode_recovery::compose_and_validate(
        &extension_catalog,
        log.read_view(),
        inherited_journal_through,
    )
    .await?;
    if options.resume
        && let Some(generation) = restore_persisted_workspace_roots(
            &checkpoint_root(&options.storage_root, &workspace, &session_id),
            &workspace,
            &workspace_roots,
            persisted_workspace_generation,
        )?
    {
        persisted_workspace_generation = generation.generation;
        workspace_roots = generation.roots;
    }
    let execution_lease_path = workspace_execution_lease_path(&options.storage_root, &workspace)?;
    let wait_for_execution_lease = options.wait_for_execution_lease;
    let execution_lease = tokio::task::spawn_blocking(move || {
        acquire_shared_execution_lease(&execution_lease_path, wait_for_execution_lease)
    })
    .await
    .map_err(|error| miette!("execution lease worker failed: {error}"))?
    .map_err(|error| miette!("execution lease could not lock: {error}"))?;

    let configured_model_alias = options
        .requested_model
        .clone()
        .unwrap_or_else(|| options.config.models.default.clone());
    let (mut initial_context, persisted_model_alias, budget_session_id) = if options.resume {
        let metadata = load_session_metadata(&options.storage_root, &session_id, &workspace)?;
        let mut context = metadata.initial_session_context;
        let recorded_count = metadata.initial_context_workspace_root_count;
        for root in workspace_roots.iter().skip(recorded_count) {
            if let Some(instructions) = rw_core::load_root_project_instructions(root)
                .map_err(|error| miette!("project instructions could not load: {error}"))?
            {
                context.push(instructions.as_system_turn());
            }
        }
        (context, metadata.model_alias, metadata.budget_session_id)
    } else {
        let context = fresh_initial_session_context(&options.storage_root, &workspace_roots)
            .map_err(|error| miette!("project instructions could not load: {error}"))?;
        persist_session_metadata(
            &options.storage_root,
            &session_id,
            &workspace,
            &configured_model_alias,
            &context,
            &workspace_roots,
        )?;
        (
            context,
            configured_model_alias,
            rw_types::SessionId(session_id.clone()),
        )
    };

    let session_checkpoint_root = checkpoint_root(&options.storage_root, &workspace, &session_id);
    let checkpoint_stores = open_checkpoint_stores(&session_checkpoint_root, &workspace_roots)?;
    let recovery_stores = Arc::clone(&checkpoint_stores);
    tokio::task::spawn_blocking(move || {
        let mut operation = rw_store::checkpoint::CheckpointOperation::default();
        for store in recovery_stores.iter() {
            store.recover_opaque_mutations(&mut operation)?;
        }
        Ok::<_, rw_store::checkpoint::CheckpointError>(())
    })
    .await
    .map_err(|error| miette!("checkpoint recovery worker failed: {error}"))?
    .map_err(|error| miette!("checkpoint recovery failed: {error}"))?;
    let rewind_stores = Arc::clone(&checkpoint_stores);
    let rewind_checkpoint_root = session_checkpoint_root.clone();
    let log = tokio::task::spawn_blocking(move || {
        let mut log = log;
        recover_rewind_transactions(&rewind_checkpoint_root, &rewind_stores, &mut log)?;
        Ok::<_, miette::Report>(log)
    })
    .await
    .map_err(|error| miette!("rewind recovery worker failed: {error}"))??;
    let recovered_events = load_session_events(&log)?;
    let recovered = crate::mode_recovery::project(&recovered_events, &runtime_modes)?;
    let descriptor_model = recovered
        .model_alias
        .clone()
        .unwrap_or_else(|| persisted_model_alias.clone());
    let driver_client_id = recovered.driver_client_id.clone();
    let shell_active = recovered.active_shell.is_some();
    let durable_sink = DurableEventSink::new_hosted(
        log,
        options.storage_root.clone(),
        session_id.clone(),
        &recovered_events,
        Arc::clone(&options.journal_service),
    )?;
    durable_sink
        .bind_canonical(Arc::clone(&runtime_modes))
        .await
        .map_err(|error| miette!("canonical recovery failed: {error}"))?;
    durable_sink.reconcile_accounting(&recovered_events)?;
    let checkpoint_coordinator = Arc::new(DurableCheckpointCoordinator::from_stores(
        session_checkpoint_root,
        checkpoint_stores,
    ));

    let offline = matches!(
        options.provider_mode,
        HostedProviderMode::DeterministicReplay { .. }
    );
    let fixture_redactor = FixtureRedactor::default();
    register_credential_environment(&fixture_redactor);
    let command_fixture_mode = if offline {
        CommandFixtureMode::Offline
    } else {
        CommandFixtureMode::Live
    };
    let proxy_credentials_path = options.credentials_path.clone();
    let deferred_global_proxy = DeferredToolProxy::from_config(
        &options.config,
        &proxy_credentials_path,
        offline,
        fixture_redactor.clone(),
    )?;
    let websearch_config = options.config.websearch.clone();
    let deferred_websearch_headers = DeferredWebSearchHeaders::from_config(
        &websearch_config,
        &proxy_credentials_path,
        offline,
        fixture_redactor.clone(),
    );
    let global_proxy = None;
    let websearch_headers = BTreeMap::new();
    let tool_workspace_roots = workspace_roots.clone();
    let tool_execution_lease = Arc::clone(&execution_lease);
    let root_question_asker: Arc<dyn QuestionAsker> = Arc::new(UnboundQuestionAsker);
    let command_safety = Arc::new(
        CommandSafetyClassifier::new(&options.config.sandbox.safe_list)
            .map_err(|error| miette!(error))?,
    );
    let tool_command_safety = Arc::clone(&command_safety);
    let root_command_safety = Arc::clone(&command_safety);
    let tool_question_asker = Arc::clone(&root_question_asker);
    let root_command_fixture_mode = command_fixture_mode.clone();
    let root_global_proxy = global_proxy.clone();
    let root_deferred_global_proxy = deferred_global_proxy.clone();
    let root_execution_lease = Arc::clone(&execution_lease);
    let root_websearch_config = websearch_config.clone();
    let root_websearch_headers = websearch_headers.clone();
    let root_deferred_websearch_headers = deferred_websearch_headers.clone();
    let background_redactor: Arc<dyn CommandFixtureRedactor> =
        Arc::new(SharedCommandFixtureRedactor(fixture_redactor.clone()));
    let root_background_redactor = Arc::clone(&background_redactor);
    let native_websearch_possible = !offline && provider_native_search_available(&options.config);
    let trusted_lsp_roots = trusted_lsp_roots(
        &tool_workspace_roots,
        &options.storage_root.join("trust.json"),
        options.dangerously_trust,
    )?;
    let trusted_read_roots = workspace_roots
        .iter()
        .zip(&trusted_lsp_roots)
        .filter_map(|(root, trusted)| trusted.then_some(root.clone()))
        .collect::<Vec<_>>();
    let derived_project_trusted = trusted_lsp_roots.first().copied().unwrap_or(false);
    let tool_index_pool = Arc::clone(&options.index_pool);
    let mut built_tools = tokio::task::spawn_blocking(move || {
        build_tools(BuildToolsInput {
            index_pool: tool_index_pool,
            workspace_roots: &tool_workspace_roots,
            trusted_lsp_roots: &trusted_lsp_roots,
            question_asker: tool_question_asker,
            offline,
            global_proxy: global_proxy.as_ref(),
            deferred_global_proxy,
            command_fixture_mode,
            execution_lease: tool_execution_lease,
            command_safety: &tool_command_safety,
            websearch_config: &websearch_config,
            websearch_headers: &websearch_headers,
            deferred_websearch_headers,
            native_websearch_possible,
            background_redactor,
            background_manager: None,
        })
    })
    .await
    .map_err(|error| miette!("tool startup worker failed: {error}"))??;
    restore_todo_state(
        &recovered.conversation,
        &workspace,
        &options.session_id,
        &built_tools.todo,
    )
    .await?;
    durable_sink.bind_todo(TodoRestoreBinding {
        todo: Arc::clone(&built_tools.todo),
        workspace: workspace.clone(),
        session_id: options.session_id.clone(),
    });

    let executable_catalog = if offline {
        crate::extension_config::ExecutableConfigCatalog::default()
    } else {
        let (user_home, _) = extension_user_roots(&extension_credentials_path);
        let catalog = crate::extension_config::discover_executable_configs(
            &user_home,
            &workspace,
            derived_project_trusted || options.dangerously_trust,
        )?;
        for warning in &catalog.warnings {
            tracing::warn!("{warning}");
        }
        catalog
    };
    let mcp_runtime = {
        let runtime = Arc::new(
            crate::extension_runtime::McpSessionRuntime::start_production(
                &executable_catalog.mcp_servers,
                &workspace_roots,
                &options.storage_root.join("sessions").join(&session_id),
                &std::env::current_exe().into_diagnostic()?,
                &options.credentials_path,
                root_global_proxy
                    .as_ref()
                    .map(|proxy| proxy.upstream.clone()),
            )
            .await?,
        );
        let names = built_tools
            .registry
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>();
        let mut registry = built_tools
            .registry
            .subset(names.iter().map(String::as_str))
            .map_err(|error| miette!("MCP tool registry could not clone: {error}"))?;
        rw_core::register_mcp_tools(
            &mut registry,
            Arc::clone(&runtime.manager),
            Arc::clone(&runtime.spool),
        )
        .map_err(|error| miette!("MCP tools could not register: {error}"))?;
        built_tools.registry = Arc::new(registry);
        if let Some(index) = runtime.deferred_context().await? {
            initial_context.push(index);
        }
        Some(runtime)
    };
    let mcp_resources =
        crate::session_resources::RuntimeSessionResources::new(mcp_runtime.clone(), None);
    let mcp_admin: Option<Arc<dyn rw_core::HostMcpService>> = mcp_runtime.as_ref().map(|runtime| {
        Arc::new(
            crate::extension_runtime::LiveMcpAdmin::new_with_stdio_environment(
                Arc::clone(&runtime.manager),
                Arc::clone(&runtime.approvals),
                ConfigLoader::new(
                    options.credentials_path.with_file_name("config.toml"),
                    workspace.join(".rottweiler/config.toml"),
                ),
                Arc::clone(&runtime.stdio_environment),
            ),
        ) as Arc<dyn rw_core::HostMcpService>
    });
    let plugin_redactor = Arc::new(crate::extension_runtime::SharedPluginRedactor::new(
        fixture_redactor.clone(),
    ));
    let plugin_runtime = if executable_catalog.plugins.is_empty() {
        None
    } else {
        let runtime = Arc::new(crate::extension_runtime::PluginSessionRuntime::compose(
            &executable_catalog.plugins,
            &options.storage_root,
            &workspace_roots,
            &std::env::current_exe().into_diagnostic()?,
            &plugin_redactor,
            &options.plugin_runtime_budget,
        )?);
        for pending in &runtime.pending {
            tracing::warn!("plugin {pending}");
        }
        if !runtime.tools.is_empty() {
            let names = built_tools
                .registry
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.name)
                .collect::<Vec<_>>();
            let mut registry = built_tools
                .registry
                .subset(names.iter().map(String::as_str))
                .map_err(|error| miette!("plugin tool registry could not clone: {error}"))?;
            for tool in &runtime.tools {
                registry
                    .register(Arc::clone(tool))
                    .map_err(|error| miette!("plugin tool could not register: {error}"))?;
            }
            built_tools.registry = Arc::new(registry);
        }
        Some(runtime)
    };
    let plugin_resources =
        crate::session_resources::RuntimeSessionResources::new(None, plugin_runtime.clone());

    let (model, engine_redactor, model_catalog): (
        Arc<dyn ModelDriver>,
        FixtureRedactor,
        Option<Arc<CachedModelCatalog>>,
    ) = match options.provider_mode {
        HostedProviderMode::Live => {
            let pricing = load_effective_pricing_table().await?;
            let extension_providers = plugin_runtime
                .iter()
                .flat_map(|runtime| runtime.providers.iter())
                .map(|(prefix, provider)| (prefix.clone(), Arc::clone(provider)))
                .collect::<Vec<_>>();
            let factory = ProviderFactory::system(options.credentials_path, pricing)
                .with_extension_providers(extension_providers);
            let user_config_path = extension_credentials_path.with_file_name("config.toml");
            let project_config_path = workspace.join(".rottweiler/config.toml");
            // Keep the hosted control plane and its private local transport
            // independent from provider credentials. Provider construction is
            // deliberately deferred until the first model preparation,
            // explicit catalog request, or provider activation. This keeps an
            // idle TUI attach free of provider credential I/O and network work.
            let redactor = fixture_redactor.clone();
            let model = lazy_live_provider_model(
                factory,
                options.config.clone(),
                user_config_path,
                project_config_path,
                persisted_model_alias.clone(),
                redactor.clone(),
                built_tools.websearch.clone(),
            );
            let cache_path = options.storage_root.join("model-catalog.json");
            let initial_catalog = load_model_catalog_cache(&cache_path)
                .ok()
                .flatten()
                .unwrap_or_else(|| ProviderModelCatalogSource::placeholder(&options.config));
            let source: Arc<dyn ModelCatalogSource> = Arc::new(PersistingHostedCatalogSource {
                inner: model.clone(),
                cache_path,
                initial: initial_catalog.clone(),
            });
            (
                model,
                redactor,
                Some(Arc::new(CachedModelCatalog::with_initial(
                    source,
                    Some(initial_catalog),
                ))),
            )
        }
        HostedProviderMode::DeterministicReplay {
            provider_name,
            scripts,
            event_delay_ms,
        } => {
            let provider: Arc<dyn Provider> =
                Arc::new(ScriptProvider::new(provider_name, scripts, event_delay_ms));
            (
                Arc::new(
                    ProviderModel::new(
                        provider,
                        options.config.compaction.clone(),
                        options.config.budget.clone(),
                    )
                    .map_err(display_agent_error)?,
                ),
                fixture_redactor,
                None,
            )
        }
    };
    register_credential_environment(&engine_redactor);
    plugin_redactor.bind(engine_redactor.clone());
    let model: Arc<dyn ModelDriver> = Arc::new(PromptRecordingModel {
        inner: model,
        journal: Arc::clone(&durable_sink.prompt_shapes),
    });
    let model = AliasAwareWebSearchModel::wrap(model, built_tools.websearch.as_ref());
    let instruction_workspace_roots = Arc::new(RwLock::new(workspace_roots.clone()));
    let active_nested_instruction_sources = Arc::new(RwLock::new(BTreeSet::new()));
    let session_tools = Arc::new(OnceLock::new());
    let model: Arc<dyn ModelDriver> = Arc::new(NestedInstructionsModel {
        inner: model,
        tools: Arc::clone(&session_tools),
        workspace_roots: Arc::clone(&instruction_workspace_roots),
        active_sources: Arc::clone(&active_nested_instruction_sources),
        memory_redactor: engine_redactor.clone(),
    });
    let project_approvals = project_approval_path(&options.storage_root, &workspace);
    let permissions = match options.permission_mode {
        Some(mode) => PermissionGate::for_headless_mode(mode),
        None => PermissionGate::from_config(options.config.permissions.clone()),
    }
    .with_workspace_roots(&workspace_roots)
    .with_trusted_read_roots(&trusted_read_roots)
    .with_command_safety(Arc::clone(&command_safety))
    .with_project_approval_file(project_approvals.clone());
    let permissions = Arc::new(permissions);
    let folder_trust = Arc::new(RuntimeFolderTrustController::new(
        options.storage_root.join("trust.json"),
        workspace_roots.clone(),
    ));
    let toolchain_runtime = Arc::new(ToolchainRuntime::new_with_read_only(
        Arc::clone(&built_tools.command_executor),
        Arc::clone(&built_tools.read_only_hook_executor),
        built_tools.read_only_hook_scratch.clone(),
        &workspace_roots,
    ));
    if let Some(index) = skill_index_turn(&extension_catalog)? {
        initial_context.push(index);
    }
    let (_, mut wasm_startup_notifications, validated_wasm_hooks) =
        compose_runtime_hooks_with_extensions_validated(
            Arc::clone(&options.wasm_workers),
            &options.config.toolchain,
            &toolchain_runtime,
            Arc::clone(&built_tools.registry),
            &extension_catalog,
            Arc::clone(&built_tools.code_intelligence),
        )
        .await?;
    wasm_startup_notifications.extend(extension_startup_notifications(&extension_catalog));
    let workspace_root_controller = Arc::new(RuntimeWorkspaceRootController {
        index_pool: Arc::clone(&options.index_pool),
        journal_service: Arc::clone(&options.journal_service),
        checkpoint_root: checkpoint_root(&options.storage_root, &workspace, &session_id),
        storage_root: options.storage_root.clone(),
        question_asker: root_question_asker,
        offline,
        global_proxy: root_global_proxy,
        deferred_global_proxy: root_deferred_global_proxy,
        command_fixture_mode: root_command_fixture_mode,
        execution_lease: root_execution_lease,
        command_safety: root_command_safety,
        websearch_config: root_websearch_config,
        websearch_headers: root_websearch_headers,
        deferred_websearch_headers: root_deferred_websearch_headers,
        background_redactor: root_background_redactor,
        background_manager: Arc::clone(&built_tools.background),
        native_websearch_possible,
        native_websearch_resolver: built_tools
            .websearch
            .as_ref()
            .and_then(|searcher| searcher.native_resolver()),
        trust_store_path: options.storage_root.join("trust.json"),
        toolchain_config: options.config.toolchain.clone(),
        toolchain_runtime: Arc::clone(&toolchain_runtime),
        validated_wasm_hooks,
        extension_user_home,
        extension_user_rottweiler,
        dangerously_trust: options.dangerously_trust,
        instruction_workspace_roots: Arc::clone(&instruction_workspace_roots),
        active_nested_instruction_sources,
        pending_instruction_roots: Mutex::new(HashMap::new()),
        root_authorization: WorkspaceRootAuthorization::Hosted(allowed_workspace_roots.clone()),
    });
    let commands_cell = Arc::new(OnceLock::<Arc<RuntimeCommandRegistry>>::new());
    let actor_event_sink: Arc<dyn SessionEventSink> = if let Some(runtime) = plugin_runtime
        .as_ref()
        .filter(|runtime| !runtime.event_routers.is_empty())
    {
        Arc::new(
            PluginFanoutEventSink::new(
                durable_sink.clone(),
                runtime.event_routers.clone(),
                engine_redactor.clone(),
            )
            .map_err(|error| miette!("plugin event delivery admission: {error}"))?,
        )
    } else {
        durable_sink.clone()
    };
    let secret_redactor: Arc<dyn rw_core::SecretRedactor> =
        Arc::new(SharedEngineSecretRedactor(engine_redactor));
    let mut agents = compose_agent_registry(&extension_catalog)
        .map_err(|error| miette!("agent registry could not compose: {error}"))?;
    for definition in agents.definitions() {
        let Some(explicit_model) = definition.model() else {
            continue;
        };
        if !model.has_model_alias(explicit_model) {
            return Err(miette!(
                "agent {:?} selects unknown model alias {:?}",
                definition.name(),
                explicit_model
            ));
        }
    }
    let mut available_tools = built_tools
        .registry
        .descriptors()
        .into_iter()
        .map(|descriptor| descriptor.name)
        .collect::<Vec<_>>();
    available_tools.push("spawn_agent".to_owned());
    available_tools.push("apply_worktree_diff".to_owned());
    if extension_catalog.workflows().len() > 0 {
        available_tools.push("workflow".to_owned());
    }
    agents
        .resolve_tool_names(available_tools)
        .map_err(|error| miette!("agent tools could not resolve: {error}"))?;
    let agents = Arc::new(agents);
    let template = Arc::new(ChildActorTemplate {
        budget_session_id: budget_session_id.clone(),
        provider_admission: options.provider_admission.clone(),
        storage_root: options.storage_root.clone(),
        model: Arc::clone(&model),
        permissions: Arc::clone(&permissions),
        secret_redactor: Arc::clone(&secret_redactor),
        lease_runtime: Arc::clone(&workspace_root_controller),
        max_turns: options.max_turns,
    });
    let create_template = Arc::clone(&template);
    let factory = ActorSubagentSessionFactory::new(move |launch| create_template.config(launch))
        .with_rebuilder(move |session_id, root, policy| {
            template.rebind_config(session_id, root, policy)
        });
    let shared: Arc<dyn SubagentSessionFactory> = Arc::new(factory);
    let isolation = WorktreeIsolation::new(
        &workspace,
        options.storage_root.join("worktrees"),
        WorktreeLimits::default(),
        CancellationToken::default(),
    )
    .await;
    let (isolated, worktree_manager, isolation_error): (
        Option<Arc<dyn SubagentSessionFactory>>,
        Option<Arc<WorktreeIsolation>>,
        String,
    ) = match isolation {
        Ok(isolation) => {
            let isolation = Arc::new(isolation);
            (
                Some(Arc::new(WorktreeSubagentSessionFactory::new(
                    Arc::clone(&shared),
                    Arc::clone(&isolation),
                ))),
                Some(isolation),
                String::new(),
            )
        }
        Err(error) => (None, None, error.to_string()),
    };
    let factory: Arc<dyn SubagentSessionFactory> = Arc::new(RuntimeSubagentSessionFactory {
        shared,
        isolated,
        isolation_error,
    });
    let orchestrator = SubagentOrchestrator::new(
        SubagentLimits {
            max_depth: options.config.engine.subagent_max_depth,
            max_concurrency: options.config.engine.subagent_max_concurrency,
            max_turns: options.max_turns,
            ..SubagentLimits::default()
        },
        factory,
        Arc::clone(&built_tools.registry),
    )
    .map_err(|error| miette!("subagent orchestrator could not start: {error}"))?;
    let metadata = Arc::new(
        crate::subagent_metadata::PrivateSubagentMetadataStore::open(&options.storage_root)
            .map_err(|error| miette!("subagent metadata could not open: {error}"))?,
    );
    orchestrator.bind_metadata_store(metadata.clone());
    let names = built_tools
        .registry
        .descriptors()
        .into_iter()
        .map(|descriptor| descriptor.name)
        .collect::<Vec<_>>();
    let mut registry = built_tools
        .registry
        .subset(names.iter().map(String::as_str))
        .map_err(|error| miette!("tool registry could not clone: {error}"))?;
    registry
        .register(Arc::new(SpawnAgentTool::new(
            orchestrator.clone(),
            Arc::clone(&agents),
            Arc::clone(&model),
        )))
        .map_err(|error| miette!("spawn_agent tool could not register: {error}"))?;
    registry
        .register(Arc::new(ApplyWorktreeDiffTool::new(
            orchestrator.diff_artifact_authority(),
        )))
        .map_err(|error| miette!("apply_worktree_diff tool could not register: {error}"))?;
    if extension_catalog.workflows().len() > 0 {
        registry
            .register(Arc::new(crate::workflow_runtime::WorkflowTool::new(
                orchestrator.clone(),
                agents,
                Arc::clone(&extension_catalog),
                options.storage_root.clone(),
            )))
            .map_err(|error| miette!("workflow tool could not register: {error}"))?;
    }
    let runtime_tools = Arc::new(registry);
    orchestrator.bind_tools(Arc::clone(&runtime_tools));
    recover_subagent_tree(
        &options.storage_root,
        &options.session_id,
        &durable_sink,
        &recovered_events,
        &allowed_workspace_roots,
        options.config.engine.subagent_max_depth,
        &orchestrator,
        metadata.as_ref(),
        worktree_manager.as_deref(),
    )
    .await
    .map_err(display_agent_error)?;
    session_tools
        .set(Arc::downgrade(&runtime_tools))
        .map_err(|_| miette!("session tool registry was bound more than once"))?;
    let mut runtime_hooks = compose_runtime_hooks_with_extensions(
        &options.config.toolchain,
        &workspace_root_controller.toolchain_runtime,
        Arc::clone(&runtime_tools),
        &extension_catalog,
        Arc::clone(&built_tools.code_intelligence),
        &workspace_root_controller.validated_wasm_hooks,
    )?;
    if let Some(plugins) = &plugin_runtime {
        for (registration, handler) in &plugins.hooks {
            runtime_hooks
                .register_shared(registration.clone(), Arc::clone(handler))
                .map_err(|error| miette!("plugin hook could not register: {error}"))?;
        }
    }
    register_nested_instruction_guard(
        &mut runtime_hooks,
        Arc::clone(&runtime_tools),
        Arc::clone(&instruction_workspace_roots),
        Arc::clone(&workspace_root_controller.active_nested_instruction_sources),
    )?;
    let runtime_hooks = Arc::new(runtime_hooks);
    let mut runtime_commands = compose_runtime_commands(
        &extension_catalog,
        &workspace_roots,
        &options.storage_root,
        &runtime_tools,
    )?;
    if let Some(mcp) = &mcp_runtime {
        crate::extension_runtime::register_mcp_command(
            &mut runtime_commands,
            Arc::clone(&mcp.manager),
            Some(Arc::clone(&mcp.approvals)),
        )
        .await
        .map_err(|error| miette!("MCP command could not register: {error}"))?;
    }
    if let Some(plugins) = &plugin_runtime {
        for (descriptor, handler) in &plugins.commands {
            runtime_commands
                .register_shared(descriptor.clone(), Arc::clone(handler))
                .map_err(|error| miette!("plugin command could not register: {error}"))?;
        }
    }
    let runtime_commands = Arc::new(runtime_commands);
    let _ = commands_cell.set(Arc::clone(&runtime_commands));
    let extension_development: Arc<dyn rw_core::SessionExtensionController> = Arc::new(
        crate::extension_runtime::RuntimeSessionExtensionController::new(
            options.storage_root.clone(),
            std::env::current_exe().into_diagnostic()?,
            Arc::clone(&plugin_redactor),
            Arc::clone(&options.plugin_runtime_budget),
        ),
    );
    let initial_thinking = configured_session_thinking(&options.config, &persisted_model_alias);
    let handle = SessionActor::spawn(SessionActorConfig {
        budget_session_id,
        session_id: options.session_id,
        workspace_root: workspace,
        additional_workspace_roots: workspace_roots.into_iter().skip(1).collect(),
        workspace_generation: recovered
            .workspace_generation
            .max(persisted_workspace_generation),
        initial_session_context: initial_context,
        startup_notifications: wasm_startup_notifications,
        model_alias: persisted_model_alias,
        model,
        tools: runtime_tools,
        permissions,
        hooks: runtime_hooks,
        commands: runtime_commands,
        modes: runtime_modes,
        event_sink: actor_event_sink,
        event_clock: Arc::new(SystemEventClock),
        provider_admission: options.provider_admission.clone(),
        secret_redactor,
        checkpoints: checkpoint_coordinator,
        folder_trust,
        workspace_roots: workspace_root_controller,
        extension_development,
        resources: Arc::new(crate::session_resources::SessionResourcePair([
            mcp_resources,
            plugin_resources,
        ])),
        recovered,
        max_turns: options.max_turns,
        identical_tool_failure_limit: DEFAULT_DOOM_LOOP_LIMIT,
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        thinking: initial_thinking,
        event_capacity: DEFAULT_EVENT_CAPACITY,
    })
    .map_err(display_agent_error)?;
    if let Some(plugins) = &plugin_runtime {
        plugins.bind_push(&handle)?;
    }
    let subagents: Arc<dyn HostSubagentService> = Arc::new(HostedSubagentController {
        parent: handle.clone(),
        orchestrator,
    });
    Ok(HostedActorRuntime {
        handle,
        model_catalog,
        mcp: mcp_admin,
        runtime_services: Arc::new(RuntimeServiceView {
            intelligence: Arc::clone(&built_tools.code_intelligence),
            toolchain: Arc::clone(&toolchain_runtime),
        }),
        subagents,
        model_alias: descriptor_model,
        driver_client_id,
        shell_active,
    })
}
