use super::accounting_projection::collect_abandoned_empty_sessions;
use super::accounting_projection::update_one_session_index;
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
use super::native_search::provider_model_for_alias;
use super::native_search::provider_native_search_available;
use super::nested_instructions::NestedInstructionsModel;
use super::nested_instructions::register_nested_instruction_guard;
use super::plugin_event_fanout::PluginFanoutEventSink;
use super::prompt_model::PromptRecordingModel;
use super::prompt_model::historical_tool_registry;
use super::prompt_shapes::PromptShapeProfile;
use super::prompt_shapes::cache_breakpoints_for_hint;
use super::prompt_shapes::validate_historical_prompt_shape;
use super::provider_activation::lazy_live_provider_model;
use super::provider_adapter::ProviderModel;
use super::provider_adapter::configured_session_thinking;
use super::provider_catalog::load_effective_pricing_table;
use super::runtime_options::AbortOnDropTask;
use super::runtime_options::DEFAULT_DOOM_LOOP_LIMIT;
use super::runtime_options::DEFAULT_EVENT_CAPACITY;
use super::runtime_options::DEFAULT_MAX_OUTPUT_TOKENS;
use super::runtime_options::LocalSessionOptions;
use super::runtime_options::LocalSessionPurpose;
use super::runtime_options::display_agent_error;
use super::script_provider::ScriptProvider;
use super::script_provider::load_provider_script;
use super::secret_redaction::SharedCommandFixtureRedactor;
use super::secret_redaction::SharedEngineSecretRedactor;
use super::secret_redaction::register_credential_environment;
use super::session_metadata::load_session_metadata;
use super::session_metadata::persist_session_metadata;
use super::session_metadata::validate_session_id;
use super::session_selection::acquire_shared_execution_lease;
use super::session_selection::checkpoint_root;
use super::session_selection::is_zero_turn_prompt_dump;
use super::session_selection::select_session;
use super::session_selection::workspace_execution_lease_path;
use super::subagent_recovery::recover_subagent_tree;
use super::subagent_runtime::ChildActorTemplate;
use super::subagent_runtime::RuntimeSubagentSessionFactory;
use super::tool_composition::BuildToolsInput;
use super::tool_composition::build_tools;
use super::tool_composition::trusted_lsp_roots;
use super::toolchain::ToolchainRuntime;
use super::wasm_hooks::compose_runtime_hooks_with_extensions;
use super::wasm_hooks::compose_runtime_hooks_with_extensions_validated;
use super::workspace_roots::RuntimeWorkspaceRootController;
use super::workspace_roots::WorkspaceRootAuthorization;
use super::workspace_roots::canonical_workspace_roots;
use crate::journal_service::JournalService;
use crate::storage_root::initialize_private_storage_root;
use miette::IntoDiagnostic;
use miette::Result;
use miette::miette;
use rw_core::ActorSubagentSessionFactory;
use rw_core::ModelDriver;
use rw_core::PermissionGate;
use rw_core::ProviderFactory;
use rw_core::ProviderNativeWebSearchFactory;
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
use rw_providers::CacheBreakpointSupport;
use rw_providers::FixtureRedactor;
use rw_providers::NativeWebSearchCapability;
use rw_providers::Provider;
use rw_providers::Recorder;
use rw_providers::ReplayProvider;
use rw_providers::ToolDefinition;
use rw_providers::deny_outbound_network_for_process;
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
use rw_types::SessionId;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::RwLock;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
/// Composes an owned local conversation or prompt-inspection session.
///
/// # Errors
/// Returns an error when configuration, durable recovery, or composition fails.
pub async fn compose_local_session(options: LocalSessionOptions) -> Result<super::LocalSession> {
    if options.max_turns == 0 {
        return Err(miette!("--max-turns must be greater than zero"));
    }
    let workspace =
        std::fs::canonicalize(std::env::current_dir().into_diagnostic()?).into_diagnostic()?;
    let mut workspace_roots =
        canonical_workspace_roots(&workspace, &options.additional_workspaces)?;
    let mut persisted_workspace_generation = 0_u64;
    if options.permission_mode == Some(PermissionMode::Yolo)
        && workspace == Path::new("/")
        && rustix::process::geteuid().is_root()
    {
        return Err(miette!(
            "--permission-mode yolo is refused for root while the workspace is /"
        ));
    }

    let config_loader = ConfigLoader::from_environment().into_diagnostic()?;
    let config_loader = if options.dangerously_trust {
        config_loader.dangerously_trust_project()
    } else {
        config_loader
    };
    let storage_root = config_loader
        .credentials_path()
        .parent()
        .ok_or_else(|| miette!("configuration root has no parent"))?
        .to_path_buf();
    initialize_private_storage_root(&storage_root).into_diagnostic()?;
    collect_abandoned_empty_sessions(&storage_root)?;
    let journal_service = JournalService::new(&storage_root)?;
    let transcripts =
        crate::transcript_service::TranscriptReader::new(Arc::clone(&journal_service));
    let provider_admission = Arc::new(
        crate::provider_admission::DurableProviderAdmission::open(storage_root.clone())
            .await
            .map_err(|error| miette!("provider accounting authority: {error}"))?,
    );
    let index_pool = Arc::new(rw_tools::WorkspaceIndexPool::default());
    let loaded_config = config_loader.load().into_diagnostic()?;
    for warning in loaded_config.warnings() {
        tracing::warn!("{}", warning.message());
    }

    let session_id = select_session(&storage_root, &workspace, &options)?;
    validate_session_id(&session_id)?;
    let session_exists = journal_service.contains_session(&session_id)?;
    let resuming = (options.resume.is_some() || options.continue_latest) && session_exists;
    if !session_exists
        && (options.resume.is_some()
            || (options.continue_latest && !is_zero_turn_prompt_dump(&options)))
    {
        return Err(miette!("session {session_id:?} does not exist"));
    }
    // Acquiring the event writer is the session-wide ownership boundary. No
    // metadata read/write or checkpoint recovery may happen before it.
    let log = SessionEventLog::open(&storage_root, &session_id)
        .map_err(|error| miette!("session log could not open: {error}"))?;
    if resuming {
        let source = log.read_view();
        let committed = tokio::task::spawn_blocking(move || {
            rw_core::recovery::WorkspaceBootstrap::read(&source)
        })
        .await
        .map_err(|error| miette!("workspace bootstrap worker failed: {error}"))?
        .map_err(|error| miette!("workspace bootstrap failed: {error}"))?;
        if let Some(generation) = preview_persisted_workspace_roots(
            &checkpoint_root(&storage_root, &workspace, &session_id),
            &workspace,
            &workspace_roots,
            committed.generation,
        )? {
            persisted_workspace_generation = generation.generation;
            workspace_roots = generation.roots;
        }
    }
    let (extension_user_home, extension_user_rottweiler) =
        extension_user_roots(&config_loader.credentials_path());
    let extension_catalog = Arc::new(discover_runtime_extensions(
        &workspace_roots,
        &storage_root.join("trust.json"),
        &extension_user_home,
        &extension_user_rottweiler,
        options.dangerously_trust,
    )?);
    let inherited_journal_through = if resuming {
        super::session_metadata::load_session_metadata_any(&storage_root, &session_id)?
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
    if resuming
        && let Some(generation) = restore_persisted_workspace_roots(
            &checkpoint_root(&storage_root, &workspace, &session_id),
            &workspace,
            &workspace_roots,
            persisted_workspace_generation,
        )?
    {
        persisted_workspace_generation = generation.generation;
        workspace_roots = generation.roots;
    }
    // Exact Git-root validation can create private worktree state, so it starts
    // only after persisted mode fingerprints have been accepted.
    let worktree_isolation_task =
        if matches!(&options.purpose, LocalSessionPurpose::Conversation { .. }) {
            let repository_root = workspace.clone();
            let private_root = storage_root.join("worktrees");
            Some(AbortOnDropTask::new(tokio::spawn(async move {
                WorktreeIsolation::new(
                    repository_root,
                    private_root,
                    WorktreeLimits::default(),
                    CancellationToken::default(),
                )
                .await
            })))
        } else {
            None
        };
    let execution_lease_path = workspace_execution_lease_path(&storage_root, &workspace)?;
    // The event writer above already excludes a live process resuming this
    // exact session. If a crashed process's command watchdog is the only
    // remaining lease owner, wait for it to finish killing the command group
    // before checkpoint recovery. Fresh sessions still fail fast when another
    // Rottweiler instance owns the workspace.
    let wait_for_execution_lease = resuming;
    let execution_lease = tokio::task::spawn_blocking(move || {
        acquire_shared_execution_lease(&execution_lease_path, wait_for_execution_lease)
    })
    .await
    .map_err(|error| miette!("execution lease worker failed: {error}"))?
    .map_err(|error| miette!("execution lease could not lock: {error}"))?;

    let configured_model_alias = options
        .model
        .clone()
        .unwrap_or_else(|| loaded_config.config.models.default.clone());
    let (mut initial_context, persisted_model_alias, budget_session_id) = if resuming {
        let metadata = load_session_metadata(&storage_root, &session_id, &workspace)?;
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
        let context = fresh_initial_session_context(&storage_root, &workspace_roots)
            .map_err(|error| miette!("project instructions could not load: {error}"))?;
        persist_session_metadata(
            &storage_root,
            &session_id,
            &workspace,
            &configured_model_alias,
            &context,
            &workspace_roots,
        )?;
        (
            context,
            configured_model_alias.clone(),
            rw_types::SessionId(session_id.clone()),
        )
    };

    let checkpoint_root = checkpoint_root(&storage_root, &workspace, &session_id);
    let checkpoint_stores = open_checkpoint_stores(&checkpoint_root, &workspace_roots)?;
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
    let rewind_checkpoint_root = checkpoint_root.clone();
    let log = tokio::task::spawn_blocking(move || {
        let mut log = log;
        recover_rewind_transactions(&rewind_checkpoint_root, &rewind_stores, &mut log)?;
        Ok::<_, miette::Report>(log)
    })
    .await
    .map_err(|error| miette!("rewind recovery worker failed: {error}"))??;
    let recovered_events = load_session_events(&log)?;
    let recovered = crate::mode_recovery::project(&recovered_events, &runtime_modes)?;
    let durable_sink = DurableEventSink::new(
        log,
        storage_root.clone(),
        session_id.clone(),
        Arc::clone(&journal_service),
    )?;
    durable_sink
        .bind_canonical(Arc::clone(&runtime_modes))
        .await
        .map_err(|error| miette!("canonical recovery failed: {error}"))?;
    if matches!(&options.purpose, LocalSessionPurpose::Conversation { .. }) {
        durable_sink
            .reconcile_provider_attempts(&provider_admission)
            .await?;
    }
    durable_sink.reconcile_accounting(&recovered_events)?;
    let checkpoint_coordinator = Arc::new(DurableCheckpointCoordinator::from_stores(
        checkpoint_root.clone(),
        checkpoint_stores,
    ));

    let prompt_dump_turn = match options.purpose {
        LocalSessionPurpose::Conversation { .. } => None,
        LocalSessionPurpose::PromptDump { turn } => Some(turn),
    };
    let inspection = prompt_dump_turn.is_some();
    let recorded_prompt_shape = if let Some(turn) = prompt_dump_turn.flatten() {
        Some(
            durable_sink
                .prompt_shapes
                .shape_for_turn(turn)?
                .ok_or_else(|| {
                    miette!(
                        "exact request shape is unavailable for historical turn {turn}; its required prompt-shape metadata is missing"
                    )
                })?,
        )
    } else if inspection {
        durable_sink.prompt_shapes.latest_shape()?
    } else {
        None
    };
    let interactive = matches!(
        options.purpose,
        LocalSessionPurpose::Conversation { interactive: true }
    );
    let question_asker: Arc<dyn QuestionAsker> = Arc::new(UnboundQuestionAsker);
    let offline_fixture =
        inspection || options.replay_dir.is_some() || options.in_memory_replay_script.is_some();
    let fixture_redactor = FixtureRedactor::default();
    register_credential_environment(&fixture_redactor);
    let configured_run_model_alias = options
        .model
        .clone()
        .unwrap_or_else(|| persisted_model_alias.clone());
    let command_fixture_mode = if options.record_replay_script.is_some() {
        CommandFixtureMode::Record {
            directory: options
                .replay_dir
                .clone()
                .ok_or_else(|| miette!("--record-replay-script requires --replay-dir"))?,
            redactor: fixture_redactor.clone(),
        }
    } else if let Some(directory) = options.replay_dir.clone() {
        CommandFixtureMode::Replay { directory }
    } else if options.in_memory_replay_script.is_some() {
        CommandFixtureMode::Offline
    } else {
        CommandFixtureMode::Live
    };
    let tool_workspace_roots = workspace_roots.clone();
    let tool_execution_lease = Arc::clone(&execution_lease);
    let proxy_credentials_path = config_loader.credentials_path().clone();
    let deferred_global_proxy = DeferredToolProxy::from_config(
        &loaded_config.config,
        &proxy_credentials_path,
        offline_fixture,
        fixture_redactor.clone(),
    )?;
    let websearch_config = loaded_config.config.websearch.clone();
    let deferred_websearch_headers = DeferredWebSearchHeaders::from_config(
        &websearch_config,
        &proxy_credentials_path,
        offline_fixture,
        fixture_redactor.clone(),
    );
    let global_proxy = None;
    let websearch_headers = BTreeMap::new();
    let root_question_asker = Arc::clone(&question_asker);
    let command_safety = Arc::new(
        CommandSafetyClassifier::new(&loaded_config.config.sandbox.safe_list)
            .map_err(|error| miette!(error))?,
    );
    let tool_command_safety = Arc::clone(&command_safety);
    let root_command_safety = Arc::clone(&command_safety);
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
    let native_websearch_possible = if inspection
        || options.in_memory_replay_script.is_some()
        || options.record_replay_script.is_some()
    {
        false
    } else if let Some(directory) = &options.replay_dir {
        ReplayProvider::recorded_native_web_search_capability(&options.replay_provider, directory)
            .await
            .map_err(|error| miette!("replay capability manifest could not load: {error}"))?
            == NativeWebSearchCapability::Supported
    } else {
        provider_native_search_available(&loaded_config.config)
    };
    let trusted_lsp_roots = trusted_lsp_roots(
        &tool_workspace_roots,
        &storage_root.join("trust.json"),
        options.dangerously_trust,
    )?;
    let trusted_read_roots = workspace_roots
        .iter()
        .zip(&trusted_lsp_roots)
        .filter_map(|(root, trusted)| trusted.then_some(root.clone()))
        .collect::<Vec<_>>();
    let derived_project_trusted = trusted_lsp_roots.first().copied().unwrap_or(false);
    let tool_index_pool = Arc::clone(&index_pool);
    let mut built_tools = tokio::task::spawn_blocking(move || {
        build_tools(BuildToolsInput {
            index_pool: tool_index_pool,
            workspace_roots: &tool_workspace_roots,
            trusted_lsp_roots: &trusted_lsp_roots,
            question_asker,
            offline: offline_fixture,
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

    // The hidden release gate uses an in-memory provider while deliberately
    // exercising production executable discovery. Other offline/replay runs
    // keep executable project configuration inert.
    let executable_catalog =
        if inspection || (offline_fixture && !options.activate_fixture_extensions) {
            crate::extension_config::ExecutableConfigCatalog::default()
        } else {
            let (user_home, _) = extension_user_roots(&config_loader.credentials_path());
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
    let mcp_runtime = (if executable_catalog.mcp_servers.is_empty() {
        None
    } else {
        let session_root = storage_root.join("sessions").join(&session_id);
        let runtime = crate::extension_runtime::McpSessionRuntime::start_production(
            &executable_catalog.mcp_servers,
            &workspace_roots,
            &session_root,
            &std::env::current_exe().into_diagnostic()?,
            &config_loader.credentials_path(),
            root_global_proxy
                .as_ref()
                .map(|proxy| proxy.upstream.clone()),
        )
        .await?;
        let mut registry = built_tools
            .registry
            .subset(
                built_tools
                    .registry
                    .descriptors()
                    .iter()
                    .map(|descriptor| descriptor.name.as_str()),
            )
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
    })
    .map(Arc::new);

    let mcp_resources =
        crate::session_resources::RuntimeSessionResources::new(mcp_runtime.clone(), None);
    let inspection_profile = if inspection {
        Some(recorded_prompt_shape.as_ref().map_or_else(
            || {
                let candidate = loaded_config
                    .config
                    .models
                    .aliases
                    .get(&configured_run_model_alias)
                    .and_then(|candidates| candidates.first())
                    .ok_or_else(|| {
                        miette!(
                            "exact request shape is unavailable: model alias {:?} has no candidate",
                            configured_run_model_alias
                        )
                    })?;
                let (provider_name, _) = candidate.split_once('/').ok_or_else(|| {
                    miette!("exact request shape is unavailable: candidate is not provider-qualified")
                })?;
                let kind = loaded_config
                    .config
                    .providers
                    .get(provider_name)
                    .ok_or_else(|| {
                        miette!(
                            "exact request shape is unavailable: provider {provider_name:?} is not configured"
                        )
                    })?
                    .kind
                    .as_str();
                let cache_support = match kind {
                    "anthropic" => CacheBreakpointSupport::Explicit,
                    "openai" | "openai_chat" | "openai_codex" => {
                        CacheBreakpointSupport::Automatic
                    }
                    "github_copilot" | "openai_compatible"
                    | "openai_compatible_responses" => CacheBreakpointSupport::None,
                    _ => {
                        return Err(miette!(
                            "exact request shape is unavailable: unsupported provider kind {kind:?}"
                        ));
                    }
                };
                Ok(PromptShapeProfile {
                    model_alias: configured_run_model_alias.clone(),
                    tools: built_tools
                        .registry
                        .descriptors()
                        .into_iter()
                        .map(|tool| ToolDefinition {
                            name: tool.name,
                            description: tool.description,
                            input_schema: tool.input_schema,
                        })
                        .collect(),
                    cache_support,
                    cache_hint: None,
                    cache_breakpoints: cache_breakpoints_for_hint(None, cache_support),
                })
            },
            |(profile, _)| Ok(profile.clone()),
        )?)
    } else {
        None
    };
    let mut actor_tools = inspection_profile.as_ref().map_or_else(
        || Ok(Arc::clone(&built_tools.registry)),
        historical_tool_registry,
    )?;

    let model_alias = inspection_profile.as_ref().map_or_else(
        || configured_run_model_alias.clone(),
        |profile| profile.model_alias.clone(),
    );
    let _network_denial = (inspection
        || (options.replay_dir.is_some() && options.record_replay_script.is_none())
        || options.in_memory_replay_script.is_some())
    .then(deny_outbound_network_for_process);
    let session_ui = Arc::new(crate::extension_runtime::ui::UiSessionBudget::default());
    let plugin_redactor = Arc::new(crate::extension_runtime::SharedPluginRedactor::new(
        fixture_redactor.clone(),
    ));
    let plugin_runtime_budget = Arc::new(crate::extension_runtime::PluginRuntimeBudget::default());
    let plugin_runtime = (if executable_catalog.plugins.is_empty() || inspection {
        None
    } else {
        let runtime = crate::extension_runtime::PluginSessionRuntime::compose(
            &executable_catalog.plugins,
            &storage_root,
            &workspace_roots,
            &std::env::current_exe().into_diagnostic()?,
            &plugin_redactor,
            &plugin_runtime_budget,
            Arc::clone(&session_ui),
        )?;
        for pending in &runtime.pending {
            tracing::warn!("plugin {pending}");
        }
        if !runtime.tools.is_empty() {
            let names = actor_tools
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.name)
                .collect::<Vec<_>>();
            let mut registry = actor_tools
                .subset(names.iter().map(String::as_str))
                .map_err(|error| miette!("plugin tool registry could not clone: {error}"))?;
            for tool in &runtime.tools {
                registry
                    .register(Arc::clone(tool))
                    .map_err(|error| miette!("plugin tool could not register: {error}"))?;
            }
            actor_tools = Arc::new(registry);
        }
        Some(runtime)
    })
    .map(Arc::new);
    let plugin_resources =
        crate::session_resources::RuntimeSessionResources::new(None, plugin_runtime.clone());
    let (model, engine_redactor): (Arc<dyn ModelDriver>, FixtureRedactor) = if inspection {
        let cache_support = inspection_profile
            .as_ref()
            .map_or(CacheBreakpointSupport::None, |profile| {
                profile.cache_support
            });
        let scripted: Arc<dyn Provider> = Arc::new(
            ScriptProvider::new("prompt-dump-offline".to_owned(), Vec::new(), 0)
                .with_cache_support(cache_support),
        );
        (
            Arc::new(
                ProviderModel::new(
                    scripted,
                    loaded_config.config.compaction.clone(),
                    loaded_config.config.budget.clone(),
                )
                .map_err(display_agent_error)?,
            ),
            fixture_redactor.clone(),
        )
    } else if let Some(script_path) = &options.in_memory_replay_script {
        let script = load_provider_script(script_path)?;
        let scripted: Arc<dyn Provider> = Arc::new(ScriptProvider::new(
            options.replay_provider.clone(),
            script,
            0,
        ));
        (
            Arc::new(
                ProviderModel::new(
                    scripted,
                    loaded_config.config.compaction.clone(),
                    loaded_config.config.budget.clone(),
                )
                .map_err(display_agent_error)?,
            ),
            fixture_redactor.clone(),
        )
    } else if let Some(script_path) = &options.record_replay_script {
        let directory = options
            .replay_dir
            .as_ref()
            .ok_or_else(|| miette!("--record-replay-script requires --replay-dir"))?;
        let script = load_provider_script(script_path)?;
        let scripted: Arc<dyn Provider> = Arc::new(ScriptProvider::new(
            options.replay_provider.clone(),
            script,
            options.record_script_delay_ms,
        ));
        let recorder: Arc<dyn Provider> =
            Arc::new(Recorder::new(scripted, directory, fixture_redactor.clone()));
        (
            Arc::new(
                ProviderModel::new(
                    recorder,
                    loaded_config.config.compaction.clone(),
                    loaded_config.config.budget.clone(),
                )
                .map_err(display_agent_error)?,
            ),
            fixture_redactor.clone(),
        )
    } else if let Some(directory) = &options.replay_dir {
        let replay: Arc<dyn Provider> = Arc::new(
            ReplayProvider::load(&options.replay_provider, directory)
                .await
                .map_err(|error| miette!("replay provider could not load: {error}"))?,
        );
        if let Some(searcher) = &built_tools.websearch {
            let provider = Arc::clone(&replay);
            let config = loaded_config.config.clone();
            let provider_name = options.replay_provider.clone();
            searcher.bind_native_resolver(Some(Arc::new(move |alias| {
                let model = provider_model_for_alias(&config, alias, &provider_name)?;
                ProviderNativeWebSearchFactory::single(Arc::clone(&provider), model)
                    .ok()
                    .flatten()
            })));
        }
        (
            Arc::new(
                ProviderModel::new(
                    replay,
                    loaded_config.config.compaction.clone(),
                    loaded_config.config.budget.clone(),
                )
                .map_err(display_agent_error)?,
            ),
            fixture_redactor.clone(),
        )
    } else {
        let pricing = load_effective_pricing_table().await?;
        let factory = ProviderFactory::system(config_loader.credentials_path(), pricing)
            .with_extension_providers(
                plugin_runtime
                    .iter()
                    .flat_map(|runtime| runtime.providers.iter())
                    .map(|(prefix, provider)| (prefix.clone(), Arc::clone(provider))),
            );
        // Line/headless startup follows the same lazy provider boundary as the
        // hosted TUI. Merely reaching an idle prompt must never touch the OS
        // credential vault; the first actual model use or explicit provider
        // activation is the authorization boundary.
        let redactor = fixture_redactor.clone();
        let model = lazy_live_provider_model(
            factory,
            loaded_config.config.clone(),
            config_loader
                .credentials_path()
                .with_file_name("config.toml"),
            workspace.join(".rottweiler/config.toml"),
            persisted_model_alias.clone(),
            redactor.clone(),
            built_tools.websearch.clone(),
        );
        (model, redactor)
    };
    register_credential_environment(&engine_redactor);
    plugin_redactor.bind(engine_redactor.clone());
    let model: Arc<dyn ModelDriver> = if inspection {
        model
    } else {
        Arc::new(PromptRecordingModel {
            inner: model,
            journal: Arc::clone(&durable_sink.prompt_shapes),
        })
    };
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
    let project_approvals = project_approval_path(&storage_root, &workspace);
    let permissions = match options.permission_mode {
        Some(mode) => PermissionGate::for_headless_mode(mode),
        None => PermissionGate::from_config(loaded_config.config.permissions.clone()),
    }
    .with_workspace_roots(&workspace_roots)
    .with_trusted_read_roots(&trusted_read_roots)
    .with_command_safety(Arc::clone(&command_safety))
    .with_project_approval_file(project_approvals.clone());
    let permissions = Arc::new(permissions);
    let folder_trust = Arc::new(RuntimeFolderTrustController::new(
        storage_root.join("trust.json"),
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
    let wasm_workers = rw_ext::WasmWorkerPool::new();
    let (_, mut wasm_startup_notifications, validated_wasm_hooks) =
        compose_runtime_hooks_with_extensions_validated(
            Arc::clone(&wasm_workers),
            &loaded_config.config.toolchain,
            &toolchain_runtime,
            Arc::clone(&built_tools.registry),
            &extension_catalog,
            Arc::clone(&built_tools.code_intelligence),
        )
        .await?;
    wasm_startup_notifications.extend(extension_startup_notifications(&extension_catalog));
    let workspace_root_controller = Arc::new(RuntimeWorkspaceRootController {
        index_pool: Arc::clone(&index_pool),
        journal_service: Arc::clone(&journal_service),
        transcripts: Arc::clone(&transcripts),
        checkpoint_root,
        storage_root: storage_root.clone(),
        question_asker: root_question_asker,
        offline: offline_fixture,
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
        trust_store_path: storage_root.join("trust.json"),
        toolchain_config: loaded_config.config.toolchain.clone(),
        toolchain_runtime,
        validated_wasm_hooks,
        extension_user_home,
        extension_user_rottweiler,
        dangerously_trust: options.dangerously_trust,
        instruction_workspace_roots: Arc::clone(&instruction_workspace_roots),
        active_nested_instruction_sources,
        pending_instruction_roots: Mutex::new(HashMap::new()),
        root_authorization: WorkspaceRootAuthorization::LocalUnrestricted,
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
                &engine_redactor,
            )
            .map_err(|error| miette!("plugin event delivery admission: {error}"))?,
        )
    } else {
        durable_sink.clone()
    };
    let secret_redactor: Arc<dyn rw_core::SecretRedactor> =
        Arc::new(SharedEngineSecretRedactor(engine_redactor));
    let runtime_tools = if inspection {
        Arc::clone(&actor_tools)
    } else {
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
        let mut available_tools = actor_tools
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
            provider_admission: provider_admission.clone(),
            storage_root: storage_root.clone(),
            model: Arc::clone(&model),
            permissions: Arc::clone(&permissions),
            secret_redactor: Arc::clone(&secret_redactor),
            lease_runtime: Arc::clone(&workspace_root_controller),
            max_turns: options.max_turns,
        });
        let create_template = Arc::clone(&template);
        let factory =
            ActorSubagentSessionFactory::new(move |launch| create_template.config(launch))
                .with_rebuilder(move |session_id, root, policy| {
                    template.rebind_config(session_id, root, policy)
                });
        let shared: Arc<dyn SubagentSessionFactory> = Arc::new(factory);
        let isolation = match worktree_isolation_task {
            Some(task) => match task.join().await {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(error) => Err(format!("worktree validation worker failed: {error}")),
            },
            None => Err("worktree validation was not started".to_owned()),
        };
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
            Err(error) => (None, None, error),
        };
        let factory: Arc<dyn SubagentSessionFactory> = Arc::new(RuntimeSubagentSessionFactory {
            shared,
            isolated,
            isolation_error,
        });
        let orchestrator = SubagentOrchestrator::new(
            SubagentLimits {
                max_depth: loaded_config.config.engine.subagent_max_depth,
                max_concurrency: loaded_config.config.engine.subagent_max_concurrency,
                max_turns: options.max_turns,
                ..SubagentLimits::default()
            },
            factory,
            Arc::clone(&actor_tools),
        )
        .map_err(|error| miette!("subagent orchestrator could not start: {error}"))?;
        let metadata = Arc::new(
            crate::subagent_metadata::PrivateSubagentMetadataStore::open(&storage_root)
                .map_err(|error| miette!("subagent metadata could not open: {error}"))?,
        );
        orchestrator.bind_metadata_store(metadata.clone());
        let parent_session = SessionId(session_id.clone());
        let names = actor_tools
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>();
        let mut registry = actor_tools
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
                    storage_root.clone(),
                )))
                .map_err(|error| miette!("workflow tool could not register: {error}"))?;
        }
        let registry = Arc::new(registry);
        orchestrator.bind_tools(Arc::clone(&registry));
        recover_subagent_tree(
            &storage_root,
            &parent_session,
            &durable_sink,
            &recovered_events,
            &workspace_roots,
            loaded_config.config.engine.subagent_max_depth,
            &orchestrator,
            metadata.as_ref(),
            worktree_manager.as_deref(),
        )
        .await
        .map_err(display_agent_error)?;
        registry
    };
    session_tools
        .set(Arc::downgrade(&runtime_tools))
        .map_err(|_| miette!("session tool registry was bound more than once"))?;
    let mut runtime_hooks = compose_runtime_hooks_with_extensions(
        &loaded_config.config.toolchain,
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
        &storage_root,
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
    let extension_development: Arc<dyn rw_core::SessionExtensionController> = if inspection {
        Arc::new(rw_core::NoopSessionExtensionController)
    } else {
        Arc::new(
            crate::extension_runtime::RuntimeSessionExtensionController::new(
                storage_root.clone(),
                std::env::current_exe().into_diagnostic()?,
                Arc::clone(&plugin_redactor),
                Arc::clone(&plugin_runtime_budget),
                Arc::clone(&session_ui),
            ),
        )
    };
    let initial_thinking = configured_session_thinking(&loaded_config.config, &model_alias);
    let actor = SessionActor::spawn(SessionActorConfig {
        ui: plugin_runtime.as_ref().map_or_else(
            || Arc::new(rw_core::ui::EmptyUiRegistry) as Arc<dyn rw_core::ui::UiRegistry>,
            |plugins| plugins.ui.clone(),
        ),
        ui_tool_source: Arc::new(crate::extension_runtime::ui::source::ToolSource {
            reader: Arc::clone(&transcripts),
            session: SessionId(session_id.clone()),
        }),
        budget_session_id,
        session_id: SessionId(session_id.clone()),
        workspace_root: workspace,
        additional_workspace_roots: workspace_roots.into_iter().skip(1).collect(),
        workspace_generation: recovered
            .workspace_generation
            .max(persisted_workspace_generation),
        initial_session_context: initial_context,
        startup_notifications: wasm_startup_notifications,
        model_alias,
        model,
        tools: runtime_tools,
        permissions,
        hooks: runtime_hooks,
        commands: runtime_commands,
        modes: runtime_modes,
        event_sink: actor_event_sink,
        event_clock: Arc::new(SystemEventClock),
        provider_admission: provider_admission.clone(),
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
    let final_sink = Arc::clone(&durable_sink);
    let final_root = storage_root.clone();
    let final_session = session_id.clone();
    let lifetime = super::headless_lifetime::own(
        actor.clone(),
        Arc::clone(&plugin_runtime_budget),
        Arc::clone(&wasm_workers),
        Arc::clone(&provider_admission),
        Arc::clone(&journal_service),
        async move {
            let indexed = if interactive {
                update_one_session_index(&final_root, &final_session, &final_sink)
            } else {
                Ok(())
            };
            indexed.map_err(|error| Arc::<str>::from(error.to_string()))
        },
    );
    let prepared = async {
        if let Some(plugins) = &plugin_runtime {
            plugins.bind_push(&actor)?;
        }
        let Some(turn) = prompt_dump_turn else {
            return Ok::<_, miette::Report>(None);
        };
        let dump = actor
            .dump_prompt(turn.map(|turn| rw_core::TurnId(turn.to_string())))
            .await
            .map_err(display_agent_error)?;
        if let (Some(_), Some((profile, record))) = (turn, &recorded_prompt_shape) {
            let tools = dump
                .tools
                .iter()
                .map(|tool| ToolDefinition {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                })
                .collect::<Vec<_>>();
            validate_historical_prompt_shape(&dump, &tools, profile, record)?;
        }
        Ok(Some(dump))
    }
    .await;
    match prepared {
        Ok(dump) => Ok(super::LocalSession::new(
            actor,
            session_id,
            storage_root,
            dump,
            lifetime,
        )),
        Err(error) => {
            rw_core::SessionResources::shutdown(lifetime.as_ref())
                .await
                .map_err(display_agent_error)?;
            Err(error)
        }
    }
}
