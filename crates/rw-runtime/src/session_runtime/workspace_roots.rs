use super::checkpoint_journal::abort_checkpoint_root_generation;
use super::checkpoint_journal::append_checkpoint_root_generation;
use super::checkpoint_journal::commit_checkpoint_root_generation;
use super::checkpoint_journal::open_checkpoint_stores;
use super::checkpoints::DurableCheckpointCoordinator;
use super::command_execution::CommandFixtureMode;
use super::credential_resolution::DeferredToolProxy;
use super::credential_resolution::DeferredWebSearchHeaders;
use super::credential_resolution::ResolvedToolProxy;
use super::custom_commands::compose_runtime_commands;
use super::durable_session::DurableEventSink;
use super::extension_discovery::discover_runtime_extensions;
use super::extension_discovery::discover_runtime_extensions_derived;
use super::extension_discovery::skill_index_turn;
use super::folder_trust::RuntimeFolderTrustController;
use super::initial_memory::fresh_initial_session_context;
use super::native_model_generations::ChildNativeModel;
use super::nested_instructions::register_nested_instruction_guard;
use super::runtime_options::DEFAULT_DOOM_LOOP_LIMIT;
use super::runtime_options::DEFAULT_EVENT_CAPACITY;
use super::runtime_options::DEFAULT_MAX_OUTPUT_TOKENS;
use super::runtime_options::MAX_WORKSPACE_ROOTS;
use super::session_selection::checkpoint_root;
use super::tool_composition::BuildToolsInput;
use super::tool_composition::BuiltTools;
use super::tool_composition::build_tools;
use super::tool_composition::trusted_lsp_roots;
use super::toolchain::ToolchainRuntime;
use super::wasm_hooks::NamedWasmHook;
use super::wasm_hooks::compose_runtime_hooks_with_extensions;
use crate::journal_service::JournalService;
use async_trait::async_trait;
use miette::IntoDiagnostic;
use miette::Result;
use miette::miette;
use rw_core::AgentLoopError;
use rw_core::PermissionGate;
use rw_core::SessionActorConfig;
use rw_core::SessionCommandContext;
use rw_core::SessionCommandOutput;
use rw_core::SystemEventClock;
use rw_core::recovery::SessionHistory;
use rw_ext::CommandRegistry;
use rw_ext::HookDispatcher;
use rw_ext::compose_mode_registry;
use rw_store::session::SessionEventLog;
use rw_store::trust::FolderTrustStore;
use rw_tools::BackgroundProcessManager;
use rw_tools::CommandFixtureRedactor;
use rw_tools::CommandSafetyClassifier;
use rw_tools::ExecutionLease;
use rw_tools::QuestionAsker;
use rw_types::SessionId;
use rw_types::Turn;
use rw_types::config::ThinkingLevel;
use rw_types::config::ToolchainConfig;
use rw_types::config::WebSearchConfig;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

pub(super) fn canonical_workspace_roots(
    primary: &Path,
    additional: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let mut roots = vec![std::fs::canonicalize(primary).into_diagnostic()?];
    for supplied in additional {
        let canonical = std::fs::canonicalize(supplied).map_err(|error| {
            miette!(
                "additional workspace {} is unavailable: {error}",
                supplied.display()
            )
        })?;
        if !canonical.is_dir() {
            return Err(miette!(
                "additional workspace {} is not a directory",
                supplied.display()
            ));
        }
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    if roots.len() > MAX_WORKSPACE_ROOTS {
        return Err(miette!(
            "workspace root count exceeds the supported maximum of {MAX_WORKSPACE_ROOTS}"
        ));
    }
    Ok(roots)
}

#[allow(clippy::struct_excessive_bools)]
pub(crate) struct RuntimeWorkspaceRootController {
    pub(super) child_plugins: Arc<crate::extension_runtime::generations::PluginGenerationConfig>,
    pub(crate) native: super::native_registry_recipe::RootNativeBinding,
    pub(super) index_pool: Arc<rw_tools::WorkspaceIndexPool>,
    pub(super) journal_service: Arc<JournalService>,
    pub(super) transcripts: Arc<crate::transcript_service::TranscriptReader>,
    pub(super) checkpoint_root: PathBuf,
    pub(super) storage_root: PathBuf,
    pub(super) question_asker: Arc<dyn QuestionAsker>,
    pub(super) offline: bool,
    pub(super) global_proxy: Option<ResolvedToolProxy>,
    pub(super) deferred_global_proxy: Option<DeferredToolProxy>,
    pub(super) command_fixture_mode: CommandFixtureMode,
    pub(super) execution_lease: Arc<ExecutionLease>,
    pub(super) command_safety: Arc<CommandSafetyClassifier>,
    pub(super) websearch_config: WebSearchConfig,
    pub(super) websearch_headers: BTreeMap<String, String>,
    pub(super) deferred_websearch_headers: Option<DeferredWebSearchHeaders>,
    pub(super) background_redactor: Arc<dyn CommandFixtureRedactor>,
    pub(super) background_manager: Arc<BackgroundProcessManager>,
    pub(super) native_websearch_possible: bool,
    pub(super) trust_store_path: PathBuf,
    pub(super) toolchain_config: ToolchainConfig,
    pub(super) toolchain_runtime: Arc<ToolchainRuntime>,
    pub(super) wasm_hooks: Arc<[NamedWasmHook]>,
    pub(super) extension_user_home: PathBuf,
    pub(super) extension_user_rottweiler: PathBuf,
    pub(super) dangerously_trust: bool,
    pub(super) instruction_workspace_roots: Arc<RwLock<Vec<PathBuf>>>,
    pub(super) active_nested_instruction_sources: Arc<RwLock<BTreeSet<PathBuf>>>,
    pub(super) pending_instruction_roots: Mutex<HashMap<u64, Vec<PathBuf>>>,
    pub(super) root_authorization: WorkspaceRootAuthorization,
}

pub(super) enum WorkspaceRootAuthorization {
    LocalUnrestricted,
    Hosted(Vec<PathBuf>),
}

impl WorkspaceRootAuthorization {
    pub(super) fn allows(&self, root: &Path) -> bool {
        match self {
            Self::LocalUnrestricted => true,
            Self::Hosted(allowed) => allowed
                .iter()
                .any(|authorized| root == authorized || root.starts_with(authorized)),
        }
    }
}

pub(crate) struct PreparedExtensionGeneration {
    pub(crate) hooks: Arc<HookDispatcher>,
    pub(crate) commands: Arc<CommandRegistry<SessionCommandContext, SessionCommandOutput>>,
    pub(crate) modes: Arc<rw_ext::ModeRegistry>,
    pub(crate) skill_index: Option<Turn>,
}

pub(crate) struct PreparedRootGeneration {
    pub(crate) catalog: Arc<rw_ext::ExtensionCatalog>,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) supplemental_context: Vec<Turn>,
    pub(crate) built: BuiltTools,
    pub(crate) permissions: Arc<PermissionGate>,
    pub(crate) extensions: PreparedExtensionGeneration,
}

impl RuntimeWorkspaceRootController {
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(super) async fn child_config(
        &self,
        storage_root: &Path,
        budget_session_id: &SessionId,
        session_id: &SessionId,
        workspace_root: &Path,
        fallback_model_alias: &str,
        model: ChildNativeModel,
        secret_redactor: Arc<dyn rw_core::SecretRedactor>,
        parent_permissions: &PermissionGate,
        max_turns: usize,
        provider_admission: Arc<dyn rw_core::provider_admission::ProviderAdmission>,
    ) -> std::result::Result<SessionActorConfig, AgentLoopError> {
        let roots = vec![workspace_root.to_path_buf()];
        let trusted_roots =
            trusted_lsp_roots(&roots, &self.trust_store_path, self.dangerously_trust).map_err(
                |_error| {
                    AgentLoopError::InvalidConfiguration(
                        "child workspace trust could not be assessed".to_owned(),
                    )
                },
            )?;
        let child_project_trusted = trusted_roots.first().copied().unwrap_or(false);
        let mut built = build_tools(BuildToolsInput {
            index_pool: Arc::clone(&self.index_pool),
            workspace_roots: &roots,
            trusted_lsp_roots: &trusted_roots,
            question_asker: Arc::clone(&self.question_asker),
            offline: self.offline,
            global_proxy: self.global_proxy.as_ref(),
            deferred_global_proxy: self.deferred_global_proxy.clone(),
            command_fixture_mode: self.command_fixture_mode.clone(),
            execution_lease: Arc::clone(&self.execution_lease),
            command_safety: &self.command_safety,
            websearch_config: &self.websearch_config,
            websearch_headers: &self.websearch_headers,
            deferred_websearch_headers: self.deferred_websearch_headers.clone(),
            native_websearch_possible: self.native_websearch_possible,
            background_redactor: Arc::clone(&self.background_redactor),
            background_manager: Some(Arc::clone(&self.background_manager)),
        })
        .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
        let plugins = super::child_plugins::ChildPlugins::compose(
            &self.child_plugins,
            &self.native_configs(&roots)?,
            &roots,
        )?;
        let child_resources = plugins.resources(model.resources.clone());
        plugins.tools(&mut built.registry)?;
        let toolchain_runtime = Arc::new(ToolchainRuntime::new_with_read_only(
            Arc::clone(&built.command_executor),
            Arc::clone(&built.read_only_hook_executor),
            built.read_only_hook_scratch.clone(),
            &roots,
        ));
        let catalog = discover_runtime_extensions_derived(
            workspace_root,
            &self.extension_user_home,
            &self.extension_user_rottweiler,
            child_project_trusted,
        );
        let instruction_roots = Arc::new(RwLock::new(roots.clone()));
        let active_sources = Arc::new(RwLock::new(BTreeSet::new()));
        let mut hooks = compose_runtime_hooks_with_extensions(
            &self.toolchain_config,
            &toolchain_runtime,
            Arc::clone(&built.registry),
            &catalog,
            Arc::clone(&built.code_intelligence),
            &self.wasm_hooks,
        )
        .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
        register_nested_instruction_guard(
            &mut hooks,
            Arc::clone(&built.registry),
            Arc::clone(&instruction_roots),
            Arc::clone(&active_sources),
        )
        .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
        plugins.hooks(&mut hooks)?;
        let mut commands =
            compose_runtime_commands(&catalog, &roots, storage_root, &built.registry)
                .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
        plugins.commands(&mut commands)?;
        let mode_registry = compose_mode_registry(&catalog)
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
        let child_checkpoint_root = checkpoint_root(storage_root, workspace_root, &session_id.0);
        let stores = open_checkpoint_stores(storage_root, &child_checkpoint_root, &roots)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        let log = SessionEventLog::open(storage_root, &session_id.0)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        let event_sink = DurableEventSink::new(
            log,
            storage_root.to_path_buf(),
            session_id.0.clone(),
            Arc::clone(&self.journal_service),
        )
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        let mode_registry = Arc::new(mode_registry);
        // Child lifecycle metadata is authoritative; subagent journals have no
        // inherited fork prefix and do not use parent SessionMetadata files.
        event_sink.configure_canonical(Arc::clone(&mode_registry), None)?;
        let recovered = rw_core::SessionActorRecovery::from_bootstrap(
            event_sink.capture_history().await?.bootstrap().await?,
        )?;
        let mut initial_context = fresh_initial_session_context(storage_root, &roots)
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
        if let Some(index) = skill_index_turn(&catalog)
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?
        {
            initial_context.push(index);
        }
        let permissions = parent_permissions
            .fork_for_workspace_roots(&roots)
            .map(|gate| {
                gate.with_trusted_read_roots(
                    roots
                        .iter()
                        .zip(&trusted_roots)
                        .filter_map(|(root, trusted)| trusted.then_some(root)),
                )
            })
            .map(Arc::new)
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
        let ChildNativeModel {
            compose,
            redactor,
            resources: _parent_lease,
        } = model;
        let provider = compose(plugins.runtime.providers.clone());
        let delivery = plugins.delivery(event_sink.clone(), &redactor)?;
        let resources = child_resources;
        let recorded: Arc<dyn rw_core::ModelDriver> =
            Arc::new(super::prompt_model::PromptRecordingModel {
                inner: provider,
                journal: Arc::clone(&event_sink.prompt_shapes),
            });
        let recorded = super::native_search::AliasAwareWebSearchModel::wrap(
            recorded,
            built.websearch.as_ref(),
        );
        let model: Arc<dyn rw_core::ModelDriver> =
            Arc::new(super::nested_instructions::NestedInstructionsModel {
                inner: recorded,
                tools: Arc::new(std::sync::OnceLock::from(Arc::downgrade(&built.registry))),
                workspace_roots: Arc::clone(&instruction_roots),
                active_sources: Arc::clone(&active_sources),
                memory_redactor: redactor,
            });
        let workspace_controller = Arc::new(RuntimeWorkspaceRootController {
            native: super::native_registry_recipe::RootNativeBinding::CapturedChild,
            child_plugins: self.child_plugins.clone(),
            index_pool: Arc::clone(&self.index_pool),
            journal_service: Arc::clone(&self.journal_service),
            transcripts: Arc::clone(&self.transcripts),
            checkpoint_root: child_checkpoint_root.clone(),
            storage_root: storage_root.to_path_buf(),
            question_asker: Arc::clone(&self.question_asker),
            offline: self.offline,
            global_proxy: self.global_proxy.clone(),
            deferred_global_proxy: self.deferred_global_proxy.clone(),
            command_fixture_mode: self.command_fixture_mode.clone(),
            execution_lease: Arc::clone(&self.execution_lease),
            command_safety: Arc::clone(&self.command_safety),
            websearch_config: self.websearch_config.clone(),
            websearch_headers: self.websearch_headers.clone(),
            deferred_websearch_headers: self.deferred_websearch_headers.clone(),
            background_redactor: Arc::clone(&self.background_redactor),
            background_manager: Arc::clone(&self.background_manager),
            native_websearch_possible: self.native_websearch_possible,
            trust_store_path: self.trust_store_path.clone(),
            toolchain_config: self.toolchain_config.clone(),
            toolchain_runtime,
            wasm_hooks: Arc::clone(&self.wasm_hooks),
            extension_user_home: self.extension_user_home.clone(),
            extension_user_rottweiler: self.extension_user_rottweiler.clone(),
            dangerously_trust: self.dangerously_trust,
            instruction_workspace_roots: instruction_roots,
            active_nested_instruction_sources: active_sources,
            pending_instruction_roots: Mutex::new(HashMap::new()),
            root_authorization: WorkspaceRootAuthorization::Hosted(roots.clone()),
        });
        Ok(SessionActorConfig {
            ui: plugins.runtime.ui.clone(),
            ui_tool_source: Arc::new(crate::extension_runtime::ui::source::ToolSource {
                reader: Arc::clone(&self.transcripts),
                session: session_id.clone(),
            }),
            budget_session_id: budget_session_id.clone(),
            session_id: session_id.clone(),
            workspace_root: workspace_root.to_path_buf(),
            additional_workspace_roots: Vec::new(),
            workspace_generation: recovered.workspace_generation,
            initial_session_context: initial_context,
            startup_notifications: Vec::new(),
            model_alias: recovered
                .model_alias
                .clone()
                .unwrap_or_else(|| fallback_model_alias.to_owned()),
            model,
            tools: built.registry,
            permissions,
            hooks: Arc::new(hooks),
            commands: Arc::new(commands),
            modes: mode_registry,
            history: event_sink.clone(),
            event_sink: delivery,
            event_clock: Arc::new(SystemEventClock),
            provider_admission,
            secret_redactor,
            checkpoints: Arc::new(DurableCheckpointCoordinator::from_stores(
                child_checkpoint_root,
                stores,
            )),
            folder_trust: Arc::new(RuntimeFolderTrustController::new(
                self.trust_store_path.clone(),
                roots,
            )),
            workspace_roots: workspace_controller,
            extension_development: Arc::new(rw_core::NoopSessionExtensionController),
            resources,
            recovered,
            max_turns,
            identical_tool_failure_limit: DEFAULT_DOOM_LOOP_LIMIT,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            thinking: ThinkingLevel::Off,
            event_capacity: DEFAULT_EVENT_CAPACITY,
        })
    }

    pub(crate) fn prepare_tools(
        &self,
        roots: &[PathBuf],
    ) -> std::result::Result<BuiltTools, AgentLoopError> {
        let trusted_lsp_roots =
            trusted_lsp_roots(roots, &self.trust_store_path, self.dangerously_trust).map_err(
                |_error| {
                    AgentLoopError::InvalidConfiguration(
                        "workspace LSP trust could not be assessed".to_owned(),
                    )
                },
            )?;
        let built = build_tools(BuildToolsInput {
            index_pool: Arc::clone(&self.index_pool),
            workspace_roots: roots,
            trusted_lsp_roots: &trusted_lsp_roots,
            question_asker: Arc::clone(&self.question_asker),
            offline: self.offline,
            global_proxy: self.global_proxy.as_ref(),
            deferred_global_proxy: self.deferred_global_proxy.clone(),
            command_fixture_mode: self.command_fixture_mode.clone(),
            execution_lease: Arc::clone(&self.execution_lease),
            command_safety: &self.command_safety,
            websearch_config: &self.websearch_config,
            websearch_headers: &self.websearch_headers,
            deferred_websearch_headers: self.deferred_websearch_headers.clone(),
            native_websearch_possible: self.native_websearch_possible,
            background_redactor: Arc::clone(&self.background_redactor),
            background_manager: Some(Arc::clone(&self.background_manager)),
        })
        .map_err(|_error| {
            AgentLoopError::InvalidConfiguration(
                "workspace tool generation could not prepare".to_owned(),
            )
        })?;
        Ok(built)
    }

    pub(super) fn appended_roots(
        &self,
        requested: &Path,
        current_roots: &[PathBuf],
    ) -> std::result::Result<Vec<PathBuf>, AgentLoopError> {
        if current_roots.len() >= MAX_WORKSPACE_ROOTS {
            return Err(AgentLoopError::InvalidConfiguration(format!(
                "workspace root count is limited to {MAX_WORKSPACE_ROOTS}"
            )));
        }
        let primary_root = current_roots.first().ok_or_else(|| {
            AgentLoopError::InvalidConfiguration(
                "workspace root generation requires an existing root".to_owned(),
            )
        })?;
        let requested = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            primary_root.join(requested)
        };
        let canonical = std::fs::canonicalize(&requested).map_err(|_error| {
            AgentLoopError::InvalidConfiguration(
                "requested workspace root is unavailable".to_owned(),
            )
        })?;
        if !canonical.is_dir() || current_roots.contains(&canonical) {
            return Err(AgentLoopError::InvalidConfiguration(
                "workspace root must be a new canonical directory".to_owned(),
            ));
        }
        if !self.root_authorization.allows(&canonical) {
            return Err(AgentLoopError::InvalidConfiguration(
                "workspace root is outside the host authorization policy".to_owned(),
            ));
        }
        FolderTrustStore::new(self.trust_store_path.clone())
            .assess(&canonical)
            .map_err(|_error| {
                AgentLoopError::InvalidConfiguration(
                    "workspace root trust assessment failed".to_owned(),
                )
            })?;
        let mut roots = current_roots.to_vec();
        roots.push(canonical);
        Ok(roots)
    }

    pub(super) fn prepare_root_generation(
        &self,
        roots: Vec<PathBuf>,
        permissions: &PermissionGate,
    ) -> std::result::Result<PreparedRootGeneration, AgentLoopError> {
        let added_root = roots.last().ok_or_else(|| {
            AgentLoopError::InvalidConfiguration(
                "workspace root generation requires an added root".to_owned(),
            )
        })?;
        let mut supplemental_context = rw_core::load_root_project_instructions(added_root)
            .map_err(|_error| {
                AgentLoopError::InvalidConfiguration(
                    "workspace root instructions could not load".to_owned(),
                )
            })?
            .map(|instructions| vec![instructions.as_system_turn()])
            .unwrap_or_default();
        let built = self.prepare_tools(&roots)?;
        let trusted_roots =
            trusted_lsp_roots(&roots, &self.trust_store_path, self.dangerously_trust).map_err(
                |_error| {
                    AgentLoopError::InvalidConfiguration(
                        "workspace permission trust could not be assessed".to_owned(),
                    )
                },
            )?;
        let permissions = permissions
            .fork_for_workspace_roots(&roots)
            .map_err(|_error| {
                AgentLoopError::Persistence(
                    "workspace permission generation could not prepare".to_owned(),
                )
            })?
            .with_trusted_read_roots(
                roots
                    .iter()
                    .zip(&trusted_roots)
                    .filter_map(|(root, trusted)| trusted.then_some(root)),
            );
        let permissions = Arc::new(permissions);
        let catalog = self.extension_catalog(&roots)?;
        let mut extensions = self.prepare_extensions(&catalog, &roots, &built)?;
        if let Some(index) = extensions.skill_index.take() {
            supplemental_context.push(index);
        }
        Ok(PreparedRootGeneration {
            catalog,
            roots,
            supplemental_context,
            built,
            permissions,
            extensions,
        })
    }

    pub(crate) fn extension_catalog(
        &self,
        roots: &[PathBuf],
    ) -> std::result::Result<Arc<rw_ext::ExtensionCatalog>, AgentLoopError> {
        discover_runtime_extensions(
            roots,
            &self.trust_store_path,
            &self.extension_user_home,
            &self.extension_user_rottweiler,
            self.dangerously_trust,
        )
        .map(Arc::new)
        .map_err(|_| {
            AgentLoopError::InvalidConfiguration(
                "workspace extensions could not be discovered".into(),
            )
        })
    }
    pub(crate) fn native_configs(
        &self,
        roots: &[PathBuf],
    ) -> std::result::Result<Vec<crate::extension_config::DiscoveredPlugin>, AgentLoopError> {
        if self.offline {
            return Ok(Vec::new());
        }
        let primary = roots.first().ok_or_else(|| {
            AgentLoopError::InvalidConfiguration("native discovery requires a workspace".into())
        })?;
        let trusted = self.dangerously_trust
            || FolderTrustStore::new(self.trust_store_path.clone())
                .assess(primary)
                .map_err(|_| {
                    AgentLoopError::InvalidConfiguration(
                        "native extension trust is unavailable".into(),
                    )
                })?
                .project_execution_enabled();
        crate::extension_config::discover_executable_configs(
            &self.extension_user_home,
            primary,
            trusted,
        )
        .map(|catalog| catalog.plugins)
        .map_err(|_| {
            AgentLoopError::InvalidConfiguration(
                "native extension configuration is unavailable".into(),
            )
        })
    }

    pub(crate) fn prepare_extensions(
        &self,
        catalog: &rw_ext::ExtensionCatalog,
        roots: &[PathBuf],
        built: &BuiltTools,
    ) -> std::result::Result<PreparedExtensionGeneration, AgentLoopError> {
        let skill_index = skill_index_turn(catalog).map_err(|_error| {
            AgentLoopError::InvalidConfiguration(
                "workspace skill index could not prepare".to_owned(),
            )
        })?;
        let mut hooks = compose_runtime_hooks_with_extensions(
            &self.toolchain_config,
            &self.toolchain_runtime,
            Arc::clone(&built.registry),
            catalog,
            Arc::clone(&built.code_intelligence),
            &self.wasm_hooks,
        )
        .map_err(|_error| {
            AgentLoopError::InvalidConfiguration(
                "workspace hook generation could not prepare".to_owned(),
            )
        })?;
        register_nested_instruction_guard(
            &mut hooks,
            Arc::clone(&built.registry),
            Arc::clone(&self.instruction_workspace_roots),
            Arc::clone(&self.active_nested_instruction_sources),
        )
        .map_err(|_error| {
            AgentLoopError::InvalidConfiguration(
                "nested instruction guard could not prepare".to_owned(),
            )
        })?;
        let commands =
            compose_runtime_commands(catalog, roots, &self.storage_root, &built.registry).map_err(
                |_error| {
                    AgentLoopError::InvalidConfiguration(
                        "workspace command generation could not prepare".to_owned(),
                    )
                },
            )?;
        let modes = compose_mode_registry(catalog).map_err(|_error| {
            AgentLoopError::InvalidConfiguration(
                "workspace mode generation could not prepare".to_owned(),
            )
        })?;
        Ok(PreparedExtensionGeneration {
            hooks: Arc::new(hooks),
            commands: Arc::new(commands),
            modes: Arc::new(modes),
            skill_index,
        })
    }
}

impl RuntimeWorkspaceRootController {
    fn prepare_checkpoint_generation(
        &self,
        current_roots: &[PathBuf],
        roots: &[PathBuf],
        generation: u64,
        effective_from_turn: u64,
    ) -> std::result::Result<Arc<Vec<Arc<rw_store::checkpoint::CheckpointStore>>>, AgentLoopError>
    {
        append_checkpoint_root_generation(
            &self.checkpoint_root,
            current_roots,
            roots,
            generation,
            effective_from_turn,
        )
        .map_err(|_error| {
            AgentLoopError::Persistence("workspace generation journal could not prepare".to_owned())
        })?;
        match open_checkpoint_stores(&self.storage_root, &self.checkpoint_root, roots) {
            Ok(stores) => Ok(stores),
            Err(_error) => {
                let _ = abort_checkpoint_root_generation(&self.checkpoint_root, generation);
                Err(AgentLoopError::Persistence(
                    "workspace checkpoint generation could not prepare".to_owned(),
                ))
            }
        }
    }

    async fn prepare_native_workspace(
        &self,
        native_owner: Option<Arc<crate::extension_runtime::RuntimeSessionExtensionController>>,
        prepared: &mut PreparedRootGeneration,
        model_alias: &str,
        generation: u64,
    ) -> std::result::Result<Option<rw_core::SessionExtensionSnapshot>, AgentLoopError> {
        if let Some(controller) = native_owner {
            Ok(Some(
                match controller
                    .prepare_workspace(prepared, model_alias, generation)
                    .await
                {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        // Cleanup diagnostics cannot turn failed native retirement
                        // into a recoverable configuration error.
                        if let Err(cleanup) =
                            rw_core::WorkspaceRootController::abort_generation(self, generation)
                                .await
                        {
                            return Err(AgentLoopError::EffectsUnsettled(format!(
                                "{error}; workspace generation rollback failed: {cleanup}"
                            )));
                        }
                        return Err(error);
                    }
                },
            ))
        } else {
            Ok(None)
        }
    }
}

#[async_trait]
impl rw_core::WorkspaceRootController for RuntimeWorkspaceRootController {
    async fn append_root(
        &self,
        request: rw_core::WorkspaceRootRequest<'_>,
    ) -> std::result::Result<rw_core::WorkspaceRuntimeGeneration, AgentLoopError> {
        if matches!(
            self.native,
            super::native_registry_recipe::RootNativeBinding::CapturedChild
        ) {
            return Err(AgentLoopError::InvalidConfiguration(
                "child workspace roots are fixed by their captured invocation authority".into(),
            ));
        }
        let rw_core::WorkspaceRootRequest {
            requested,
            roots: current_roots,
            generation: current_generation,
            effective_from_turn,
            permissions,
            model,
            model_alias,
            mcp_policy,
        } = request;
        let roots = self.appended_roots(requested, current_roots)?;
        let mut prepared = self.prepare_root_generation(roots, &permissions)?;
        prepared.built.registry = Arc::new(
            prepared
                .built
                .registry
                .as_ref()
                .clone()
                .with_mcp_tool_policy(mcp_policy),
        );
        let generation = current_generation.checked_add(1).ok_or_else(|| {
            AgentLoopError::InvalidConfiguration("workspace generation exhausted".into())
        })?;
        let native_owner = match &self.native {
            super::native_registry_recipe::RootNativeBinding::Standalone
            | super::native_registry_recipe::RootNativeBinding::CapturedChild => None,
            super::native_registry_recipe::RootNativeBinding::Session(binding) => Some(
                binding
                    .get()
                    .and_then(std::sync::Weak::upgrade)
                    .ok_or_else(|| {
                        AgentLoopError::InvalidConfiguration(
                            "session root composition is unavailable".into(),
                        )
                    })?,
            ),
        };
        let stores = self.prepare_checkpoint_generation(
            current_roots,
            &prepared.roots,
            generation,
            effective_from_turn,
        )?;
        let native = self
            .prepare_native_workspace(native_owner, &mut prepared, model_alias, generation)
            .await?;
        self.toolchain_runtime.prepare(
            generation,
            Arc::clone(&prepared.built.command_executor),
            Arc::clone(&prepared.built.read_only_hook_executor),
            prepared.built.read_only_hook_scratch.clone(),
            &prepared.roots,
        );
        self.pending_instruction_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(generation, prepared.roots.clone());
        let (model, publication, ui) = native.map_or_else(
            || {
                (
                    model,
                    rw_core::RuntimePublication::Active,
                    Arc::new(rw_core::ui::EmptyUiRegistry) as Arc<dyn rw_core::ui::UiRegistry>,
                )
            },
            |snapshot| (snapshot.model, snapshot.publication, snapshot.ui),
        );
        Ok(rw_core::WorkspaceRuntimeGeneration {
            model,
            publication,
            ui,
            generation,
            effective_from_turn,
            roots: prepared.roots.clone(),
            tools: prepared.built.registry,
            hooks: prepared.extensions.hooks,
            commands: prepared.extensions.commands,
            modes: prepared.extensions.modes,
            permissions: prepared.permissions,
            checkpoints: Arc::new(DurableCheckpointCoordinator::from_stores(
                self.checkpoint_root.clone(),
                stores,
            )),
            folder_trust: Arc::new(RuntimeFolderTrustController::new(
                self.trust_store_path.clone(),
                prepared.roots,
            )),
            supplemental_context: prepared.supplemental_context,
        })
    }

    async fn prepare_commit_generation(
        &self,
        generation: u64,
    ) -> std::result::Result<(), AgentLoopError> {
        commit_checkpoint_root_generation(&self.checkpoint_root, generation).map_err(|_error| {
            AgentLoopError::Persistence("workspace generation marker could not commit".to_owned())
        })
    }

    fn finalize_generation(&self, generation: u64) {
        self.toolchain_runtime.commit(generation);
        if let Some(roots) = self
            .pending_instruction_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&generation)
        {
            *self
                .instruction_workspace_roots
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = roots;
        }
    }

    async fn abort_generation(&self, generation: u64) -> std::result::Result<(), AgentLoopError> {
        self.pending_instruction_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&generation);
        self.toolchain_runtime.abort(generation);
        abort_checkpoint_root_generation(&self.checkpoint_root, generation).map_err(|_error| {
            AgentLoopError::Persistence("workspace generation could not abort".to_owned())
        })
    }
}
