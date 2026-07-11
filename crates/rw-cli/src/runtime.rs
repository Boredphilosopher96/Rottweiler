use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures_util::StreamExt;
use miette::{IntoDiagnostic, Result, miette};
use rustyline::{DefaultEditor, error::ReadlineError};
use rw_core::runtime_support::{
    ApprovalBinding, ApprovalDecision, AskUserInput, AskUserTool, BashTool, Block, BoxEventStream,
    CacheBreakpointSupport, CacheHint, CancellationToken, Capabilities, CapabilityManifest,
    CommandFixtureRedactor, CommandSafetyClassifier, EditTool, EgressDecision, EgressPin,
    EgressPolicy, ExecutionLease, FetchRequest, FetchResponse, FixtureRedactor, GlobTool, GrepTool,
    GuardedHttpFetchError, GuardedHttpFetchRequest, LsTool, MultiEditTool, MutationScope,
    PricingTable, Provider, ProviderError, ProviderErrorKind, ProviderEvent, ProviderRequest,
    ProxyEnvironment, ProxySettings, QuestionAsker, ReadTool, Recorder, RecordingCommandExecutor,
    ReplayCommandExecutor, ReplayProvider, SandboxNetworkPolicy, SandboxPolicy, SandboxSupport,
    SessionId, SupervisedEgressProxy, SymbolsTool, ThinkingLevel, TodoTool, TokioCommandExecutor,
    Tool, ToolCapability, ToolChoice, ToolContext, ToolDefinition, ToolDescriptor, ToolError,
    ToolLimits, ToolOutput, ToolRegistry, ToolResult, Turn, UpstreamProxy, WebFetchTool,
    WebFetcher, WireMode, WorkspaceSymbolIndex, WriteTool, deny_outbound_network_for_process,
    guarded_http_fetch, probe_policy_egress,
};
use rw_core::{
    AccountingAttribution, AgentLoopError, BudgetLedgerQuery, BudgetLedgerTotals, Config,
    EngineEvent, EventClock, EventMeta, FolderTrustController, FolderTrustOperation,
    MessageDisposition, ModelDriver, MutationCheckpoint, MutationCheckpointCoordinator,
    MutationCheckpointOutcome, PermissionGate, ProviderFactory, QuestionId, RewindCheckpoint,
    SESSION_EVENT_VERSION, SequenceId, SessionActor, SessionActorConfig, SessionEventSink,
    SystemEventClock, ToolOutputStream, TurnStatus, UnrestorablePath, Usage,
    builtin_command_registry, builtin_hook_dispatcher, initial_session_context,
    project_session_events,
};
use rw_store::{
    checkpoint::{CheckpointStore, OpaqueMutation, RewindHandle},
    config::ConfigLoader,
    credentials::{CredentialManager, CredentialReference},
    session::{
        AccountingLedger, SessionEventLog, SessionIndex, SessionProjection, SessionSummary,
        TurnAccountingEntry, UtcTimestamp,
    },
    trust::FolderTrustStore,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use url::{Host, Url};

use crate::{OutputFormat, PermissionMode};

const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8_192;
const DEFAULT_EVENT_CAPACITY: usize = 1_024;
const DEFAULT_DOOM_LOOP_LIMIT: usize = 5;
const MAX_REDIRECTS: usize = 5;
const SESSION_METADATA_VERSION: u16 = 1;
const PROMPT_SHAPE_VERSION: u16 = 2;
const CHECKPOINT_ROOTS_VERSION: u16 = 1;

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CheckpointRootMapping {
    version: u16,
    generations: Vec<CheckpointRootGeneration>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CheckpointRootGeneration {
    generation: u64,
    effective_from_turn: u64,
    roots: Vec<PathBuf>,
    committed: bool,
}

pub(crate) struct RunOptions {
    pub prompt: Option<String>,
    pub output_format: OutputFormat,
    pub permission_mode: Option<PermissionMode>,
    pub max_turns: usize,
    pub resume: Option<String>,
    pub continue_latest: bool,
    pub replay_dir: Option<PathBuf>,
    pub record_replay_script: Option<PathBuf>,
    pub in_memory_replay_script: Option<PathBuf>,
    pub record_script_delay_ms: u64,
    pub perf_markers: bool,
    pub replay_provider: String,
    pub model: Option<String>,
    pub additional_workspaces: Vec<PathBuf>,
    pub dangerously_trust: bool,
    pub action: RunAction,
}

pub(crate) enum RunAction {
    Agent,
    PromptDump { turn: Option<u64> },
}

#[derive(Clone)]
#[allow(dead_code)]
pub(crate) enum HostedProviderMode {
    Live,
    DeterministicReplay {
        provider_name: String,
        scripts: Vec<Vec<ProviderEvent>>,
    },
}

pub(crate) struct HostedSessionComposition {
    pub workspace: PathBuf,
    pub additional_workspaces: Vec<PathBuf>,
    pub storage_root: PathBuf,
    pub credentials_path: PathBuf,
    pub config: Config,
    pub session_id: SessionId,
    pub requested_model: Option<String>,
    pub resume: bool,
    pub permission_mode: Option<PermissionMode>,
    pub max_turns: usize,
    pub provider_mode: HostedProviderMode,
}

pub(crate) struct HostedActorRuntime {
    pub handle: rw_core::SessionHandle,
    pub model_alias: String,
    pub driver_client_id: Option<rw_core::ClientId>,
    pub shell_active: bool,
}

fn canonical_workspace_roots(primary: &Path, additional: &[PathBuf]) -> Result<Vec<PathBuf>> {
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
    Ok(roots)
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn run(options: RunOptions) -> Result<()> {
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
    std::fs::create_dir_all(&storage_root).into_diagnostic()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&storage_root, std::fs::Permissions::from_mode(0o700))
            .into_diagnostic()?;
    }
    let loaded_config = config_loader.load().into_diagnostic()?;
    for warning in loaded_config.warnings() {
        eprintln!("warning: {}", warning.message());
    }

    let session_id = select_session(&storage_root, &workspace, &options)?;
    validate_session_id(&session_id)?;
    let session_exists = storage_root
        .join("sessions")
        .join(&session_id)
        .join("events.jsonl")
        .is_file();
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
        let committed = project_session_events(&load_session_events(&log)?)
            .map_err(|error| miette!("session root projection failed: {error}"))?;
        if let Some(generation) = restore_persisted_workspace_roots(
            &checkpoint_root(&storage_root, &workspace, &session_id),
            &workspace,
            &workspace_roots,
            committed.workspace_generation,
        )? {
            persisted_workspace_generation = generation.generation;
            workspace_roots = generation.roots;
        }
    }
    let execution_lease_path = storage_root
        .join("sessions")
        .join(&session_id)
        .join("execution.lock");
    let execution_lease = Arc::new(
        tokio::task::spawn_blocking(move || ExecutionLease::acquire(execution_lease_path))
            .await
            .map_err(|error| miette!("execution lease worker failed: {error}"))?
            .map_err(|error| miette!("execution lease could not lock: {error}"))?,
    );

    let configured_model_alias = options
        .model
        .clone()
        .unwrap_or_else(|| loaded_config.config.models.default.clone());
    let (initial_context, persisted_model_alias) = if resuming {
        let metadata = load_session_metadata(&storage_root, &session_id, &workspace)?;
        let mut context = metadata.initial_session_context;
        let recorded_count = metadata.workspace_roots.len().max(1);
        for root in workspace_roots.iter().skip(recorded_count) {
            if let Some(instructions) = rw_core::load_root_project_instructions(root)
                .map_err(|error| miette!("project instructions could not load: {error}"))?
            {
                context.push(instructions.as_system_turn());
            }
        }
        (context, metadata.model_alias)
    } else {
        let context = initial_session_context(&workspace)
            .map_err(|error| miette!("project instructions could not load: {error}"))?;
        persist_session_metadata(
            &storage_root,
            &session_id,
            &workspace,
            &configured_model_alias,
            &context,
            &workspace_roots,
        )?;
        (context, configured_model_alias.clone())
    };

    let checkpoint_root = checkpoint_root(&storage_root, &workspace, &session_id);
    let checkpoint_stores = open_checkpoint_stores(&checkpoint_root, &workspace_roots)?;
    let recovery_stores = Arc::clone(&checkpoint_stores);
    tokio::task::spawn_blocking(move || {
        for store in recovery_stores.iter() {
            store.recover_opaque_mutations()?;
        }
        Ok::<_, rw_store::checkpoint::CheckpointError>(())
    })
    .await
    .map_err(|error| miette!("checkpoint recovery worker failed: {error}"))?
    .map_err(|error| miette!("checkpoint recovery failed: {error}"))?;

    let rewind_stores = Arc::clone(&checkpoint_stores);
    let log = tokio::task::spawn_blocking(move || {
        let mut log = log;
        recover_rewind_transactions(&rewind_stores, &mut log)?;
        Ok::<_, miette::Report>(log)
    })
    .await
    .map_err(|error| miette!("rewind recovery worker failed: {error}"))??;
    let recovered_events = load_session_events(&log)?;
    let recovered = project_session_events(&recovered_events)
        .map_err(|error| miette!("session log projection failed: {error}"))?;
    let durable_sink = Arc::new(DurableEventSink::new(
        log,
        storage_root.clone(),
        session_id.clone(),
    )?);
    durable_sink.reconcile_accounting(&recovered_events)?;
    let checkpoint_coordinator =
        Arc::new(DurableCheckpointCoordinator::from_stores(checkpoint_stores));

    let prompt_dump_turn = match options.action {
        RunAction::Agent => None,
        RunAction::PromptDump { turn } => Some(turn),
    };
    let inspection = prompt_dump_turn.is_some();
    let recorded_prompt_shape = if let Some(turn) = prompt_dump_turn.flatten() {
        Some(
            durable_sink
                .prompt_shapes
                .shape_for_turn(turn)?
                .ok_or_else(|| {
                    miette!(
                        "exact request shape is unavailable for historical turn {turn}; the session predates prompt-shape recording or its metadata is missing"
                    )
                })?,
        )
    } else if inspection {
        durable_sink.prompt_shapes.latest_shape()?
    } else {
        None
    };
    let interactive = !inspection && options.prompt.is_none();
    let question_asker: Arc<dyn QuestionAsker> = Arc::new(HeadlessQuestionAsker);
    let offline_fixture =
        inspection || options.replay_dir.is_some() || options.in_memory_replay_script.is_some();
    let fixture_redactor = FixtureRedactor::default();
    register_credential_environment(&fixture_redactor);
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
    let proxy_config = loaded_config.config.clone();
    let proxy_credentials_path = config_loader.credentials_path().clone();
    let proxy_redactor = fixture_redactor.clone();
    let global_proxy = tokio::task::spawn_blocking(move || {
        resolve_tool_proxy(
            &proxy_config,
            &proxy_credentials_path,
            offline_fixture,
            &proxy_redactor,
        )
    })
    .await
    .map_err(|error| miette!("tool proxy credential worker failed: {error}"))??;
    let root_question_asker = Arc::clone(&question_asker);
    let command_safety = Arc::new(
        CommandSafetyClassifier::new(&loaded_config.config.sandbox.safe_list)
            .map_err(|error| miette!(error))?,
    );
    let tool_command_safety = Arc::clone(&command_safety);
    let root_command_safety = Arc::clone(&command_safety);
    let root_command_fixture_mode = command_fixture_mode.clone();
    let root_global_proxy = global_proxy.clone();
    let root_execution_lease = Arc::clone(&execution_lease);
    let built_tools = tokio::task::spawn_blocking(move || {
        build_tools(
            &tool_workspace_roots,
            question_asker,
            offline_fixture,
            global_proxy.as_ref(),
            command_fixture_mode,
            tool_execution_lease,
            &tool_command_safety,
        )
    })
    .await
    .map_err(|error| miette!("tool startup worker failed: {error}"))??;
    restore_todo_state(
        &recovered.conversation,
        &workspace,
        &SessionId(session_id.clone()),
        &built_tools.todo,
    )
    .await?;
    durable_sink.bind_todo(TodoRestoreBinding {
        todo: Arc::clone(&built_tools.todo),
        workspace: workspace.clone(),
        session_id: SessionId(session_id.clone()),
    });

    let configured_run_model_alias = options.model.clone().unwrap_or(persisted_model_alias);
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
                    "openai" | "openai_responses" | "openai_chat" | "openai_codex"
                    | "openai_subscription" => CacheBreakpointSupport::Automatic,
                    "github_copilot" | "openai_compatible" | "openai_compatible_chat"
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
    let actor_tools = inspection_profile.as_ref().map_or_else(
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
            Arc::new(ProviderModel::new(
                scripted,
                loaded_config.config.compaction.clone(),
                loaded_config.config.budget.clone(),
            )),
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
            Arc::new(ProviderModel::new(
                scripted,
                loaded_config.config.compaction.clone(),
                loaded_config.config.budget.clone(),
            )),
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
            Arc::new(ProviderModel::new(
                recorder,
                loaded_config.config.compaction.clone(),
                loaded_config.config.budget.clone(),
            )),
            fixture_redactor.clone(),
        )
    } else if let Some(directory) = &options.replay_dir {
        let replay: Arc<dyn Provider> = Arc::new(
            ReplayProvider::load(&options.replay_provider, directory)
                .await
                .map_err(|error| miette!("replay provider could not load: {error}"))?,
        );
        (
            Arc::new(ProviderModel::new(
                replay,
                loaded_config.config.compaction.clone(),
                loaded_config.config.budget.clone(),
            )),
            fixture_redactor.clone(),
        )
    } else {
        let pricing = PricingTable::bundled()
            .map_err(|error| miette!("bundled model catalog is invalid: {error}"))?;
        let runtime = ProviderFactory::system(config_loader.credentials_path(), pricing)
            .build(&loaded_config.config)
            .map_err(|error| miette!("provider runtime could not start: {error}"))?;
        let redactor = runtime.fixture_redactor();
        (Arc::new(runtime), redactor)
    };
    register_credential_environment(&engine_redactor);
    let model: Arc<dyn ModelDriver> = if inspection {
        model
    } else {
        Arc::new(PromptRecordingModel {
            inner: model,
            journal: Arc::clone(&durable_sink.prompt_shapes),
        })
    };
    let project_approvals = project_approval_path(&storage_root, &workspace);
    let permissions = match options.permission_mode {
        Some(mode) => PermissionGate::for_headless_mode(mode.into()),
        None => PermissionGate::from_config(loaded_config.config.permissions.clone()),
    }
    .with_workspace_roots(&workspace_roots)
    .with_command_safety(Arc::clone(&command_safety))
    .with_project_approval_file(project_approvals.clone());
    let permissions = Arc::new(permissions);
    let folder_trust = Arc::new(RuntimeFolderTrustController::new(
        storage_root.join("trust.json"),
        workspace_roots.clone(),
    ));
    let workspace_root_controller = Arc::new(RuntimeWorkspaceRootController {
        checkpoint_root,
        question_asker: root_question_asker,
        offline: offline_fixture,
        global_proxy: root_global_proxy,
        command_fixture_mode: root_command_fixture_mode,
        execution_lease: root_execution_lease,
        command_safety: root_command_safety,
        trust_store_path: storage_root.join("trust.json"),
    });
    let actor = SessionActor::spawn(SessionActorConfig {
        session_id: SessionId(session_id.clone()),
        workspace_root: workspace,
        additional_workspace_roots: workspace_roots.into_iter().skip(1).collect(),
        workspace_generation: recovered
            .workspace_generation
            .max(persisted_workspace_generation),
        initial_session_context: initial_context,
        model_alias,
        model,
        tools: actor_tools,
        permissions,
        hooks: Arc::new(builtin_hook_dispatcher().map_err(display_agent_error)?),
        commands: Arc::new(builtin_command_registry().map_err(display_agent_error)?),
        event_sink: durable_sink.clone(),
        event_clock: Arc::new(SystemEventClock),
        secret_redactor: Arc::new(SharedEngineSecretRedactor(engine_redactor)),
        checkpoints: checkpoint_coordinator,
        folder_trust,
        workspace_roots: workspace_root_controller,
        recovered,
        max_turns: options.max_turns,
        identical_tool_failure_limit: DEFAULT_DOOM_LOOP_LIMIT,
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        thinking: ThinkingLevel::Off,
        event_capacity: DEFAULT_EVENT_CAPACITY,
    })
    .map_err(display_agent_error)?;

    let outcome = if let Some(turn) = prompt_dump_turn {
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
        serde_json::to_writer_pretty(io::stdout().lock(), &dump).into_diagnostic()?;
        println!();
        None
    } else if let Some(prompt) = options.prompt {
        run_print(
            &actor,
            &session_id,
            &prompt,
            options.output_format,
            options.perf_markers,
        )
        .await?
    } else {
        run_repl(&actor, &storage_root, options.output_format).await?
    };
    if interactive {
        update_one_session_index(&storage_root, &session_id, &durable_sink)?;
    }
    built_tools
        .todo
        .clear_session(&SessionId(session_id.clone()))
        .await;
    if let Some(status) = outcome
        && status != TurnStatus::Completed
    {
        return Err(miette!("agent turn ended with status {status:?}"));
    }
    Ok(())
}

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
    let workspace = std::fs::canonicalize(&options.workspace).into_diagnostic()?;
    if workspace != options.workspace {
        return Err(miette!("hosted workspace must already be canonical"));
    }
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
    std::fs::create_dir_all(&options.storage_root).into_diagnostic()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            &options.storage_root,
            std::fs::Permissions::from_mode(0o700),
        )
        .into_diagnostic()?;
    }

    let session_id = options.session_id.0.clone();
    let log = SessionEventLog::open(&options.storage_root, &session_id)
        .map_err(|error| miette!("session log could not open: {error}"))?;
    if options.resume {
        let committed = project_session_events(&load_session_events(&log)?)
            .map_err(|error| miette!("session root projection failed: {error}"))?;
        if let Some(generation) = restore_persisted_workspace_roots(
            &checkpoint_root(&options.storage_root, &workspace, &session_id),
            &workspace,
            &workspace_roots,
            committed.workspace_generation,
        )? {
            persisted_workspace_generation = generation.generation;
            workspace_roots = generation.roots;
        }
    }
    let execution_lease_path = options
        .storage_root
        .join("sessions")
        .join(&session_id)
        .join("execution.lock");
    let execution_lease = Arc::new(
        tokio::task::spawn_blocking(move || ExecutionLease::acquire(execution_lease_path))
            .await
            .map_err(|error| miette!("execution lease worker failed: {error}"))?
            .map_err(|error| miette!("execution lease could not lock: {error}"))?,
    );

    let configured_model_alias = options
        .requested_model
        .clone()
        .unwrap_or_else(|| options.config.models.default.clone());
    let (initial_context, persisted_model_alias) = if options.resume {
        let metadata = load_session_metadata(&options.storage_root, &session_id, &workspace)?;
        let mut context = metadata.initial_session_context;
        let recorded_count = metadata.workspace_roots.len().max(1);
        for root in workspace_roots.iter().skip(recorded_count) {
            if let Some(instructions) = rw_core::load_root_project_instructions(root)
                .map_err(|error| miette!("project instructions could not load: {error}"))?
            {
                context.push(instructions.as_system_turn());
            }
        }
        (context, metadata.model_alias)
    } else {
        let context = initial_session_context(&workspace)
            .map_err(|error| miette!("project instructions could not load: {error}"))?;
        persist_session_metadata(
            &options.storage_root,
            &session_id,
            &workspace,
            &configured_model_alias,
            &context,
            &workspace_roots,
        )?;
        (context, configured_model_alias)
    };

    let checkpoint_stores = open_checkpoint_stores(
        &checkpoint_root(&options.storage_root, &workspace, &session_id),
        &workspace_roots,
    )?;
    let recovery_stores = Arc::clone(&checkpoint_stores);
    tokio::task::spawn_blocking(move || {
        for store in recovery_stores.iter() {
            store.recover_opaque_mutations()?;
        }
        Ok::<_, rw_store::checkpoint::CheckpointError>(())
    })
    .await
    .map_err(|error| miette!("checkpoint recovery worker failed: {error}"))?
    .map_err(|error| miette!("checkpoint recovery failed: {error}"))?;
    let rewind_stores = Arc::clone(&checkpoint_stores);
    let log = tokio::task::spawn_blocking(move || {
        let mut log = log;
        recover_rewind_transactions(&rewind_stores, &mut log)?;
        Ok::<_, miette::Report>(log)
    })
    .await
    .map_err(|error| miette!("rewind recovery worker failed: {error}"))??;
    let recovered_events = load_session_events(&log)?;
    let recovered = project_session_events(&recovered_events)
        .map_err(|error| miette!("session log projection failed: {error}"))?;
    let descriptor_model = recovered
        .model_alias
        .clone()
        .unwrap_or_else(|| persisted_model_alias.clone());
    let driver_client_id = recovered.driver_client_id.clone();
    let shell_active = recovered.active_shell.is_some();
    let durable_sink = Arc::new(DurableEventSink::new(
        log,
        options.storage_root.clone(),
        session_id.clone(),
    )?);
    durable_sink.reconcile_accounting(&recovered_events)?;
    let checkpoint_coordinator =
        Arc::new(DurableCheckpointCoordinator::from_stores(checkpoint_stores));

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
    let proxy_config = options.config.clone();
    let proxy_credentials_path = options.credentials_path.clone();
    let proxy_redactor = fixture_redactor.clone();
    let global_proxy = tokio::task::spawn_blocking(move || {
        resolve_tool_proxy(
            &proxy_config,
            &proxy_credentials_path,
            offline,
            &proxy_redactor,
        )
    })
    .await
    .map_err(|error| miette!("tool proxy credential worker failed: {error}"))??;
    let tool_workspace_roots = workspace_roots.clone();
    let tool_execution_lease = Arc::clone(&execution_lease);
    let root_question_asker: Arc<dyn QuestionAsker> = Arc::new(HeadlessQuestionAsker);
    let command_safety = Arc::new(
        CommandSafetyClassifier::new(&options.config.sandbox.safe_list)
            .map_err(|error| miette!(error))?,
    );
    let tool_command_safety = Arc::clone(&command_safety);
    let root_command_safety = Arc::clone(&command_safety);
    let tool_question_asker = Arc::clone(&root_question_asker);
    let root_command_fixture_mode = command_fixture_mode.clone();
    let root_global_proxy = global_proxy.clone();
    let root_execution_lease = Arc::clone(&execution_lease);
    let built_tools = tokio::task::spawn_blocking(move || {
        build_tools(
            &tool_workspace_roots,
            tool_question_asker,
            offline,
            global_proxy.as_ref(),
            command_fixture_mode,
            tool_execution_lease,
            &tool_command_safety,
        )
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

    let (model, engine_redactor): (Arc<dyn ModelDriver>, FixtureRedactor) = match options
        .provider_mode
    {
        HostedProviderMode::Live => {
            let pricing = PricingTable::bundled()
                .map_err(|error| miette!("bundled model catalog is invalid: {error}"))?;
            match ProviderFactory::system(options.credentials_path, pricing).build(&options.config)
            {
                Ok(runtime) => {
                    let redactor = runtime.fixture_redactor();
                    (Arc::new(runtime), redactor)
                }
                Err(error) => (
                    Arc::new(UnavailableHostedModel {
                        alias: persisted_model_alias.clone(),
                        reason: error.to_string(),
                        compaction: options.config.compaction.clone(),
                        budget: options.config.budget.clone(),
                    }),
                    fixture_redactor,
                ),
            }
        }
        HostedProviderMode::DeterministicReplay {
            provider_name,
            scripts,
        } => {
            let provider: Arc<dyn Provider> =
                Arc::new(ScriptProvider::new(provider_name, scripts, 0));
            (
                Arc::new(ProviderModel::new(
                    provider,
                    options.config.compaction.clone(),
                    options.config.budget.clone(),
                )),
                fixture_redactor,
            )
        }
    };
    register_credential_environment(&engine_redactor);
    let model: Arc<dyn ModelDriver> = Arc::new(PromptRecordingModel {
        inner: model,
        journal: Arc::clone(&durable_sink.prompt_shapes),
    });
    let project_approvals = project_approval_path(&options.storage_root, &workspace);
    let permissions = match options.permission_mode {
        Some(mode) => PermissionGate::for_headless_mode(mode.into()),
        None => PermissionGate::from_config(options.config.permissions.clone()),
    }
    .with_workspace_roots(&workspace_roots)
    .with_command_safety(Arc::clone(&command_safety))
    .with_project_approval_file(project_approvals.clone());
    let permissions = Arc::new(permissions);
    let folder_trust = Arc::new(RuntimeFolderTrustController::new(
        options.storage_root.join("trust.json"),
        workspace_roots.clone(),
    ));
    let workspace_root_controller = Arc::new(RuntimeWorkspaceRootController {
        checkpoint_root: checkpoint_root(&options.storage_root, &workspace, &session_id),
        question_asker: root_question_asker,
        offline,
        global_proxy: root_global_proxy,
        command_fixture_mode: root_command_fixture_mode,
        execution_lease: root_execution_lease,
        command_safety: root_command_safety,
        trust_store_path: options.storage_root.join("trust.json"),
    });
    let handle = SessionActor::spawn(SessionActorConfig {
        session_id: options.session_id,
        workspace_root: workspace,
        additional_workspace_roots: workspace_roots.into_iter().skip(1).collect(),
        workspace_generation: recovered
            .workspace_generation
            .max(persisted_workspace_generation),
        initial_session_context: initial_context,
        model_alias: persisted_model_alias,
        model,
        tools: built_tools.registry,
        permissions,
        hooks: Arc::new(builtin_hook_dispatcher().map_err(display_agent_error)?),
        commands: Arc::new(builtin_command_registry().map_err(display_agent_error)?),
        event_sink: durable_sink,
        event_clock: Arc::new(SystemEventClock),
        secret_redactor: Arc::new(SharedEngineSecretRedactor(engine_redactor)),
        checkpoints: checkpoint_coordinator,
        folder_trust,
        workspace_roots: workspace_root_controller,
        recovered,
        max_turns: options.max_turns,
        identical_tool_failure_limit: DEFAULT_DOOM_LOOP_LIMIT,
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        thinking: ThinkingLevel::Off,
        event_capacity: DEFAULT_EVENT_CAPACITY,
    })
    .map_err(display_agent_error)?;
    Ok(HostedActorRuntime {
        handle,
        model_alias: descriptor_model,
        driver_client_id,
        shell_active,
    })
}

fn load_provider_script(path: &Path) -> Result<Vec<Vec<ProviderEvent>>> {
    serde_json::from_slice(&std::fs::read(path).into_diagnostic()?).into_diagnostic()
}

#[allow(clippy::needless_pass_by_value)]
fn display_agent_error(error: AgentLoopError) -> miette::Report {
    miette!(error.to_string())
}

impl From<PermissionMode> for rw_core::HeadlessPermissionMode {
    fn from(value: PermissionMode) -> Self {
        match value {
            PermissionMode::Strict => Self::Strict,
            PermissionMode::AutoSafe => Self::AutoSafe,
            PermissionMode::Yolo => Self::Yolo,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionMetadata {
    version: u16,
    session_id: String,
    pub workspace: PathBuf,
    pub model_alias: String,
    initial_session_context: Vec<Turn>,
    #[serde(default)]
    pub workspace_generation: u64,
    #[serde(default)]
    pub workspace_roots: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PromptShapeProfile {
    model_alias: String,
    tools: Vec<ToolDefinition>,
    cache_support: CacheBreakpointSupport,
    #[serde(default)]
    cache_hint: Option<CacheHint>,
    #[serde(default)]
    cache_breakpoints: Vec<PromptCacheBreakpoint>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PromptCacheBreakpoint {
    after_item_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PromptShapeRecord {
    profile_id: String,
    request_fingerprint: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PromptShapeState {
    version: u16,
    #[serde(default)]
    profiles: BTreeMap<String, PromptShapeProfile>,
    #[serde(default)]
    records: BTreeMap<String, PromptShapeRecord>,
}

impl Default for PromptShapeState {
    fn default() -> Self {
        Self {
            version: PROMPT_SHAPE_VERSION,
            profiles: BTreeMap::new(),
            records: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
struct PromptShapeJournal {
    path: PathBuf,
    state: Mutex<PromptShapeState>,
    active_turn: Mutex<Option<rw_core::TurnId>>,
}

impl PromptShapeJournal {
    fn open(storage_root: &Path, session_id: &str) -> Result<Self> {
        validate_session_id(session_id)?;
        let directory = storage_root.join("sessions").join(session_id);
        ensure_real_directory(&directory, false)?;
        let path = directory.join("prompt-shapes.json");
        let state = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(miette!("prompt-shape metadata is not a regular file"));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    if metadata.permissions().mode() & 0o077 != 0 {
                        return Err(miette!(
                            "prompt-shape metadata permissions grant group or other access"
                        ));
                    }
                }
                let bytes = std::fs::read(&path).into_diagnostic()?;
                let state: PromptShapeState = serde_json::from_slice(&bytes).into_diagnostic()?;
                if state.version != PROMPT_SHAPE_VERSION {
                    return Err(miette!("unsupported prompt-shape metadata version"));
                }
                validate_prompt_shape_state(&state)?;
                state
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => PromptShapeState::default(),
            Err(error) => return Err(error).into_diagnostic(),
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
            active_turn: Mutex::new(None),
        })
    }

    fn set_active_turn(&self, turn_id: rw_core::TurnId) {
        *self
            .active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(turn_id);
    }

    fn clear_active_turn(&self, turn_id: &rw_core::TurnId) {
        let mut active = self
            .active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.as_ref() == Some(turn_id) {
            *active = None;
        }
    }

    fn record_request(
        &self,
        model_alias: &str,
        request: &ProviderRequest,
        cache_support: CacheBreakpointSupport,
    ) -> Result<()> {
        if request.tool_choice == ToolChoice::None {
            return Ok(());
        }
        let Some(turn_id) = self
            .active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            return Ok(());
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.records.contains_key(&turn_id.0) {
            return Ok(());
        }
        let profile = PromptShapeProfile {
            model_alias: model_alias.to_owned(),
            tools: request.tools.clone(),
            cache_support,
            cache_hint: request.cache_hint,
            cache_breakpoints: cache_breakpoints_for_hint(request.cache_hint, cache_support),
        };
        let profile_id = hash_serialized(&profile)?;
        let request_fingerprint = prompt_request_fingerprint(
            model_alias,
            &request.turns,
            &request.tools,
            request.cache_hint,
            cache_support,
            &profile.cache_breakpoints,
        )?;
        state.profiles.entry(profile_id.clone()).or_insert(profile);
        state.records.insert(
            turn_id.0,
            PromptShapeRecord {
                profile_id,
                request_fingerprint,
            },
        );
        persist_prompt_shape_state(&self.path, &state)
    }

    fn shape_for_turn(&self, turn: u64) -> Result<Option<(PromptShapeProfile, PromptShapeRecord)>> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(record) = state.records.get(&turn.to_string()) else {
            return Ok(None);
        };
        let profile = state
            .profiles
            .get(&record.profile_id)
            .ok_or_else(|| miette!("prompt-shape record references a missing profile"))?;
        Ok(Some((profile.clone(), record.clone())))
    }

    fn latest_shape(&self) -> Result<Option<(PromptShapeProfile, PromptShapeRecord)>> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((_, record)) = state
            .records
            .iter()
            .filter_map(|(turn, record)| turn.parse::<u64>().ok().map(|turn| (turn, record)))
            .max_by_key(|(turn, _)| *turn)
        else {
            return Ok(None);
        };
        let profile = state
            .profiles
            .get(&record.profile_id)
            .ok_or_else(|| miette!("prompt-shape record references a missing profile"))?;
        Ok(Some((profile.clone(), record.clone())))
    }
}

fn hash_serialized(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value).into_diagnostic()?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn cache_breakpoints_for_hint(
    cache_hint: Option<CacheHint>,
    cache_support: CacheBreakpointSupport,
) -> Vec<PromptCacheBreakpoint> {
    if cache_support == CacheBreakpointSupport::None {
        return Vec::new();
    }
    let after_item_id = cache_hint
        .and_then(|hint| hint.stable_prefix_turns.checked_sub(1))
        .map(|index| format!("system:{index}"));
    vec![PromptCacheBreakpoint { after_item_id }]
}

fn prompt_dump_cache_breakpoints(dump: &rw_core::PromptDump) -> Vec<PromptCacheBreakpoint> {
    dump.cache_breakpoints
        .iter()
        .map(|breakpoint| PromptCacheBreakpoint {
            after_item_id: breakpoint
                .after_item_id
                .as_ref()
                .map(|item_id| item_id.0.clone()),
        })
        .collect()
}

fn is_blake3_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_prompt_shape_state(state: &PromptShapeState) -> Result<()> {
    for (profile_id, profile) in &state.profiles {
        if !is_blake3_hex(profile_id) || hash_serialized(profile)? != *profile_id {
            return Err(miette!(
                "prompt-shape profile id does not match its serialized content"
            ));
        }
        if profile
            .cache_hint
            .is_some_and(|hint| hint.tools_in_prefix == profile.tools.is_empty())
            || profile.cache_breakpoints
                != cache_breakpoints_for_hint(profile.cache_hint, profile.cache_support)
        {
            return Err(miette!(
                "prompt-shape profile contains inconsistent cache metadata"
            ));
        }
    }
    for (turn, record) in &state.records {
        if turn.parse::<u64>().is_err() {
            return Err(miette!("prompt-shape record has an invalid turn id"));
        }
        if !is_blake3_hex(&record.request_fingerprint) {
            return Err(miette!(
                "prompt-shape record has an invalid request fingerprint"
            ));
        }
        if !state.profiles.contains_key(&record.profile_id) {
            return Err(miette!("prompt-shape record references a missing profile"));
        }
    }
    Ok(())
}

fn prompt_request_fingerprint(
    model_alias: &str,
    turns: &[Turn],
    tools: &[ToolDefinition],
    cache_hint: Option<CacheHint>,
    cache_support: CacheBreakpointSupport,
    cache_breakpoints: &[PromptCacheBreakpoint],
) -> Result<String> {
    hash_serialized(&serde_json::json!({
        "model_alias": model_alias,
        "turns": turns,
        "tools": tools,
        "cache_hint": cache_hint,
        "cache_support": cache_support,
        "cache_breakpoints": cache_breakpoints,
    }))
}

fn validate_historical_prompt_shape(
    dump: &rw_core::PromptDump,
    tools: &[ToolDefinition],
    profile: &PromptShapeProfile,
    record: &PromptShapeRecord,
) -> Result<()> {
    let fingerprint = prompt_request_fingerprint(
        &dump.model_alias.0,
        &dump.turns,
        tools,
        profile.cache_hint,
        profile.cache_support,
        &profile.cache_breakpoints,
    )?;
    if fingerprint != record.request_fingerprint {
        return Err(miette!(
            "historical prompt reconstruction did not match its recorded request shape"
        ));
    }
    if prompt_dump_cache_breakpoints(dump) != profile.cache_breakpoints {
        return Err(miette!(
            "historical prompt reconstruction did not match its recorded cache behavior"
        ));
    }
    Ok(())
}

fn persist_prompt_shape_state(path: &Path, state: &PromptShapeState) -> Result<()> {
    let bytes = serde_json::to_vec(state).into_diagnostic()?;
    let parent = path
        .parent()
        .ok_or_else(|| miette!("prompt-shape path has no parent"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".prompt-shapes-{}-{nonce}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).into_diagnostic()?;
    let result = (|| -> Result<()> {
        file.write_all(&bytes).into_diagnostic()?;
        file.flush().into_diagnostic()?;
        file.sync_all().into_diagnostic()?;
        std::fs::rename(&temporary, path).into_diagnostic()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn select_session(storage_root: &Path, workspace: &Path, options: &RunOptions) -> Result<String> {
    if let Some(session) = &options.resume {
        return Ok(session.clone());
    }
    if options.continue_latest {
        // The SQLite index is a disposable projection. Print mode intentionally
        // leaves it stale so completing a headless turn never waits on SQLite;
        // an explicit continue operation rebuilds from authoritative JSONL.
        refresh_session_index(storage_root)?;
        if let Some(session) = latest_workspace_session(storage_root, workspace)? {
            return Ok(session);
        }
        if is_zero_turn_prompt_dump(options) {
            return new_session_id();
        }
        return Err(miette!(
            "there is no previous session for workspace {} to continue",
            workspace.display()
        ));
    }
    new_session_id()
}

pub(crate) fn select_interactive_session(
    storage_root: &Path,
    workspace: &Path,
    resume: Option<&str>,
    continue_latest: bool,
) -> Result<String> {
    if let Some(session) = resume {
        validate_session_id(session)?;
        return Ok(session.to_owned());
    }
    if continue_latest {
        refresh_session_index(storage_root)?;
        return latest_workspace_session(storage_root, workspace)?.ok_or_else(|| {
            miette!(
                "there is no previous session for workspace {} to continue",
                workspace.display()
            )
        });
    }
    new_session_id()
}

fn is_zero_turn_prompt_dump(options: &RunOptions) -> bool {
    matches!(options.action, RunAction::PromptDump { turn: None })
}

fn latest_workspace_session(storage_root: &Path, workspace: &Path) -> Result<Option<String>> {
    let sessions = SessionIndex::open(storage_root)
        .map_err(|error| miette!("session index could not open: {error}"))?
        .list(10_000)
        .map_err(|error| miette!("sessions could not be listed: {error}"))?;
    for session in sessions {
        match load_session_metadata(storage_root, &session.id, workspace) {
            Ok(_) => return Ok(Some(session.id)),
            Err(error) => tracing::debug!(
                session_id = %session.id,
                reason = %error,
                "skipping session which does not belong to this workspace"
            ),
        }
    }
    Ok(None)
}

fn validate_session_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(miette!("session id is empty, too long, or unsafe"));
    }
    Ok(())
}

fn checkpoint_root(storage_root: &Path, workspace: &Path, session_id: &str) -> PathBuf {
    let digest = blake3::hash(workspace.as_os_str().as_encoded_bytes())
        .to_hex()
        .to_string();
    storage_root
        .join("workspaces")
        .join(digest)
        .join("sessions")
        .join(session_id)
}

fn open_checkpoint_stores(
    root: &Path,
    workspace_roots: &[PathBuf],
) -> Result<Arc<Vec<Arc<CheckpointStore>>>> {
    if workspace_roots.is_empty() {
        return Err(miette!("checkpoint root mapping cannot be empty"));
    }
    std::fs::create_dir_all(root).into_diagnostic()?;
    let mapping_path = root.join("workspace-roots.json");
    let initial = CheckpointRootMapping {
        version: CHECKPOINT_ROOTS_VERSION,
        generations: vec![CheckpointRootGeneration {
            generation: 0,
            effective_from_turn: 1,
            roots: workspace_roots.to_vec(),
            committed: true,
        }],
    };
    match std::fs::read(&mapping_path) {
        Ok(bytes) => {
            let existing: CheckpointRootMapping = serde_json::from_slice(&bytes)
                .map_err(|error| miette!("checkpoint root mapping is corrupt: {error}"))?;
            if existing.version != CHECKPOINT_ROOTS_VERSION
                || existing.generations.last().map(|entry| &entry.roots)
                    != Some(&workspace_roots.to_vec())
            {
                return Err(miette!(
                    "checkpoint root mapping changed; refusing to resume with reordered or replaced workspace roots"
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            persist_private_json(&mapping_path, &initial)?;
        }
        Err(error) => return Err(miette!("checkpoint root mapping could not load: {error}")),
    }
    let stores = workspace_roots
        .iter()
        .enumerate()
        .map(|(index, workspace)| {
            CheckpointStore::open(&root.join(format!("root-{index:04}")), workspace)
                .map(Arc::new)
                .map_err(|error| {
                    miette!("checkpoint store for root {index} could not open: {error}")
                })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Arc::new(stores))
}

fn append_checkpoint_root_generation(
    root: &Path,
    current_roots: &[PathBuf],
    roots: &[PathBuf],
    generation: u64,
    effective_from_turn: u64,
) -> Result<()> {
    let path = root.join("workspace-roots.json");
    let mut mapping: CheckpointRootMapping = serde_json::from_slice(
        &std::fs::read(&path)
            .map_err(|error| miette!("checkpoint root journal could not load: {error}"))?,
    )
    .map_err(|error| miette!("checkpoint root journal is corrupt: {error}"))?;
    let previous = mapping
        .generations
        .last()
        .ok_or_else(|| miette!("checkpoint root journal is empty"))?;
    if mapping.version != CHECKPOINT_ROOTS_VERSION
        || previous.roots != current_roots
        || generation != previous.generation.saturating_add(1)
        || roots.len() != current_roots.len() + 1
        || roots.iter().take(current_roots.len()).ne(current_roots)
        || effective_from_turn < previous.effective_from_turn
    {
        return Err(miette!(
            "checkpoint root generation is not a strict stable-index append"
        ));
    }
    mapping.generations.push(CheckpointRootGeneration {
        generation,
        effective_from_turn,
        roots: roots.to_vec(),
        committed: false,
    });
    persist_private_json(&path, &mapping)
}

fn commit_checkpoint_root_generation(root: &Path, generation: u64) -> Result<()> {
    let path = root.join("workspace-roots.json");
    let mut mapping: CheckpointRootMapping = serde_json::from_slice(
        &std::fs::read(&path)
            .map_err(|error| miette!("checkpoint root journal could not load: {error}"))?,
    )
    .map_err(|error| miette!("checkpoint root journal is corrupt: {error}"))?;
    let entry = mapping
        .generations
        .last_mut()
        .filter(|entry| entry.generation == generation)
        .ok_or_else(|| miette!("prepared workspace generation is unavailable"))?;
    entry.committed = true;
    persist_private_json(&path, &mapping)
}

fn abort_checkpoint_root_generation(root: &Path, generation: u64) -> Result<()> {
    let path = root.join("workspace-roots.json");
    let mut mapping: CheckpointRootMapping = serde_json::from_slice(
        &std::fs::read(&path)
            .map_err(|error| miette!("checkpoint root journal could not load: {error}"))?,
    )
    .map_err(|error| miette!("checkpoint root journal is corrupt: {error}"))?;
    if mapping
        .generations
        .last()
        .is_some_and(|entry| entry.generation == generation && !entry.committed)
    {
        mapping.generations.pop();
        if mapping.generations.is_empty() {
            return Err(miette!(
                "checkpoint root journal cannot remove its base generation"
            ));
        }
        persist_private_json(&path, &mapping)?;
    }
    Ok(())
}

fn load_checkpoint_root_generation(root: &Path) -> Result<Option<CheckpointRootGeneration>> {
    let path = root.join("workspace-roots.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(miette!("checkpoint root journal could not load: {error}")),
    };
    let mapping: CheckpointRootMapping = serde_json::from_slice(&bytes)
        .map_err(|error| miette!("checkpoint root journal is corrupt: {error}"))?;
    if mapping.version != CHECKPOINT_ROOTS_VERSION {
        return Err(miette!("checkpoint root journal version is unsupported"));
    }
    Ok(mapping
        .generations
        .iter()
        .rev()
        .find(|generation| generation.committed)
        .cloned())
}

pub(crate) fn load_session_workspace_roots(
    storage_root: &Path,
    workspace: &Path,
    session_id: &str,
) -> Result<Vec<PathBuf>> {
    let root = checkpoint_root(storage_root, workspace, session_id);
    Ok(load_checkpoint_root_generation(&root)?.map_or_else(
        || vec![workspace.to_path_buf()],
        |generation| generation.roots,
    ))
}

fn restore_persisted_workspace_roots(
    root: &Path,
    primary: &Path,
    supplied: &[PathBuf],
    committed_generation: u64,
) -> Result<Option<CheckpointRootGeneration>> {
    let path = root.join("workspace-roots.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(miette!("checkpoint root journal could not load: {error}")),
    };
    let mut mapping: CheckpointRootMapping = serde_json::from_slice(&bytes)
        .map_err(|error| miette!("checkpoint root journal is corrupt: {error}"))?;
    let Some(position) = mapping
        .generations
        .iter()
        .position(|entry| entry.generation == committed_generation)
    else {
        return Err(miette!(
            "committed workspace generation is absent from the local root journal"
        ));
    };
    let needs_rewrite =
        position + 1 < mapping.generations.len() || !mapping.generations[position].committed;
    if position + 1 < mapping.generations.len() {
        mapping.generations.truncate(position + 1);
    }
    mapping.generations[position].committed = true;
    if needs_rewrite {
        persist_private_json(&path, &mapping)?;
    }
    let Some(mut generation) = mapping.generations.last().cloned() else {
        return Ok(None);
    };
    generation.roots = canonical_workspace_roots(primary, &generation.roots[1..])?;
    if supplied.len() > 1 && supplied != generation.roots {
        return Err(miette!(
            "resume workspace roots differ from the durable stable-index generation"
        ));
    }
    Ok(Some(generation))
}

fn persist_private_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).into_diagnostic()?;
    let parent = path
        .parent()
        .ok_or_else(|| miette!("private JSON path has no parent"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".roots-{}-{nonce}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).into_diagnostic()?;
    let result = (|| -> Result<()> {
        file.write_all(&bytes).into_diagnostic()?;
        file.flush().into_diagnostic()?;
        file.sync_all().into_diagnostic()?;
        std::fs::rename(&temporary, path).into_diagnostic()?;
        std::fs::File::open(parent)
            .into_diagnostic()?
            .sync_all()
            .into_diagnostic()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn project_approval_path(storage_root: &Path, workspace: &Path) -> PathBuf {
    let digest = blake3::hash(workspace.as_os_str().as_encoded_bytes())
        .to_hex()
        .to_string();
    storage_root
        .join("workspaces")
        .join(digest)
        .join("permission-approvals.json")
}

fn persist_session_metadata(
    storage_root: &Path,
    session_id: &str,
    workspace: &Path,
    model_alias: &str,
    initial_session_context: &[Turn],
    workspace_roots: &[PathBuf],
) -> Result<()> {
    validate_session_id(session_id)?;
    let sessions = storage_root.join("sessions");
    ensure_real_directory(&sessions, false)?;
    let directory = sessions.join(session_id);
    ensure_real_directory(&directory, false)?;
    let metadata = SessionMetadata {
        version: SESSION_METADATA_VERSION,
        session_id: session_id.to_owned(),
        workspace: workspace.to_path_buf(),
        model_alias: model_alias.to_owned(),
        initial_session_context: initial_session_context.to_vec(),
        workspace_generation: 0,
        workspace_roots: workspace_roots.to_vec(),
    };
    let bytes = serde_json::to_vec(&metadata).into_diagnostic()?;
    let path = directory.join("metadata.json");
    #[cfg(unix)]
    {
        persist_session_metadata_unix(&directory, &path, &bytes)
    }
    #[cfg(not(unix))]
    {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        let mut file = options.open(&path).into_diagnostic()?;
        file.write_all(&bytes).into_diagnostic()?;
        file.flush().into_diagnostic()?;
        sync_file(&file)?;
        sync_directory(&directory)
    }
}

fn load_session_metadata(
    storage_root: &Path,
    session_id: &str,
    expected_workspace: &Path,
) -> Result<SessionMetadata> {
    let metadata = load_session_metadata_any(storage_root, session_id)?;
    if metadata.workspace != expected_workspace {
        return Err(miette!(
            "session metadata identity does not match this session and canonical workspace"
        ));
    }
    Ok(metadata)
}

pub(crate) fn load_session_metadata_any(
    storage_root: &Path,
    session_id: &str,
) -> Result<SessionMetadata> {
    validate_session_id(session_id)?;
    let sessions = storage_root.join("sessions");
    ensure_real_directory(&sessions, false)?;
    let directory = sessions.join(session_id);
    ensure_real_directory(&directory, false)?;
    let path = directory.join("metadata.json");
    #[cfg(unix)]
    let bytes = load_session_metadata_unix(&directory, &path)?;
    #[cfg(not(unix))]
    let bytes = {
        let metadata_on_disk = std::fs::symlink_metadata(&path).into_diagnostic()?;
        if metadata_on_disk.file_type().is_symlink() || !metadata_on_disk.is_file() {
            return Err(miette!("session metadata is not a regular file"));
        }
        std::fs::read(&path).into_diagnostic()?
    };
    let metadata: SessionMetadata = serde_json::from_slice(&bytes).into_diagnostic()?;
    if metadata.version != SESSION_METADATA_VERSION || metadata.session_id != session_id {
        return Err(miette!(
            "session metadata identity does not match this session and canonical workspace"
        ));
    }
    Ok(metadata)
}

#[cfg(unix)]
fn open_session_metadata_directory(directory: &Path) -> Result<std::os::fd::OwnedFd> {
    rustix::fs::open(
        directory,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()
}

#[cfg(unix)]
fn persist_session_metadata_unix(directory: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = open_session_metadata_directory(directory)?;
    let descriptor = rustix::fs::openat(
        &parent,
        "metadata.json",
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    let mut file = std::fs::File::from(descriptor);
    file.write_all(bytes).into_diagnostic()?;
    file.flush().into_diagnostic()?;
    rustix::fs::fsync(&file)
        .map_err(std::io::Error::from)
        .into_diagnostic()?;
    rustix::fs::fsync(&parent)
        .map_err(std::io::Error::from)
        .into_diagnostic()
        .map_err(|error| miette!("could not synchronize {}: {error}", path.display()))
}

#[cfg(unix)]
fn load_session_metadata_unix(directory: &Path, path: &Path) -> Result<Vec<u8>> {
    let parent = open_session_metadata_directory(directory)?;
    let stat = rustix::fs::statat(
        &parent,
        "metadata.json",
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(miette!("session metadata is not a regular file"));
    }
    if stat.st_mode & 0o077 != 0 {
        return Err(miette!(
            "session metadata permissions grant group or other access"
        ));
    }
    let descriptor = rustix::fs::openat(
        &parent,
        "metadata.json",
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    let mut file = std::fs::File::from(descriptor);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .into_diagnostic()
        .map_err(|error| miette!("could not read {}: {error}", path.display()))?;
    Ok(bytes)
}

fn ensure_real_directory(path: &Path, create: bool) -> Result<()> {
    if create {
        std::fs::create_dir_all(path).into_diagnostic()?;
    }
    let metadata = std::fs::symlink_metadata(path).into_diagnostic()?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(miette!("{} is not a real directory", path.display()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(path: &Path) -> Result<()> {
    let directory = std::fs::File::open(path).into_diagnostic()?;
    sync_file(&directory)
}

#[cfg(not(unix))]
fn sync_file(file: &std::fs::File) -> Result<()> {
    #[cfg(unix)]
    {
        rustix::fs::fsync(file)
            .map_err(std::io::Error::from)
            .into_diagnostic()
    }
    #[cfg(not(unix))]
    {
        file.sync_all().into_diagnostic()
    }
}

pub(crate) fn new_session_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| miette!("session id entropy failed: {error}"))?;
    let mut id = String::with_capacity(40);
    id.push_str("session-");
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut id, "{byte:02x}").into_diagnostic()?;
    }
    Ok(id)
}

struct DurableEventSink {
    log: Arc<Mutex<SessionEventLog>>,
    storage_root: PathBuf,
    session_id: String,
    prompt_shapes: Arc<PromptShapeJournal>,
    accounting_dirty: AtomicBool,
    todo_restore: Mutex<Option<TodoRestoreBinding>>,
}

impl DurableEventSink {
    fn new(log: SessionEventLog, storage_root: PathBuf, session_id: String) -> Result<Self> {
        let log = Arc::new(Mutex::new(log));
        let prompt_shapes = Arc::new(PromptShapeJournal::open(&storage_root, &session_id)?);
        Ok(Self {
            log,
            storage_root,
            session_id,
            prompt_shapes,
            accounting_dirty: AtomicBool::new(false),
            todo_restore: Mutex::new(None),
        })
    }

    fn bind_todo(&self, binding: TodoRestoreBinding) {
        *self
            .todo_restore
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(binding);
    }

    fn load(&self) -> Result<Vec<EngineEvent>> {
        let log = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        load_session_events(&log)
    }
}

#[async_trait]
impl SessionEventSink for DurableEventSink {
    async fn append(&self, event: EngineEvent) -> std::result::Result<EngineEvent, AgentLoopError> {
        self.append_batch(vec![event])
            .await?
            .pop()
            .ok_or_else(|| AgentLoopError::Persistence("event batch returned empty".to_owned()))
    }

    async fn append_batch(
        &self,
        events: Vec<EngineEvent>,
    ) -> std::result::Result<Vec<EngineEvent>, AgentLoopError> {
        let rewound = events
            .iter()
            .any(|event| matches!(event, EngineEvent::ConversationRewound { .. }));
        let log = Arc::clone(&self.log);
        let append = move || {
            let mut log = log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (offset, event) in events.iter().enumerate() {
                let offset = u64::try_from(offset).map_err(|_| {
                    AgentLoopError::Persistence("event batch length overflow".to_owned())
                })?;
                let expected = log.next_sequence().checked_add(offset).ok_or_else(|| {
                    AgentLoopError::Persistence("event sequence overflow".to_owned())
                })?;
                let meta = event.meta().ok_or_else(|| {
                    AgentLoopError::Persistence(
                        "connection acknowledgement cannot be persisted".to_owned(),
                    )
                })?;
                if meta.sequence_id.0 != expected {
                    return Err(AgentLoopError::Persistence(format!(
                        "event sequence {} does not match log sequence {expected}",
                        meta.sequence_id.0
                    )));
                }
            }
            log.append_batch(events)
                .map_err(|error| AgentLoopError::Persistence(error.to_string()))
        };
        let envelopes = match tokio::runtime::Handle::current().runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(append),
            _ => tokio::task::spawn_blocking(append)
                .await
                .map_err(|error| AgentLoopError::Persistence(error.to_string()))?,
        }?;
        if rewound {
            let binding = self
                .todo_restore
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(binding) = binding {
                let events = self
                    .load()
                    .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
                let recovered = project_session_events(&events)
                    .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
                restore_todo_state(
                    &recovered.conversation,
                    &binding.workspace,
                    &binding.session_id,
                    &binding.todo,
                )
                .await
                .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
            }
        }
        let persisted = envelopes
            .into_iter()
            .map(|envelope| envelope.event)
            .collect::<Vec<_>>();
        for event in &persisted {
            match event {
                EngineEvent::TurnStarted { turn_id, .. } => {
                    self.prompt_shapes.set_active_turn(turn_id.clone());
                }
                EngineEvent::TurnFinished { turn_id, .. } => {
                    self.prompt_shapes.clear_active_turn(turn_id);
                }
                _ => {}
            }
        }
        if let Err(error) = self.reconcile_accounting(&persisted) {
            self.accounting_dirty.store(true, Ordering::Release);
            tracing::warn!(
                session_id = %self.session_id,
                reason = %error,
                "durable accounting projection will be repaired on the next query"
            );
        }
        Ok(persisted)
    }

    async fn read_after(
        &self,
        last_seen: Option<SequenceId>,
    ) -> std::result::Result<Vec<EngineEvent>, AgentLoopError> {
        let events = self
            .load()
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        Ok(events
            .into_iter()
            .filter(|event| {
                event
                    .meta()
                    .is_some_and(|meta| last_seen.is_none_or(|last| meta.sequence_id > last))
            })
            .collect())
    }

    async fn budget_totals(
        &self,
        query: BudgetLedgerQuery,
    ) -> std::result::Result<BudgetLedgerTotals, AgentLoopError> {
        if self.accounting_dirty.swap(false, Ordering::AcqRel) {
            let repair = self
                .load()
                .and_then(|events| self.reconcile_accounting(&events));
            if let Err(error) = repair {
                self.accounting_dirty.store(true, Ordering::Release);
                return Err(AgentLoopError::Persistence(error.to_string()));
            }
        }
        let now = UtcTimestamp::from_unix_millis(query.now_unix_ms)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        let day_start = UtcTimestamp::from_unix_millis(query.utc_day_start_unix_ms)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        let trailing_start = UtcTimestamp::from_unix_millis(query.trailing_minute_start_unix_ms)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        let totals = AccountingLedger::open(&self.storage_root)
            .and_then(|ledger| {
                ledger.totals(
                    &self.session_id,
                    &day_start.utc_day(),
                    &trailing_start,
                    &now,
                )
            })
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        Ok(BudgetLedgerTotals {
            authoritative: true,
            session_cost_micros_usd: totals.session_micros_usd,
            session_ai_credit_micros: totals.session_ai_credit_micros,
            daily_cost_micros_usd: totals.day_micros_usd,
            daily_ai_credit_micros: totals.day_ai_credit_micros,
            trailing_minute_cost_micros_usd: totals.trailing_all_sessions_micros_usd,
            trailing_minute_ai_credit_micros: totals.trailing_all_sessions_ai_credit_micros,
            session_subscription_quota_entries: totals.session_subscription_quota_turns,
            session_cost_unavailable_entries: totals.session_unavailable_turns,
            session_non_usd_monetary_entries: totals.session_non_usd_monetary_turns,
            daily_subscription_quota_entries: totals.day_subscription_quota_turns,
            daily_cost_unavailable_entries: totals.day_unavailable_turns,
            daily_non_usd_monetary_entries: totals.day_non_usd_monetary_turns,
        })
    }
}

impl DurableEventSink {
    fn reconcile_accounting(&self, events: &[EngineEvent]) -> Result<()> {
        let entries = project_accounting(&self.session_id, events)?;
        if entries.is_empty() {
            return Ok(());
        }
        AccountingLedger::open(&self.storage_root)
            .and_then(|ledger| ledger.reconcile(&entries))
            .map_err(|error| miette!("session accounting could not reconcile: {error}"))
    }
}

#[derive(Clone)]
struct TodoRestoreBinding {
    todo: Arc<TodoTool>,
    workspace: PathBuf,
    session_id: SessionId,
}

fn load_session_events(log: &SessionEventLog) -> Result<Vec<EngineEvent>> {
    let envelopes = log
        .load::<EngineEvent>()
        .map_err(|error| miette!("session events could not load: {error}"))?;
    envelopes
        .into_iter()
        .map(|envelope| {
            let meta = envelope
                .event
                .meta()
                .ok_or_else(|| miette!("persisted command acknowledgement is invalid"))?;
            if meta.sequence_id != envelope.sequence {
                return Err(miette!(
                    "persisted event sequence {} does not match storage envelope {}",
                    meta.sequence_id.0,
                    envelope.sequence.0
                ));
            }
            Ok(envelope.event)
        })
        .collect()
}

struct RuntimeFolderTrustController {
    store: FolderTrustStore,
    workspaces: Vec<PathBuf>,
}

struct RuntimeWorkspaceRootController {
    checkpoint_root: PathBuf,
    question_asker: Arc<dyn QuestionAsker>,
    offline: bool,
    global_proxy: Option<ResolvedToolProxy>,
    command_fixture_mode: CommandFixtureMode,
    execution_lease: Arc<ExecutionLease>,
    command_safety: Arc<CommandSafetyClassifier>,
    trust_store_path: PathBuf,
}

#[async_trait]
impl rw_core::WorkspaceRootController for RuntimeWorkspaceRootController {
    async fn append_root(
        &self,
        requested: &Path,
        current_roots: &[PathBuf],
        current_generation: u64,
        effective_from_turn: u64,
        permissions: Arc<PermissionGate>,
    ) -> std::result::Result<rw_core::WorkspaceRuntimeGeneration, AgentLoopError> {
        let requested = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            current_roots[0].join(requested)
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
        FolderTrustStore::new(self.trust_store_path.clone())
            .assess(&canonical)
            .map_err(|_error| {
                AgentLoopError::InvalidConfiguration(
                    "workspace root trust assessment failed".to_owned(),
                )
            })?;
        let mut roots = current_roots.to_vec();
        roots.push(canonical.clone());
        let supplemental_context = rw_core::load_root_project_instructions(&canonical)
            .map_err(|_error| {
                AgentLoopError::InvalidConfiguration(
                    "workspace root instructions could not load".to_owned(),
                )
            })?
            .map(|instructions| vec![instructions.as_system_turn()])
            .unwrap_or_default();
        let built = build_tools(
            &roots,
            Arc::clone(&self.question_asker),
            self.offline,
            self.global_proxy.as_ref(),
            self.command_fixture_mode.clone(),
            Arc::clone(&self.execution_lease),
            &self.command_safety,
        )
        .map_err(|_error| {
            AgentLoopError::InvalidConfiguration(
                "workspace tool generation could not prepare".to_owned(),
            )
        })?;
        let permissions = Arc::new(permissions.fork_for_workspace_roots(&roots).map_err(
            |_error| {
                AgentLoopError::Persistence(
                    "workspace permission generation could not prepare".to_owned(),
                )
            },
        )?);
        let generation = current_generation.saturating_add(1);
        append_checkpoint_root_generation(
            &self.checkpoint_root,
            current_roots,
            &roots,
            generation,
            effective_from_turn,
        )
        .map_err(|_error| {
            AgentLoopError::Persistence("workspace generation journal could not prepare".to_owned())
        })?;
        let stores = match open_checkpoint_stores(&self.checkpoint_root, &roots) {
            Ok(stores) => stores,
            Err(_error) => {
                let _ = abort_checkpoint_root_generation(&self.checkpoint_root, generation);
                return Err(AgentLoopError::Persistence(
                    "workspace checkpoint generation could not prepare".to_owned(),
                ));
            }
        };
        Ok(rw_core::WorkspaceRuntimeGeneration {
            generation,
            effective_from_turn,
            roots: roots.clone(),
            tools: built.registry,
            permissions,
            checkpoints: Arc::new(DurableCheckpointCoordinator::from_stores(stores)),
            folder_trust: Arc::new(RuntimeFolderTrustController::new(
                self.trust_store_path.clone(),
                roots,
            )),
            supplemental_context,
        })
    }

    async fn commit_generation(&self, generation: u64) -> std::result::Result<(), AgentLoopError> {
        commit_checkpoint_root_generation(&self.checkpoint_root, generation).map_err(|_error| {
            AgentLoopError::Persistence("workspace generation marker could not commit".to_owned())
        })
    }

    async fn abort_generation(&self, generation: u64) -> std::result::Result<(), AgentLoopError> {
        abort_checkpoint_root_generation(&self.checkpoint_root, generation).map_err(|_error| {
            AgentLoopError::Persistence("workspace generation could not abort".to_owned())
        })
    }
}

impl RuntimeFolderTrustController {
    fn new(store_path: PathBuf, workspaces: Vec<PathBuf>) -> Self {
        Self {
            store: FolderTrustStore::new(store_path),
            workspaces,
        }
    }
}

fn trust_confirmation_token(assessments: &[rw_store::trust::FolderTrustAssessment]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rottweiler-folder-trust-confirmation-v1\0");
    for assessment in assessments {
        let workspace = assessment.workspace().as_os_str().as_encoded_bytes();
        hasher.update(
            &u64::try_from(workspace.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        hasher.update(workspace);
        hasher.update(assessment.executable_hash().as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn render_trust_assessments(assessments: &[rw_store::trust::FolderTrustAssessment]) -> String {
    assessments
        .iter()
        .enumerate()
        .map(|(index, assessment)| {
            assessment.render_prompt_with_workspace(&format!("@root/{index}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl FolderTrustController for RuntimeFolderTrustController {
    async fn execute(
        &self,
        operation: FolderTrustOperation,
    ) -> std::result::Result<String, AgentLoopError> {
        let store = self.store.clone();
        let workspaces = self.workspaces.clone();
        tokio::task::spawn_blocking(move || {
            let trust_error = |_error: rw_store::trust::FolderTrustError| {
                AgentLoopError::Persistence("folder trust operation failed".to_owned())
            };
            let assessments = workspaces
                .iter()
                .map(|workspace| store.assess(workspace).map_err(&trust_error))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            match operation {
                FolderTrustOperation::Status => Ok(render_trust_assessments(&assessments)),
                FolderTrustOperation::Grant { confirmation: None } => {
                    let token = trust_confirmation_token(&assessments);
                    Ok(format!(
                        "{}\nreview the exact inventory and confirm with `/trust grant {token}`\n",
                        render_trust_assessments(&assessments)
                    ))
                }
                FolderTrustOperation::Grant {
                    confirmation: Some(confirmation),
                } => {
                    let expected = trust_confirmation_token(&assessments);
                    if confirmation != expected {
                        return Err(AgentLoopError::InvalidConfiguration(
                            "folder trust confirmation is stale or does not match the current root inventories; run `/trust grant` again"
                                .to_owned(),
                        ));
                    }
                    store.grant_all(&assessments).map_err(&trust_error)?;
                    let current = workspaces
                        .iter()
                        .map(|workspace| store.assess(workspace).map_err(&trust_error))
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    Ok(format!(
                        "{}\nfolder trust granted for all workspace roots; executable project configuration activates in the next session\n",
                        render_trust_assessments(&current)
                    ))
                }
                FolderTrustOperation::Revoke => {
                    store.revoke_all(&workspaces).map_err(&trust_error)?;
                    let current = workspaces
                        .iter()
                        .map(|workspace| store.assess(workspace).map_err(&trust_error))
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    Ok(format!(
                        "{}\nfolder trust revoked for all workspace roots; executable project configuration unloads in the next session\n",
                        render_trust_assessments(&current)
                    ))
                }
            }
        })
        .await
        .map_err(|_error| {
            AgentLoopError::Persistence("folder trust operation failed".to_owned())
        })?
    }
}

enum ActiveCheckpoint {
    Known,
    Opaque(Vec<(usize, OpaqueMutation)>),
}

fn group_checkpoint_paths(
    stores: &[Arc<CheckpointStore>],
    paths: Vec<PathBuf>,
) -> std::result::Result<BTreeMap<usize, Vec<PathBuf>>, AgentLoopError> {
    let mut grouped = BTreeMap::<usize, Vec<PathBuf>>::new();
    for path in paths {
        let mut components = path.components();
        let first = components.next();
        let virtual_target = match first {
            Some(std::path::Component::Normal(value)) if value == "@root" => {
                let index = match components.next() {
                    Some(std::path::Component::Normal(value)) => value
                        .to_str()
                        .and_then(|value| value.parse::<usize>().ok())
                        .filter(|index| *index > 0 && *index < stores.len())
                        .ok_or_else(|| {
                            AgentLoopError::Persistence(format!(
                                "checkpoint path has an invalid workspace-root index: {}",
                                path.display()
                            ))
                        })?,
                    _ => {
                        return Err(AgentLoopError::Persistence(format!(
                            "checkpoint path has no workspace-root index: {}",
                            path.display()
                        )));
                    }
                };
                let relative = components.collect::<PathBuf>();
                if relative.as_os_str().is_empty() {
                    return Err(AgentLoopError::Persistence(format!(
                        "checkpoint path names a workspace root rather than a file: {}",
                        path.display()
                    )));
                }
                Some((index, relative))
            }
            Some(
                std::path::Component::Normal(_)
                | std::path::Component::ParentDir
                | std::path::Component::CurDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_),
            ) => None,
            _ => {
                return Err(AgentLoopError::Persistence(format!(
                    "checkpoint path is not a confined workspace-relative path: {}",
                    path.display()
                )));
            }
        };
        let (root_index, relative) = if let Some(target) = virtual_target {
            target
        } else {
            resolve_checkpoint_path(stores, &path)?
        };
        grouped.entry(root_index).or_default().push(relative);
    }
    Ok(grouped)
}

fn resolve_checkpoint_path(
    stores: &[Arc<CheckpointStore>],
    path: &Path,
) -> std::result::Result<(usize, PathBuf), AgentLoopError> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        stores[0].workspace_root().join(path)
    };
    let canonical = match std::fs::canonicalize(&candidate) {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = candidate.parent().ok_or_else(|| {
                AgentLoopError::Persistence(format!(
                    "checkpoint path has no parent: {}",
                    path.display()
                ))
            })?;
            let filename = candidate.file_name().ok_or_else(|| {
                AgentLoopError::Persistence(format!(
                    "checkpoint path has no file name: {}",
                    path.display()
                ))
            })?;
            std::fs::canonicalize(parent)
                .map(|parent| parent.join(filename))
                .map_err(|error| {
                    AgentLoopError::Persistence(format!(
                        "checkpoint path parent is unavailable for {}: {error}",
                        path.display()
                    ))
                })?
        }
        Err(error) => {
            return Err(AgentLoopError::Persistence(format!(
                "checkpoint path is unavailable for {}: {error}",
                path.display()
            )));
        }
    };
    let (root_index, root) = stores
        .iter()
        .enumerate()
        .filter(|(_, store)| canonical.starts_with(store.workspace_root()))
        .max_by_key(|(_, store)| store.workspace_root().components().count())
        .ok_or_else(|| {
            AgentLoopError::Persistence(format!(
                "checkpoint path escapes every workspace root: {}",
                path.display()
            ))
        })?;
    let relative = canonical
        .strip_prefix(root.workspace_root())
        .map_err(|_| AgentLoopError::Persistence("checkpoint root mismatch".to_owned()))?
        .to_path_buf();
    if relative.as_os_str().is_empty() {
        return Err(AgentLoopError::Persistence(format!(
            "checkpoint path names a workspace root rather than a file: {}",
            path.display()
        )));
    }
    Ok((root_index, relative))
}

fn checkpoint_display_path(root_index: usize, path: &str) -> String {
    if root_index == 0 {
        path.to_owned()
    } else {
        format!("@root/{root_index}/{path}")
    }
}

struct DurableCheckpointCoordinator {
    stores: Arc<Vec<Arc<CheckpointStore>>>,
    active: Mutex<HashMap<String, ActiveCheckpoint>>,
    rewinds: Mutex<HashMap<String, Vec<RewindHandle>>>,
}

impl DurableCheckpointCoordinator {
    #[cfg(test)]
    fn new(store: Arc<CheckpointStore>) -> Self {
        Self::from_stores(Arc::new(vec![store]))
    }

    fn from_stores(stores: Arc<Vec<Arc<CheckpointStore>>>) -> Self {
        Self {
            stores,
            active: Mutex::new(HashMap::new()),
            rewinds: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl MutationCheckpointCoordinator for DurableCheckpointCoordinator {
    async fn begin(
        &self,
        session_id: &SessionId,
        agent_turn: u64,
        tool_call_id: &str,
        scope: &MutationScope,
    ) -> std::result::Result<MutationCheckpoint, AgentLoopError> {
        if matches!(scope, MutationScope::None) {
            return Ok(MutationCheckpoint { id: None });
        }
        let session_id = session_id.0.clone();
        let scope = scope.clone();
        let stores = Arc::clone(&self.stores);
        let active = tokio::task::spawn_blocking(move || {
            Ok::<_, AgentLoopError>(match scope {
                MutationScope::None => unreachable!("none returned before the worker"),
                MutationScope::Paths(paths) => {
                    let grouped = group_checkpoint_paths(&stores, paths)?;
                    for (root_index, paths) in grouped {
                        stores[root_index]
                            .checkpoint_known(&session_id, agent_turn, paths)
                            .map_err(checkpoint_agent_error)?;
                    }
                    ActiveCheckpoint::Known
                }
                MutationScope::OpaqueWorkspace => {
                    let mut mutations = Vec::with_capacity(stores.len());
                    for (root_index, store) in stores.iter().enumerate() {
                        mutations.push((
                            root_index,
                            store
                                .begin_opaque_mutation(&session_id, agent_turn)
                                .map_err(checkpoint_agent_error)?,
                        ));
                    }
                    ActiveCheckpoint::Opaque(mutations)
                }
            })
        })
        .await
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))??;
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(tool_call_id.to_owned(), active);
        Ok(MutationCheckpoint {
            id: Some(tool_call_id.to_owned()),
        })
    }

    async fn finish(
        &self,
        checkpoint: &MutationCheckpoint,
        _outcome: MutationCheckpointOutcome,
    ) -> std::result::Result<(), AgentLoopError> {
        let Some(id) = &checkpoint.id else {
            return Ok(());
        };
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
            .ok_or_else(|| AgentLoopError::Persistence("unknown mutation checkpoint".to_owned()))?;
        if let ActiveCheckpoint::Opaque(mutations) = active {
            let stores = Arc::clone(&self.stores);
            tokio::task::spawn_blocking(move || {
                for (root_index, mutation) in mutations {
                    stores[root_index].finish_opaque_mutation(&mutation)?;
                }
                Ok::<_, rw_store::checkpoint::CheckpointError>(())
            })
            .await
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?
            .map_err(checkpoint_agent_error)?;
        }
        Ok(())
    }

    async fn prepare_apply_rewind(
        &self,
        session_id: &SessionId,
        to_turn: u64,
        operation_id: &str,
    ) -> std::result::Result<RewindCheckpoint, AgentLoopError> {
        let stores = Arc::clone(&self.stores);
        let session_id = session_id.0.clone();
        let operation_id_owned = operation_id.to_owned();
        let (handles, unrestorable_paths) = tokio::task::spawn_blocking(move || {
            let mut handles = Vec::with_capacity(stores.len());
            let mut unrestorable_paths = Vec::new();
            for store in stores.iter() {
                handles.push(
                    store
                        .prepare_rewind(&session_id, to_turn, &operation_id_owned)
                        .map_err(checkpoint_agent_error)?,
                );
            }
            for (root_index, (store, handle)) in stores.iter().zip(&handles).enumerate() {
                let commit = store.apply_rewind(handle).map_err(checkpoint_agent_error)?;
                unrestorable_paths.extend(commit.report.unrestorable.into_iter().map(
                    |(path, reason)| UnrestorablePath {
                        path: checkpoint_display_path(root_index, &path),
                        reason,
                    },
                ));
            }
            Ok::<_, AgentLoopError>((handles, unrestorable_paths))
        })
        .await
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))??;
        self.rewinds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(operation_id.to_owned(), handles);
        Ok(RewindCheckpoint {
            id: operation_id.to_owned(),
            unrestorable_paths,
        })
    }

    async fn acknowledge_rewind(
        &self,
        checkpoint: &RewindCheckpoint,
    ) -> std::result::Result<(), AgentLoopError> {
        let handles = self
            .rewinds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&checkpoint.id)
            .ok_or_else(|| AgentLoopError::Persistence("unknown rewind checkpoint".to_owned()))?;
        let stores = Arc::clone(&self.stores);
        tokio::task::spawn_blocking(move || {
            if handles.len() != stores.len() {
                return Err(rw_store::checkpoint::CheckpointError::CorruptRewindTransaction);
            }
            for (store, handle) in stores.iter().zip(&handles) {
                store.acknowledge_rewind(handle)?;
            }
            Ok::<_, rw_store::checkpoint::CheckpointError>(())
        })
        .await
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?
        .map_err(checkpoint_agent_error)
    }
}

fn checkpoint_agent_error(error: impl std::fmt::Display) -> AgentLoopError {
    AgentLoopError::Persistence(format!("checkpoint store failed: {error}"))
}

fn recover_rewind_transactions(
    checkpoints: &[Arc<CheckpointStore>],
    log: &mut SessionEventLog,
) -> Result<()> {
    let existing = log
        .load::<EngineEvent>()
        .map_err(|error| miette!("session log could not load for rewind recovery: {error}"))?;
    let operations = existing
        .iter()
        .filter_map(|event| match &event.event {
            EngineEvent::ConversationRewound { operation_id, .. } => Some(operation_id.clone()),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    let mut recovered = BTreeMap::<
        String,
        (
            String,
            u64,
            Vec<UnrestorablePath>,
            Vec<Option<RewindHandle>>,
        ),
    >::new();
    for (root_index, store) in checkpoints.iter().enumerate() {
        for commit in store
            .recover_rewinds()
            .map_err(|error| miette!("rewind recovery failed for root {root_index}: {error}"))?
        {
            let operation_id = commit.handle.operation_id.clone();
            let entry = recovered.entry(operation_id.clone()).or_insert_with(|| {
                (
                    commit.handle.session_id.clone(),
                    commit.target_turn,
                    Vec::new(),
                    vec![None; checkpoints.len()],
                )
            });
            if entry.0 != commit.handle.session_id || entry.1 != commit.target_turn {
                return Err(miette!(
                    "rewind recovery identity differs between workspace roots"
                ));
            }
            entry.2.extend(
                commit
                    .report
                    .unrestorable
                    .into_iter()
                    .map(|(path, reason)| UnrestorablePath {
                        path: checkpoint_display_path(root_index, &path),
                        reason,
                    }),
            );
            entry.3[root_index] = Some(commit.handle);
        }
    }
    for (operation_id, (session_id, target_turn, mut unrestorable_paths, mut handles)) in recovered
    {
        for (root_index, (store, handle)) in checkpoints.iter().zip(&mut handles).enumerate() {
            if handle.is_none() {
                let prepared = store
                    .prepare_rewind(&session_id, target_turn, &operation_id)
                    .map_err(|error| {
                        miette!("rewind recovery could not stage root {root_index}: {error}")
                    })?;
                let commit = store.apply_rewind(&prepared).map_err(|error| {
                    miette!("rewind recovery could not apply root {root_index}: {error}")
                })?;
                unrestorable_paths.extend(commit.report.unrestorable.into_iter().map(
                    |(path, reason)| UnrestorablePath {
                        path: checkpoint_display_path(root_index, &path),
                        reason,
                    },
                ));
                *handle = Some(prepared);
            }
        }
        if !operations.contains(&operation_id) {
            log.append(EngineEvent::ConversationRewound {
                meta: EventMeta {
                    protocol_version: SESSION_EVENT_VERSION,
                    session_id: SessionId(session_id.clone()),
                    sequence_id: SequenceId(log.next_sequence()),
                    emitted_at: SystemEventClock.emitted_at(),
                    caused_by: None,
                },
                to_agent_turn: target_turn,
                operation_id: operation_id.clone(),
                unrestorable_paths,
            })
            .map_err(|error| miette!("recovered rewind event could not persist: {error}"))?;
        }
        for (root_index, (store, handle)) in checkpoints.iter().zip(handles).enumerate() {
            store
                .acknowledge_rewind(
                    &handle.ok_or_else(|| miette!("rewind root {root_index} has no handle"))?,
                )
                .map_err(|error| {
                    miette!("recovered rewind root {root_index} could not acknowledge: {error}")
                })?;
        }
    }
    Ok(())
}

struct ProviderModel {
    provider: Arc<dyn Provider>,
    model_metadata: Option<rw_core::ProviderModelMetadata>,
    compaction: rw_core::CompactionConfig,
    budget: rw_core::BudgetConfig,
}

struct UnavailableHostedModel {
    alias: String,
    reason: String,
    compaction: rw_core::CompactionConfig,
    budget: rw_core::BudgetConfig,
}

impl ModelDriver for UnavailableHostedModel {
    fn stream(
        &self,
        _alias: &str,
        _request: ProviderRequest,
    ) -> std::result::Result<BoxEventStream, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(format!(
            "the interactive engine is ready, but its provider is unavailable: {}",
            self.reason
        )))
    }

    fn has_model_alias(&self, alias: &str) -> bool {
        alias == self.alias
    }

    fn compaction_config(&self) -> rw_core::CompactionConfig {
        self.compaction.clone()
    }

    fn budget_config(&self) -> rw_core::BudgetConfig {
        self.budget.clone()
    }
}

impl ProviderModel {
    fn new(
        provider: Arc<dyn Provider>,
        compaction: rw_core::CompactionConfig,
        budget: rw_core::BudgetConfig,
    ) -> Self {
        let model_metadata = provider.cached_model_metadata();
        Self {
            provider,
            model_metadata,
            compaction,
            budget,
        }
    }
}

impl ModelDriver for ProviderModel {
    fn stream(
        &self,
        _alias: &str,
        request: ProviderRequest,
    ) -> std::result::Result<BoxEventStream, AgentLoopError> {
        let provider = Arc::clone(&self.provider);
        Ok(Box::pin(async_stream::stream! {
            match provider.stream(request).await {
                Ok(mut stream) => {
                    while let Some(item) = stream.next().await {
                        yield item;
                    }
                }
                Err(error) => yield Err(error),
            }
        }))
    }

    fn context_metadata(&self, _alias: &str) -> rw_core::ModelContextMetadata {
        let capabilities = self.model_metadata.as_ref().map_or_else(
            || self.provider.capabilities(),
            |metadata| metadata.capabilities.clone(),
        );
        rw_core::ModelContextMetadata {
            max_context_tokens: capabilities.max_context_tokens,
            max_output_tokens: capabilities.max_output_tokens,
            cache_breakpoints: Some(capabilities.cache_breakpoints),
        }
    }

    fn compaction_config(&self) -> rw_core::CompactionConfig {
        self.compaction.clone()
    }

    fn budget_config(&self) -> rw_core::BudgetConfig {
        self.budget.clone()
    }

    fn cost(&self, _alias: &str, usage: rw_core::ModelTokenUsage) -> rw_core::Cost {
        self.model_metadata.as_ref().map_or_else(
            || rw_core::Cost::Unavailable {
                reason: "recorded provider accounting is unavailable".to_owned(),
            },
            |metadata| rw_core::cost_from_model_metadata(metadata, usage),
        )
    }
}

struct PromptRecordingModel {
    inner: Arc<dyn ModelDriver>,
    journal: Arc<PromptShapeJournal>,
}

impl ModelDriver for PromptRecordingModel {
    fn stream(
        &self,
        alias: &str,
        request: ProviderRequest,
    ) -> std::result::Result<BoxEventStream, AgentLoopError> {
        let cache_support = self
            .inner
            .context_metadata(alias)
            .cache_breakpoints
            .unwrap_or(CacheBreakpointSupport::None);
        self.journal
            .record_request(alias, &request, cache_support)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        self.inner.stream(alias, request)
    }

    fn context_metadata(&self, alias: &str) -> rw_core::ModelContextMetadata {
        self.inner.context_metadata(alias)
    }

    fn compaction_config(&self) -> rw_core::CompactionConfig {
        self.inner.compaction_config()
    }

    fn budget_config(&self) -> rw_core::BudgetConfig {
        self.inner.budget_config()
    }

    fn cost(&self, alias: &str, usage: rw_core::ModelTokenUsage) -> rw_core::Cost {
        self.inner.cost(alias, usage)
    }

    fn cost_for_reported_model(
        &self,
        alias: &str,
        reported_model: Option<&str>,
        usage: rw_core::ModelTokenUsage,
    ) -> rw_core::Cost {
        self.inner
            .cost_for_reported_model(alias, reported_model, usage)
    }

    fn cost_for_route(
        &self,
        alias: &str,
        route: Option<&str>,
        reported_model: Option<&str>,
        usage: rw_core::ModelTokenUsage,
    ) -> rw_core::Cost {
        self.inner
            .cost_for_route(alias, route, reported_model, usage)
    }
}

struct HistoricalPromptTool(ToolDescriptor);

#[async_trait]
impl Tool for HistoricalPromptTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.0.clone()
    }

    async fn execute(
        &self,
        _context: &ToolContext,
        _input: serde_json::Value,
    ) -> std::result::Result<ToolResult, ToolError> {
        Err(ToolError::InvalidInput(
            "historical prompt tools cannot execute".to_owned(),
        ))
    }
}

fn historical_tool_registry(profile: &PromptShapeProfile) -> Result<Arc<ToolRegistry>> {
    let mut registry = ToolRegistry::new();
    for tool in &profile.tools {
        registry
            .register(Arc::new(HistoricalPromptTool(ToolDescriptor {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
                capabilities: CapabilityManifest::default(),
            })))
            .map_err(|error| miette!("historical prompt tool could not register: {error}"))?;
    }
    Ok(Arc::new(registry))
}

struct ScriptProvider {
    name: String,
    scripts: Mutex<VecDeque<Vec<ProviderEvent>>>,
    event_delay: std::time::Duration,
    model_metadata: Option<rw_core::ProviderModelMetadata>,
    cache_support: CacheBreakpointSupport,
}

impl ScriptProvider {
    fn new(name: String, scripts: Vec<Vec<ProviderEvent>>, event_delay_ms: u64) -> Self {
        Self {
            name,
            scripts: Mutex::new(scripts.into()),
            event_delay: std::time::Duration::from_millis(event_delay_ms),
            model_metadata: None,
            cache_support: CacheBreakpointSupport::None,
        }
    }

    fn with_cache_support(mut self, cache_support: CacheBreakpointSupport) -> Self {
        self.cache_support = cache_support;
        self
    }

    #[cfg(test)]
    fn with_model_metadata(mut self, metadata: rw_core::ProviderModelMetadata) -> Self {
        self.model_metadata = Some(metadata);
        self
    }
}

#[async_trait]
impl Provider for ScriptProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calling: true,
            vision: false,
            thinking: true,
            cache_breakpoints: self.cache_support,
            max_context_tokens: Some(128_000),
            max_output_tokens: Some(16_384),
            wire_mode: WireMode::NormalizedReplay,
        }
    }

    async fn model_metadata(
        &self,
    ) -> std::result::Result<Option<rw_core::ProviderModelMetadata>, ProviderError> {
        Ok(self.model_metadata.clone())
    }

    fn cached_model_metadata(&self) -> Option<rw_core::ProviderModelMetadata> {
        self.model_metadata.clone()
    }

    async fn stream(
        &self,
        _request: ProviderRequest,
    ) -> std::result::Result<BoxEventStream, ProviderError> {
        let events = self
            .scripts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::ReplayMiss,
                    "scripted provider sequence is exhausted",
                )
            })?;
        let delay = self.event_delay;
        Ok(Box::pin(async_stream::stream! {
            for event in events {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                yield Ok(event);
            }
        }))
    }
}

#[derive(Clone)]
enum CommandFixtureMode {
    Live,
    Record {
        directory: PathBuf,
        redactor: FixtureRedactor,
    },
    Replay {
        directory: PathBuf,
    },
    Offline,
}

#[derive(Clone)]
struct ResolvedToolProxy {
    url: Url,
    upstream: UpstreamProxy,
}

fn resolve_tool_proxy(
    config: &Config,
    credentials_path: &Path,
    offline: bool,
    redactor: &FixtureRedactor,
) -> Result<Option<ResolvedToolProxy>> {
    if offline {
        return Ok(None);
    }
    let Some(configured) = config.network.proxy.as_deref() else {
        return Ok(None);
    };
    let url = Url::parse(configured)
        .map_err(|error| miette!("configured global proxy is invalid: {error}"))?;
    let mut upstream = UpstreamProxy::new(url.clone())
        .map_err(|error| miette!("configured global proxy is invalid: {error}"))?;
    match (
        config.network.proxy_username.as_deref(),
        config.network.proxy_password_credential.as_deref(),
    ) {
        (None, None) => {}
        (Some(username), Some(reference)) => {
            let resolved = CredentialManager::system(credentials_path)
                .resolve(&CredentialReference::new(reference))
                .map_err(|error| miette!("global proxy credential could not resolve: {error}"))?;
            for warning in resolved.warnings() {
                eprintln!("warning: {warning}");
            }
            let password = resolved.secret().expose_secret().clone();
            redactor.register_known_value(&password);
            upstream = upstream.with_basic_auth(username, &password);
        }
        _ => {
            return Err(miette!(
                "global proxy authentication requires username and password credential reference"
            ));
        }
    }
    Ok(Some(ResolvedToolProxy { url, upstream }))
}

struct SharedCommandFixtureRedactor(FixtureRedactor);

fn credential_shaped_environment_name(name: &str) -> bool {
    let normalized = name.to_ascii_uppercase().replace('-', "_");
    matches!(
        normalized.as_str(),
        "API_KEY"
            | "TOKEN"
            | "ACCESS_TOKEN"
            | "REFRESH_TOKEN"
            | "ID_TOKEN"
            | "AUTH_TOKEN"
            | "BEARER_TOKEN"
            | "SESSION_TOKEN"
            | "OAUTH_TOKEN"
            | "PASSWORD"
            | "SECRET"
            | "CLIENT_SECRET"
            | "PRIVATE_KEY"
            | "CREDENTIAL"
            | "CREDENTIALS"
            | "AUTHORIZATION"
            | "COOKIE"
    ) || normalized.ends_with("_API_KEY")
        || normalized.ends_with("_TOKEN")
        || normalized.ends_with("_PASSWORD")
        || normalized.ends_with("_SECRET")
        || normalized.ends_with("_PRIVATE_KEY")
        || normalized.ends_with("_CREDENTIAL")
        || normalized.ends_with("_CREDENTIALS")
}

fn register_credential_environment(redactor: &FixtureRedactor) {
    for (name, value) in std::env::vars_os() {
        let (Some(name), Some(value)) = (name.to_str(), value.to_str()) else {
            continue;
        };
        register_credential_environment_value(redactor, name, value);
    }
}

fn register_credential_environment_value(redactor: &FixtureRedactor, name: &str, value: &str) {
    if !value.is_empty() && credential_shaped_environment_name(name) {
        redactor.register_known_value(value);
    }
}

impl CommandFixtureRedactor for SharedCommandFixtureRedactor {
    fn redact(&self, value: &str) -> String {
        self.0.redact_text(value)
    }
}

struct SharedEngineSecretRedactor(FixtureRedactor);

impl rw_core::SecretRedactor for SharedEngineSecretRedactor {
    fn redact(&self, value: &str) -> String {
        self.0.redact_text(value)
    }
}

#[allow(clippy::too_many_lines)]
fn build_tools(
    workspace_roots: &[PathBuf],
    question_asker: Arc<dyn QuestionAsker>,
    offline: bool,
    global_proxy: Option<&ResolvedToolProxy>,
    command_fixture_mode: CommandFixtureMode,
    execution_lease: Arc<ExecutionLease>,
    command_safety: &Arc<CommandSafetyClassifier>,
) -> Result<BuiltTools> {
    let workspace = workspace_roots
        .first()
        .ok_or_else(|| miette!("tool composition requires a primary workspace"))?;
    let symbols = Arc::new(
        WorkspaceSymbolIndex::new(workspace_roots)
            .map_err(|error| miette!("symbol index could not start: {error}"))?,
    );
    let limits = ToolLimits::default();
    let todo = Arc::new(TodoTool::new(limits));
    let web_fetcher: Arc<dyn WebFetcher> = if offline {
        Arc::new(OfflineWebFetcher)
    } else {
        Arc::new(PolicyWebFetcher::new(false, global_proxy.cloned()))
    };
    let scratch = std::env::temp_dir().join(format!("rottweiler-sandbox-{}", std::process::id()));
    create_private_sandbox_scratch(&scratch)?;
    let mut sandbox_roots = workspace_roots.to_vec();
    sandbox_roots.push(scratch.clone());
    let sandbox_policy = Arc::new(
        SandboxPolicy::new(&sandbox_roots, SandboxNetworkPolicy::Deny)
            .map_err(|error| miette!("OS sandbox policy could not be built: {error}"))?,
    );
    // Each approved live command receives its own supervised proxy. macOS
    // binds Seatbelt to its exact port; Linux exposes that port only inside a
    // disposable user/network namespace and relays over a private Unix socket.
    // Replay/offline never probes, resolves credentials, or binds sockets.
    let policy_egress_available = command_mode_can_open_proxy(&command_fixture_mode)
        && probe_policy_egress().support == SandboxSupport::Enforced;
    let command_executor = || {
        Arc::new(
            TokioCommandExecutor::with_execution_lease(Arc::clone(&execution_lease))
                .sandboxed(Arc::clone(&sandbox_policy))
                .with_command_safety(Arc::clone(command_safety))
                .with_policy_egress(policy_egress_available)
                .with_upstream_proxy(global_proxy.map(|proxy| proxy.upstream.clone())),
        )
    };
    let bash: Arc<dyn Tool> =
        match command_fixture_mode {
            CommandFixtureMode::Live => Arc::new(
                BashTool::new(command_executor(), limits)
                    .with_command_safety(Arc::clone(command_safety)),
            ),
            CommandFixtureMode::Record {
                directory,
                redactor,
            } => Arc::new(
                BashTool::new(
                    Arc::new(
                        RecordingCommandExecutor::new_with_redactor(
                            command_executor(),
                            directory,
                            workspace,
                            Arc::new(SharedCommandFixtureRedactor(redactor)),
                        )
                        .map_err(|error| miette!("command recorder could not start: {error}"))?,
                    ),
                    limits,
                )
                .with_command_safety(Arc::clone(command_safety)),
            ),
            CommandFixtureMode::Replay { directory } => Arc::new(
                BashTool::new(
                    Arc::new(
                        ReplayCommandExecutor::load(directory, workspace)
                            .map_err(|error| miette!("command replay could not load: {error}"))?,
                    ),
                    limits,
                )
                .with_command_safety(Arc::clone(command_safety)),
            ),
            CommandFixtureMode::Offline => Arc::new(
                BashTool::new(
                    Arc::new(ReplayCommandExecutor::empty(workspace).map_err(|error| {
                        miette!("offline command replay could not start: {error}")
                    })?),
                    limits,
                )
                .with_command_safety(Arc::clone(command_safety)),
            ),
        };
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool::new(limits)),
        Arc::new(WriteTool::new(limits).with_symbol_index(Arc::clone(&symbols))),
        Arc::new(EditTool::new(limits).with_symbol_index(Arc::clone(&symbols))),
        Arc::new(MultiEditTool::new(limits).with_symbol_index(Arc::clone(&symbols))),
        Arc::new(GrepTool::new(limits)),
        Arc::new(GlobTool::new(limits)),
        Arc::new(LsTool::new(limits)),
        bash,
        Arc::new(WebFetchTool::new(web_fetcher, limits)),
        todo.clone(),
        Arc::new(AskUserTool::new(question_asker, limits)),
        Arc::new(LazySymbolsTool::new(Arc::clone(&symbols), limits)),
    ];
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry
            .register(tool)
            .map_err(|error| miette!("built-in tools could not register: {error}"))?;
    }
    Ok(BuiltTools {
        registry: Arc::new(registry),
        todo,
        _execution_lease: execution_lease,
    })
}

fn command_mode_can_open_proxy(mode: &CommandFixtureMode) -> bool {
    matches!(
        mode,
        CommandFixtureMode::Live | CommandFixtureMode::Record { .. }
    )
}

fn create_private_sandbox_scratch(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .map_err(|error| miette!("sandbox scratch directory could not be created: {error}"))?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| miette!("sandbox scratch directory could not be inspected: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(miette!(
            "sandbox scratch path must be a real directory, never a symlink"
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(miette!(
                "sandbox scratch directory must be owned by the current user"
            ));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
            |error| miette!("sandbox scratch permissions could not be secured: {error}"),
        )?;
    }
    Ok(())
}

struct BuiltTools {
    registry: Arc<ToolRegistry>,
    todo: Arc<TodoTool>,
    _execution_lease: Arc<ExecutionLease>,
}

struct LazySymbolsTool {
    inner: SymbolsTool,
    index: Arc<WorkspaceSymbolIndex>,
    initialized: tokio::sync::Mutex<bool>,
}

impl LazySymbolsTool {
    fn new(index: Arc<WorkspaceSymbolIndex>, limits: ToolLimits) -> Self {
        Self {
            inner: SymbolsTool::new(Arc::clone(&index), limits),
            index,
            initialized: tokio::sync::Mutex::new(false),
        }
    }
}

#[async_trait]
impl Tool for LazySymbolsTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.inner.descriptor()
    }

    async fn execute(
        &self,
        context: &ToolContext,
        input: serde_json::Value,
    ) -> std::result::Result<ToolResult, ToolError> {
        let mut initialized = self.initialized.lock().await;
        if !*initialized {
            let index = Arc::clone(&self.index);
            tokio::task::spawn_blocking(move || index.index_workspaces())
                .await
                .map_err(|error| ToolError::Intelligence(error.to_string()))?
                .map_err(|error| ToolError::Intelligence(error.to_string()))?;
            *initialized = true;
        }
        drop(initialized);
        self.inner.execute(context, input).await
    }
}

async fn restore_todo_state(
    conversation: &[Turn],
    workspace: &Path,
    session_id: &SessionId,
    todo: &Arc<TodoTool>,
) -> Result<()> {
    todo.clear_session(session_id).await;
    let context = ToolContext::new(workspace)
        .map_err(|error| miette!("todo restore context failed: {error}"))?
        .with_session_id(session_id.clone());
    let mut pending = HashMap::new();
    for turn in conversation {
        for block in &turn.blocks {
            match block {
                Block::ToolCall { id, name, args } if name == "todo" => {
                    pending.insert(id.0.clone(), args.clone());
                }
                Block::ToolResult {
                    id,
                    is_error: false,
                    ..
                } => {
                    if let Some(arguments) = pending.remove(&id.0) {
                        todo.execute(&context, arguments)
                            .await
                            .map_err(|error| miette!("persisted todo state is invalid: {error}"))?;
                    }
                }
                Block::ToolResult { id, .. } => {
                    pending.remove(&id.0);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

struct HeadlessQuestionAsker;

#[async_trait]
impl QuestionAsker for HeadlessQuestionAsker {
    async fn ask(
        &self,
        request: AskUserInput,
        _cancellation: CancellationToken,
    ) -> std::result::Result<String, ToolError> {
        if let Some(first) = request.options.first() {
            Ok(first.clone())
        } else if request.allow_free_text {
            Ok("No interactive answer is available in headless mode.".to_owned())
        } else {
            Err(ToolError::Interaction(
                "headless ask_user has no selectable default".to_owned(),
            ))
        }
    }
}

#[derive(Clone)]
struct PolicyWebFetcher {
    allow_loopback: bool,
    proxies: ProxySettings,
    corporate_proxy: Option<ResolvedToolProxy>,
}

struct ValidatedWebTarget {
    direct_pin: Option<(String, SocketAddr)>,
    proxy_pin: EgressPin,
}

struct OfflineWebFetcher;

#[async_trait]
impl WebFetcher for OfflineWebFetcher {
    async fn fetch(
        &self,
        _request: FetchRequest,
        _cancellation: CancellationToken,
    ) -> std::result::Result<FetchResponse, ToolError> {
        Err(ToolError::Network(
            "webfetch is disabled while replaying an offline fixture".to_owned(),
        ))
    }
}

impl PolicyWebFetcher {
    fn new(allow_loopback: bool, global_proxy: Option<ResolvedToolProxy>) -> Self {
        let configured_url = global_proxy.as_ref().map(|proxy| proxy.url.clone());
        Self {
            allow_loopback,
            proxies: ProxySettings {
                global: configured_url,
                per_provider: BTreeMap::new(),
                environment: ProxyEnvironment::capture(),
            },
            corporate_proxy: global_proxy,
        }
    }

    async fn validate_and_pin(
        &self,
        url: &Url,
        policy: &EgressPolicy,
    ) -> std::result::Result<ValidatedWebTarget, ToolError> {
        if !matches!(url.scheme(), "http" | "https")
            || url.username() != ""
            || url.password().is_some()
        {
            return Err(ToolError::Network(
                "webfetch requires an http(s) URL without userinfo".to_owned(),
            ));
        }
        let port = url
            .port_or_known_default()
            .ok_or_else(|| ToolError::Network("URL has no usable port".to_owned()))?;
        match url.host() {
            Some(Host::Ipv4(address)) => {
                self.validate_ip(IpAddr::V4(address))?;
                validate_egress_decision(
                    policy,
                    address.to_string().as_str(),
                    &[IpAddr::V4(address)],
                )?;
                let socket = SocketAddr::new(IpAddr::V4(address), port);
                Ok(ValidatedWebTarget {
                    direct_pin: None,
                    proxy_pin: EgressPin::new(&address.to_string(), port, vec![socket])
                        .map_err(|error| ToolError::Network(error.to_string()))?,
                })
            }
            Some(Host::Ipv6(address)) => {
                self.validate_ip(IpAddr::V6(address))?;
                validate_egress_decision(
                    policy,
                    address.to_string().as_str(),
                    &[IpAddr::V6(address)],
                )?;
                let socket = SocketAddr::new(IpAddr::V6(address), port);
                Ok(ValidatedWebTarget {
                    direct_pin: None,
                    proxy_pin: EgressPin::new(&address.to_string(), port, vec![socket])
                        .map_err(|error| ToolError::Network(error.to_string()))?,
                })
            }
            Some(Host::Domain(host)) => {
                let addresses = tokio::net::lookup_host((host, port))
                    .await
                    .map_err(|error| ToolError::Network(format!("DNS lookup failed: {error}")))?
                    .collect::<Vec<_>>();
                if addresses.is_empty() {
                    return Err(ToolError::Network("DNS returned no addresses".to_owned()));
                }
                for address in &addresses {
                    self.validate_ip(address.ip())?;
                }
                let ips = addresses.iter().map(SocketAddr::ip).collect::<Vec<_>>();
                validate_egress_decision(policy, host, &ips)?;
                Ok(ValidatedWebTarget {
                    direct_pin: Some((host.to_owned(), addresses[0])),
                    proxy_pin: EgressPin::new(host, port, addresses)
                        .map_err(|error| ToolError::Network(error.to_string()))?,
                })
            }
            None => Err(ToolError::Network("URL has no host".to_owned())),
        }
    }

    fn validate_ip(&self, address: IpAddr) -> std::result::Result<(), ToolError> {
        if self.allow_loopback && address.is_loopback() {
            return Ok(());
        }
        if is_public_ip(address) {
            Ok(())
        } else {
            Err(ToolError::Network(
                "local, private, reserved, and non-routable targets are blocked".to_owned(),
            ))
        }
    }
}

#[async_trait]
impl WebFetcher for PolicyWebFetcher {
    #[allow(clippy::too_many_lines)]
    async fn fetch(
        &self,
        mut request: FetchRequest,
        cancellation: CancellationToken,
    ) -> std::result::Result<FetchResponse, ToolError> {
        let original_origin = origin(&request.url);
        let mut policy = EgressPolicy::default().with_private_destinations(self.allow_loopback);
        let original_host = request
            .url
            .host_str()
            .ok_or_else(|| ToolError::Network("URL has no host".to_owned()))?;
        if !policy.allow_domain(original_host) {
            return Err(ToolError::Network(
                "webfetch requested an invalid network domain".to_owned(),
            ));
        }
        for redirect in 0..=MAX_REDIRECTS {
            if cancellation.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let validated = self.validate_and_pin(&request.url, &policy).await?;
            let mut outgoing = Vec::with_capacity(request.headers.len());
            for (name, value) in &request.headers {
                let lower = name.to_ascii_lowercase();
                if matches!(
                    lower.as_str(),
                    "host" | "connection" | "proxy-authorization"
                ) {
                    return Err(ToolError::Network(format!(
                        "webfetch header {name:?} is not allowed"
                    )));
                }
                if origin(&request.url) != original_origin
                    && !cross_origin_webfetch_header_is_safe(&lower)
                {
                    continue;
                }
                outgoing.push((name.clone(), value.clone()));
            }
            let proxy_resolution = self.proxies.resolve_global(&request.url);
            let mut supervised_proxy = None;
            let (proxy, dns_pin) = if let Some(resolution) = proxy_resolution {
                let upstream = self
                    .corporate_proxy
                    .as_ref()
                    .filter(|configured| configured.url == resolution.url)
                    .map_or_else(
                        || UpstreamProxy::new(resolution.url.clone()),
                        |configured| Ok(configured.upstream.clone()),
                    )
                    .map_err(|error| ToolError::Network(error.to_string()))?;
                let local = SupervisedEgressProxy::start_with_upstream_and_pins(
                    policy.clone(),
                    Some(upstream),
                    vec![validated.proxy_pin],
                )
                .map_err(|error| ToolError::Network(error.to_string()))?;
                let url = Url::parse(&local.url())
                    .map_err(|error| ToolError::Network(error.to_string()))?;
                supervised_proxy = Some(local);
                (Some(url), None)
            } else {
                (None, validated.direct_pin)
            };
            let response = tokio::select! {
                response = guarded_http_fetch(GuardedHttpFetchRequest {
                    url: request.url.clone(),
                    headers: outgoing,
                    proxy,
                    proxy_authentication: None,
                    dns_pin,
                    max_bytes: request.max_bytes,
                }) => {
                    response.map_err(|error| match error {
                        GuardedHttpFetchError::Provider(error) => {
                            ToolError::Network(error.to_string())
                        }
                        GuardedHttpFetchError::SizeLimit { limit } => {
                            ToolError::SizeLimit { limit }
                        }
                    })?
                },
                () = cancellation.cancelled() => return Err(ToolError::Cancelled),
            };
            drop(supervised_proxy);
            if is_redirect(response.status) {
                if redirect == MAX_REDIRECTS {
                    return Err(ToolError::Network(
                        "webfetch redirect limit exceeded".to_owned(),
                    ));
                }
                let location = response
                    .location
                    .as_deref()
                    .ok_or_else(|| ToolError::Network("redirect omitted Location".to_owned()))?
                    .to_owned();
                request.url = request
                    .url
                    .join(&location)
                    .map_err(|error| ToolError::Network(format!("invalid redirect: {error}")))?;
                continue;
            }
            return Ok(FetchResponse {
                status: response.status,
                final_url: response.final_url,
                content_type: response.content_type,
                body: response.body,
            });
        }
        Err(ToolError::Network("webfetch redirect loop".to_owned()))
    }
}

fn cross_origin_webfetch_header_is_safe(name: &str) -> bool {
    matches!(name, "accept" | "accept-language" | "user-agent")
}

fn validate_egress_decision(
    policy: &EgressPolicy,
    host: &str,
    addresses: &[IpAddr],
) -> std::result::Result<(), ToolError> {
    match policy.evaluate(host, addresses) {
        EgressDecision::Allowed => Ok(()),
        EgressDecision::ApprovalRequired => Err(ToolError::Network(format!(
            "network domain {host:?} requires a separate approval"
        ))),
        EgressDecision::HardDenied => Err(ToolError::Network(
            "local, private, reserved, and non-routable targets are blocked".to_owned(),
        )),
    }
}

fn origin(url: &Url) -> (String, String, Option<u16>) {
    (
        url.scheme().to_owned(),
        url.host_str().unwrap_or_default().to_ascii_lowercase(),
        url.port_or_known_default(),
    )
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => is_public_v6(address),
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || address.is_multicast()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..=19).contains(&b))
        || a >= 240)
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_v4(mapped);
    }
    if segments[..6] == [0, 0, 0, 0, 0, 0] {
        return is_public_v4(embedded_ipv4(segments[6], segments[7]));
    }
    if segments[0] == 0x0064 && segments[1] == 0xff9b {
        if segments[2..6] == [0, 0, 0, 0] {
            return is_public_v4(embedded_ipv4(segments[6], segments[7]));
        }
        return false;
    }
    if segments[0] == 0x2002 {
        return is_public_v4(embedded_ipv4(segments[1], segments[2]));
    }
    if segments[0] == 0x2001 && segments[1] == 0 {
        return false;
    }
    if matches!(segments[4], 0 | 0x0200) && segments[5] == 0x5efe {
        return is_public_v4(embedded_ipv4(segments[6], segments[7]));
    }
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || address.is_unique_local()
        || address.is_unicast_link_local()
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

fn embedded_ipv4(high: u16, low: u16) -> Ipv4Addr {
    let [a, b] = high.to_be_bytes();
    let [c, d] = low.to_be_bytes();
    Ipv4Addr::new(a, b, c, d)
}

#[allow(clippy::too_many_lines)]
async fn run_print(
    actor: &rw_core::SessionHandle,
    session_id: &str,
    prompt: &str,
    format: OutputFormat,
    perf_markers: bool,
) -> Result<Option<TurnStatus>> {
    let mut events = actor.subscribe();
    let dispatch_started = std::time::Instant::now();
    let actor_task = actor.clone();
    let prompt_task = prompt.to_owned();
    let dispatch = tokio::spawn(async move { actor_task.send_message(prompt_task).await });
    // Prime the subscription before awaiting the command completion. Otherwise
    // an initial durable replay can overtake the connection-scoped ACK.
    let first_event = events
        .recv()
        .await
        .map_err(|error| miette!("session event stream failed: {error}"))?;
    let disposition = dispatch
        .await
        .map_err(|error| miette!("message dispatch worker failed: {error}"))?
        .map_err(display_agent_error)?;
    let command_mode = disposition == MessageDisposition::Command;
    let waits_for_compaction = prompt
        .split_whitespace()
        .next()
        .is_some_and(|name| name == "/compact");
    let mut aggregate = PrintAggregate::new(session_id);
    let mut target_turn = None;
    let mut first_event = Some(first_event);
    loop {
        let event = if let Some(event) = first_event.take() {
            event
        } else {
            tokio::select! {
                event = events.recv() => event
                    .map_err(|error| miette!("session event stream failed: {error}"))?,
                signal = tokio::signal::ctrl_c() => {
                    signal.into_diagnostic()?;
                    if !actor.interrupt().await.map_err(display_agent_error)? {
                        return Err(miette!("interrupt received while no turn was running"));
                    }
                    continue;
                }
            }
        };
        if let EngineEvent::ToolApprovalNeeded {
            tool_call_id, diff, ..
        } = &event
        {
            let binding = diff.as_ref().map(|diff| ApprovalBinding {
                proposal_id: diff.proposal_id.clone(),
                arguments_hash: diff.arguments_hash.clone(),
                base_hash: diff.base_hash.clone(),
                diff_hash: diff.diff_hash.clone(),
            });
            actor
                .approve_bound(tool_call_id.0.clone(), ApprovalDecision::Deny, binding)
                .await
                .map_err(display_agent_error)?;
        }
        if let EngineEvent::QuestionAsked {
            question_id,
            questions,
            ..
        } = &event
            && let Some(question) = questions.first()
        {
            let answer = question.options.first().map_or_else(
                || "No interactive answer is available in headless mode.".to_owned(),
                |option| option.value.clone(),
            );
            actor
                .answer_question(question_id.clone(), vec![answer])
                .await
                .map_err(display_agent_error)?;
        }
        match format {
            OutputFormat::Text => render_text_event(&event, false)?,
            OutputFormat::StreamJson => write_json_line(&event)?,
            OutputFormat::Json => {}
        }
        if let EngineEvent::UserMessageAccepted {
            agent_turn,
            content,
            ..
        } = &event
            && content == prompt
        {
            target_turn = Some(agent_turn.to_string());
        }
        let target_finished = if command_mode {
            if waits_for_compaction {
                matches!(&event, EngineEvent::CompactionFinished { .. })
            } else {
                matches!(&event, EngineEvent::CommandFinished { .. })
            }
        } else {
            matches!(
                &event,
                EngineEvent::TurnFinished { turn_id, .. }
                    if Some(&turn_id.0) == target_turn.as_ref()
            )
        };
        if command_mode || target_turn.is_some() {
            aggregate.push(event);
        }
        if target_finished {
            if perf_markers {
                eprintln!(
                    "rw_perf_zero_latency_turn_us={}",
                    dispatch_started.elapsed().as_micros()
                );
            }
            break;
        }
    }
    if format == OutputFormat::Json {
        serde_json::to_writer(io::stdout().lock(), &aggregate).into_diagnostic()?;
        println!();
    } else if format == OutputFormat::Text && !aggregate.text.ends_with('\n') {
        println!();
    }
    Ok(aggregate.status)
}

#[derive(Serialize)]
struct PrintAggregate {
    session_id: String,
    status: Option<TurnStatus>,
    text: String,
    usage: Usage,
    events: Vec<EngineEvent>,
}

impl PrintAggregate {
    fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_owned(),
            status: None,
            text: String::new(),
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            events: Vec::new(),
        }
    }

    fn push(&mut self, event: EngineEvent) {
        match &event {
            EngineEvent::TextDelta { text, .. } => self.text.push_str(text),
            EngineEvent::TurnFinished { status, usage, .. } => {
                self.status = Some(status.clone());
                self.usage = usage.clone();
            }
            EngineEvent::CommandFinished { message, .. } => {
                self.text.push_str(message);
                self.text.push('\n');
            }
            _ => {}
        }
        self.events.push(event);
    }
}

fn write_json_line(value: &impl Serialize) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value).into_diagnostic()?;
    stdout.write_all(b"\n").into_diagnostic()?;
    stdout.flush().into_diagnostic()
}

fn render_text_event(event: &EngineEvent, repl: bool) -> Result<()> {
    match event {
        EngineEvent::TextDelta { text, .. } => {
            print!("{text}");
            io::stdout().flush().into_diagnostic()?;
        }
        EngineEvent::ToolOutputDelta { stream, chunk, .. } if repl => {
            if *stream == ToolOutputStream::Stderr {
                eprint!("{chunk}");
                io::stderr().flush().into_diagnostic()?;
            } else {
                print!("{chunk}");
                io::stdout().flush().into_diagnostic()?;
            }
        }
        EngineEvent::ContextSnapshotReady { snapshot, .. } => {
            println!(
                "{}",
                serde_json::to_string_pretty(snapshot).into_diagnostic()?
            );
        }
        EngineEvent::CostSnapshotReady { snapshot, .. } => {
            println!(
                "{}",
                serde_json::to_string_pretty(snapshot).into_diagnostic()?
            );
        }
        EngineEvent::PromptDumpReady { dump, .. } => {
            println!("{}", serde_json::to_string_pretty(dump).into_diagnostic()?);
        }
        EngineEvent::ContextItemPinned { item_id, .. } => {
            println!("pinned context item {}", item_id.0);
        }
        EngineEvent::ContextItemEvicted { item_id, .. } => {
            println!("evicted context item {}", item_id.0);
        }
        EngineEvent::CompactionStarted { reason, .. } => {
            println!("compaction started ({reason:?})");
        }
        EngineEvent::CompactionAttemptFinished { cost, .. } => {
            println!("compaction attempt accounted ({cost:?})");
        }
        EngineEvent::CompactionFinished {
            reclaimed_tokens, ..
        } => {
            println!("compaction finished; reclaimed {reclaimed_tokens} estimated tokens");
        }
        EngineEvent::BudgetStatusChanged {
            level,
            scope,
            current,
            limit,
            ..
        } => {
            eprintln!("budget {level:?} ({scope:?}): {current}/{limit}");
        }
        EngineEvent::CommandFinished { message, .. } => println!("{message}"),
        EngineEvent::GuardTriggered { message, .. } => {
            eprintln!("error: {message}");
        }
        EngineEvent::Error { error, .. } => eprintln!("error: {}", error.message),
        _ => {}
    }
    Ok(())
}

enum InputLine {
    Line(String),
    Interrupt,
    Eof,
    Error(String),
}

fn spawn_readline(
    history: PathBuf,
) -> Result<(
    mpsc::UnboundedReceiver<InputLine>,
    Box<dyn rustyline::ExternalPrinter + Send>,
)> {
    let (send, receive) = mpsc::unbounded_channel();
    let mut editor = DefaultEditor::new().into_diagnostic()?;
    let printer = editor.create_external_printer().into_diagnostic()?;
    let _ = editor.load_history(&history);
    std::thread::spawn(move || {
        loop {
            match editor.readline("rw> ") {
                Ok(line) => {
                    if !line.trim().is_empty() {
                        let _ = editor.add_history_entry(line.as_str());
                        if let Some(parent) = history.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = editor.save_history(&history);
                    }
                    if send.send(InputLine::Line(line)).is_err() {
                        break;
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    if send.send(InputLine::Interrupt).is_err() {
                        break;
                    }
                }
                Err(ReadlineError::Eof) => {
                    let _ = send.send(InputLine::Eof);
                    break;
                }
                Err(error) => {
                    let _ = send.send(InputLine::Error(error.to_string()));
                    break;
                }
            }
        }
        if let Some(parent) = history.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = editor.save_history(&history);
    });
    Ok((receive, Box::new(printer)))
}

#[allow(clippy::too_many_lines)]
async fn run_repl(
    actor: &rw_core::SessionHandle,
    storage_root: &Path,
    format: OutputFormat,
) -> Result<Option<TurnStatus>> {
    let mut events = actor.subscribe();
    let (mut input, mut printer) = spawn_readline(storage_root.join("history.txt"))?;
    let mut interactions = VecDeque::new();
    let mut last_status = None;
    loop {
        tokio::select! {
            maybe = input.recv() => {
                match maybe.unwrap_or(InputLine::Eof) {
                    InputLine::Line(line) => {
                        if let Some(interaction) = interactions.pop_front() {
                            match interaction {
                                PendingInteraction::Plan => {
                                    let (decision, revisions) = if line.trim().eq_ignore_ascii_case("approve")
                                        || line.trim().eq_ignore_ascii_case("y")
                                    {
                                        (rw_core::PlanDecision::Approve, None)
                                    } else {
                                        (rw_core::PlanDecision::Reject, Some(line))
                                    };
                                    let _ = actor
                                        .review_plan(decision, revisions)
                                        .await
                                        .map_err(display_agent_error)?;
                                }
                                PendingInteraction::Question { id, .. } => {
                                    let _ = actor
                                        .answer_question(id, vec![line])
                                        .await
                                        .map_err(display_agent_error)?;
                                }
                                PendingInteraction::Permission { tool_call_id, binding, .. } => {
                                    let decision = parse_approval(&line);
                                    let _ = actor
                                        .approve_bound(tool_call_id, decision, binding)
                                        .await
                                        .map_err(display_agent_error)?;
                                }
                            }
                            display_next_interaction(interactions.front(), printer.as_mut())?;
                            continue;
                        }
                        if matches!(line.trim(), "/exit" | "/quit") {
                            let _ = actor.interrupt().await;
                            break;
                        }
                        if line.trim().is_empty() {
                            continue;
                        }
                        actor.send_message(line).await.map_err(display_agent_error)?;
                    }
                    InputLine::Interrupt => {
                        if actor.interrupt().await.map_err(display_agent_error)? {
                            interactions.clear();
                        } else {
                            break;
                        }
                    }
                    InputLine::Eof => {
                        let _ = actor.interrupt().await;
                        break;
                    }
                    InputLine::Error(error) => return Err(miette!("readline failed: {error}")),
                }
            }
            event = events.recv() => {
                let event = event.map_err(|error| miette!("session event stream failed: {error}"))?;
                if let EngineEvent::ToolApprovalNeeded {
                    tool_call_id,
                    capabilities,
                    rationale,
                    diff,
                    ..
                } = &event {
                    let announce = interactions.is_empty();
                    interactions.push_back(PendingInteraction::Permission {
                        tool_call_id: tool_call_id.0.clone(),
                        capabilities: capabilities.clone(),
                        rationale: rationale.clone(),
                        binding: diff.as_ref().map(|diff| ApprovalBinding {
                            proposal_id: diff.proposal_id.clone(),
                            arguments_hash: diff.arguments_hash.clone(),
                            base_hash: diff.base_hash.clone(),
                            diff_hash: diff.diff_hash.clone(),
                        }),
                    });
                    if announce {
                        display_next_interaction(interactions.front(), printer.as_mut())?;
                    }
                }
                if let EngineEvent::QuestionAsked {
                    question_id,
                    questions,
                    ..
                } = &event
                    && let Some(question) = questions.first()
                {
                    let announce = interactions.is_empty();
                    interactions.push_back(PendingInteraction::Question {
                        id: question_id.clone(),
                        prompt: question.prompt.clone(),
                        options: question
                            .options
                            .iter()
                            .map(|option| option.label.clone())
                            .collect(),
                    });
                    if announce {
                        display_next_interaction(interactions.front(), printer.as_mut())?;
                    }
                }
                if let EngineEvent::PlanSubmitted { .. } = &event {
                    interactions.push_back(PendingInteraction::Plan);
                }
                if let EngineEvent::TurnFinished { status, .. } = &event {
                    last_status = Some(status.clone());
                    interactions.retain(|interaction| matches!(interaction, PendingInteraction::Plan));
                    display_next_interaction(interactions.front(), printer.as_mut())?;
                }
                if let Some(message) = repl_event_message(&event, format)? {
                    printer.print(message).into_diagnostic()?;
                }
            }
        }
    }
    Ok(last_status)
}

enum PendingInteraction {
    Plan,
    Question {
        id: QuestionId,
        prompt: String,
        options: Vec<String>,
    },
    Permission {
        tool_call_id: String,
        capabilities: Vec<ToolCapability>,
        rationale: String,
        binding: Option<ApprovalBinding>,
    },
}

fn display_next_interaction(
    interaction: Option<&PendingInteraction>,
    printer: &mut dyn rustyline::ExternalPrinter,
) -> Result<()> {
    let message = match interaction {
        Some(PendingInteraction::Plan) => {
            "plan submitted: type `approve` to enter Execute, or rejection feedback to stay in Plan\n".to_owned()
        }
        Some(PendingInteraction::Question {
            prompt, options, ..
        }) => {
            if options.is_empty() {
                format!("question: {prompt}\n")
            } else {
                format!("question: {prompt}\noptions: {}\n", options.join(" | "))
            }
        }
        Some(PendingInteraction::Permission {
            capabilities,
            rationale,
            ..
        }) => format!("allow {capabilities:?} ({rationale})? [y] once / [a] session / [p] project / [n] deny\n"),
        None => return Ok(()),
    };
    printer.print(message).into_diagnostic()
}

fn repl_event_message(event: &EngineEvent, format: OutputFormat) -> Result<Option<String>> {
    if format == OutputFormat::StreamJson {
        let mut message = serde_json::to_string(event).into_diagnostic()?;
        message.push('\n');
        return Ok(Some(message));
    }
    Ok(match event {
        EngineEvent::TextDelta { text, .. } | EngineEvent::ToolOutputDelta { chunk: text, .. } => {
            Some(text.clone())
        }
        EngineEvent::ContextSnapshotReady { snapshot, .. } => Some(format!(
            "{}\n",
            serde_json::to_string_pretty(snapshot).into_diagnostic()?
        )),
        EngineEvent::CostSnapshotReady { snapshot, .. } => Some(format!(
            "{}\n",
            serde_json::to_string_pretty(snapshot).into_diagnostic()?
        )),
        EngineEvent::ContextItemPinned { item_id, .. } => {
            Some(format!("pinned context item {}\n", item_id.0))
        }
        EngineEvent::ContextItemEvicted { item_id, .. } => {
            Some(format!("evicted context item {}\n", item_id.0))
        }
        EngineEvent::CompactionStarted { reason, .. } => {
            Some(format!("compaction started ({reason:?})\n"))
        }
        EngineEvent::CompactionAttemptFinished { cost, .. } => {
            Some(format!("compaction attempt accounted ({cost:?})\n"))
        }
        EngineEvent::CompactionFinished {
            reclaimed_tokens, ..
        } => Some(format!(
            "compaction finished; reclaimed {reclaimed_tokens} estimated tokens\n"
        )),
        EngineEvent::BudgetStatusChanged {
            level,
            scope,
            current,
            limit,
            ..
        } => Some(format!("budget {level:?} ({scope:?}): {current}/{limit}\n")),
        EngineEvent::CommandFinished { message, .. } => Some(format!("{message}\n")),
        EngineEvent::GuardTriggered { message, .. } => Some(format!("error: {message}\n")),
        EngineEvent::Error { error, .. } => Some(format!("error: {}\n", error.message)),
        _ => None,
    })
}

fn parse_approval(input: &str) -> ApprovalDecision {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "once" => ApprovalDecision::AllowOnce,
        "a" | "all" | "session" => ApprovalDecision::AllowSession,
        "p" | "project" => ApprovalDecision::AllowProject,
        _ => ApprovalDecision::Deny,
    }
}

fn refresh_session_index(storage_root: &Path) -> Result<()> {
    let sessions_root = storage_root.join("sessions");
    let mut projections = Vec::new();
    let mut accounting_entries = Vec::new();
    match std::fs::read_dir(&sessions_root) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.into_diagnostic()?;
                if !entry.file_type().into_diagnostic()?.is_dir() {
                    continue;
                }
                let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let log = SessionEventLog::open(storage_root, &id)
                    .map_err(|error| miette!("session {id:?} could not open: {error}"))?;
                let events = load_session_events(&log)?;
                projections.push(project_session(&id, &events, log.path()));
                accounting_entries.extend(project_accounting(&id, &events)?);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).into_diagnostic(),
    }
    SessionIndex::rebuild(storage_root, &projections, &accounting_entries)
        .map_err(|error| miette!("session index rebuild failed: {error}"))?;
    Ok(())
}

fn update_one_session_index(
    storage_root: &Path,
    session_id: &str,
    sink: &DurableEventSink,
) -> Result<()> {
    let events = sink.load()?;
    let path = storage_root
        .join("sessions")
        .join(session_id)
        .join("events.jsonl");
    let projection = project_session(session_id, &events, &path);
    SessionIndex::open(storage_root)
        .map_err(|error| miette!("session index could not open: {error}"))?
        .upsert(&projection)
        .map_err(|error| miette!("session index could not update: {error}"))?;
    let accounting_entries = project_accounting(session_id, &events)?;
    AccountingLedger::open(storage_root)
        .and_then(|ledger| ledger.reconcile(&accounting_entries))
        .map_err(|error| miette!("session accounting could not update: {error}"))
}

fn project_accounting(
    session_id: &str,
    events: &[EngineEvent],
) -> Result<Vec<TurnAccountingEntry>> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::TurnFinished {
                meta,
                turn_id,
                usage,
                cost,
                ..
            } => Some((meta, turn_id, usage, cost, AccountingAttribution::Main)),
            EngineEvent::CompactionFinished {
                meta,
                summary_turn_id,
                usage: Some(usage),
                cost: Some(cost),
                ..
            }
            | EngineEvent::CompactionAttemptFinished {
                meta,
                summary_turn_id,
                usage,
                cost,
            } => Some((
                meta,
                summary_turn_id,
                usage,
                cost,
                AccountingAttribution::Compaction,
            )),
            _ => None,
        })
        .map(|(meta, turn_id, usage, cost, attribution)| {
            let emitted_at_utc = UtcTimestamp::parse(meta.emitted_at.clone()).map_err(|error| {
                miette!(
                    "turn {} has a malformed accounting timestamp: {error}",
                    turn_id.0
                )
            })?;
            Ok(TurnAccountingEntry {
                session_id: session_id.to_owned(),
                turn_id: turn_id.clone(),
                sequence_id: meta.sequence_id,
                utc_day: emitted_at_utc.utc_day(),
                emitted_at_utc,
                attribution,
                usage: usage.clone(),
                cost: cost.clone(),
            })
        })
        .collect()
}

fn project_session(session_id: &str, events: &[EngineEvent], path: &Path) -> SessionProjection {
    let title = events
        .iter()
        .find_map(|event| match event {
            EngineEvent::UserMessageAccepted { content, .. } => Some(compact_title(content)),
            _ => None,
        })
        .unwrap_or_else(|| "New session".to_owned());
    let mut transcript = String::new();
    for event in events {
        match event {
            EngineEvent::UserMessageAccepted { content, .. } => {
                transcript.push_str("user: ");
                transcript.push_str(content);
                transcript.push('\n');
            }
            EngineEvent::TextDelta { text, .. } => transcript.push_str(text),
            EngineEvent::ToolCallFinished { output, .. } => {
                transcript.push_str("\ntool: ");
                append_tool_output(&mut transcript, output);
                transcript.push('\n');
            }
            _ => {}
        }
    }
    let updated_unix_ms = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or_else(|_| SystemTime::now())
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX);
    SessionProjection {
        summary: SessionSummary {
            id: session_id.to_owned(),
            title,
            updated_unix_ms,
            cost_micros: 0,
        },
        transcript,
        projected_through: events
            .last()
            .and_then(EngineEvent::meta)
            .map(|meta| meta.sequence_id),
    }
}

fn compact_title(content: &str) -> String {
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(80).collect()
}

fn append_tool_output(target: &mut String, output: &ToolOutput) {
    match output {
        ToolOutput::Text { text } => target.push_str(text),
        ToolOutput::Structured { value } => target.push_str(&value.to_string()),
        ToolOutput::Mixed { parts } => {
            let _ = std::fmt::Write::write_fmt(target, format_args!("{parts:?}"));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use rw_core::runtime_support::{
        FinishReason, PermissionDecision, Role, ToolCallId, ToolCapability, TurnMeta,
    };
    use rw_core::{Cost, TurnId};
    use tempfile::tempdir;

    #[test]
    fn replay_and_offline_command_modes_never_enable_command_egress() {
        assert!(!command_mode_can_open_proxy(&CommandFixtureMode::Replay {
            directory: PathBuf::from("fixtures"),
        }));
        assert!(!command_mode_can_open_proxy(&CommandFixtureMode::Offline));
        assert!(command_mode_can_open_proxy(&CommandFixtureMode::Live));
    }

    #[test]
    fn rejects_private_and_reserved_network_targets() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.1.1",
            "192.0.2.1",
            "100.64.0.1",
            "::1",
            "fc00::1",
            "2001:db8::1",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b::a00:1",
            "64:ff9b:1::1",
            "2002:a9fe:a9fe::1",
            "2001:0000::1",
            "2001:4860:4860:0:0200:5efe:a9fe:a9fe",
        ] {
            let address: IpAddr = address.parse().expect("fixture address");
            assert!(!is_public_ip(address), "{address} must be rejected");
        }
        assert!(is_public_ip("1.1.1.1".parse().expect("public address")));
        assert!(is_public_ip(
            "2606:4700:4700::1111".parse().expect("public address")
        ));
        assert!(is_public_ip(
            "64:ff9b::101:101".parse().expect("public NAT64 address")
        ));
    }

    #[test]
    fn webfetch_egress_requires_new_domain_approval_and_keeps_ssrf_hard_denied() {
        let public = "1.1.1.1".parse().expect("public address");
        let private = "169.254.169.254".parse().expect("metadata address");
        let mut policy = EgressPolicy::default();
        assert!(policy.allow_domain("example.com"));
        assert!(validate_egress_decision(&policy, "example.com", &[public]).is_ok());
        assert!(matches!(
            validate_egress_decision(&policy, "other.example", &[public]),
            Err(ToolError::Network(message)) if message.contains("separate approval")
        ));
        assert!(matches!(
            validate_egress_decision(&policy, "example.com", &[private]),
            Err(ToolError::Network(message)) if message.contains("private")
        ));
    }

    #[test]
    fn cross_origin_webfetch_redirects_drop_custom_credentials() {
        for credential in [
            "authorization",
            "cookie",
            "x-api-key",
            "x-auth-token",
            "proxy-authorization",
        ] {
            assert!(!cross_origin_webfetch_header_is_safe(credential));
        }
        for safe in ["accept", "accept-language", "user-agent"] {
            assert!(cross_origin_webfetch_header_is_safe(safe));
        }
    }

    #[tokio::test]
    async fn webfetch_chains_through_authenticated_proxy_after_target_pin() {
        use std::net::TcpListener;
        use std::thread;

        let corporate = TcpListener::bind("127.0.0.1:0").expect("corporate proxy");
        let address = corporate.local_addr().expect("corporate address");
        let worker = thread::spawn(move || {
            let (mut stream, _) = corporate.accept().expect("accept");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let length = stream.read(&mut buffer).expect("request");
                if length == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..length]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(bytes).expect("request UTF-8");
            assert!(request.starts_with("GET http://127.0.0.1:8/target HTTP/1.1\r\n"));
            assert!(request.contains(
                "\r\nProxy-Authorization: Basic dXNlcjp3ZWJmZXRjaC1zZWNyZXQtY2FuYXJ5\r\n"
            ));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .expect("response");
        });
        let url = Url::parse(&format!("http://{address}")).expect("proxy URL");
        let upstream = UpstreamProxy::new(url.clone())
            .expect("upstream")
            .with_basic_auth("user", "webfetch-secret-canary");
        let fetcher = PolicyWebFetcher::new(true, Some(ResolvedToolProxy { url, upstream }));
        let response = fetcher
            .fetch(
                FetchRequest {
                    url: Url::parse("http://127.0.0.1:8/target").expect("target URL"),
                    headers: BTreeMap::new(),
                    max_bytes: 64,
                },
                CancellationToken::default(),
            )
            .await
            .expect("proxy webfetch");
        worker.join().expect("proxy worker");
        assert_eq!(response.body, b"ok");
    }

    #[test]
    fn offline_tool_proxy_resolution_never_touches_credentials() {
        let mut config = Config::default();
        config.network.proxy = Some("http://127.0.0.1:9".to_owned());
        config.network.proxy_username = Some("user".to_owned());
        config.network.proxy_password_credential = Some("missing-secret".to_owned());
        let missing = PathBuf::from("/definitely/missing/credentials.toml");
        assert!(
            resolve_tool_proxy(&config, &missing, true, &FixtureRedactor::default())
                .expect("offline resolution")
                .is_none()
        );
    }

    #[test]
    fn headless_approval_parser_fails_closed() {
        assert_eq!(parse_approval("yes"), ApprovalDecision::AllowOnce);
        assert_eq!(parse_approval("session"), ApprovalDecision::AllowSession);
        assert_eq!(parse_approval("project"), ApprovalDecision::AllowProject);
        assert_eq!(parse_approval("anything else"), ApprovalDecision::Deny);
    }

    #[test]
    fn session_titles_are_bounded_and_single_line() {
        let title = compact_title(&format!("hello\n{}", "world ".repeat(30)));
        assert!(!title.contains('\n'));
        assert!(title.chars().count() <= 80);
    }

    #[test]
    fn accounting_projection_keeps_main_and_compaction_attribution() {
        let meta = |sequence| EventMeta {
            protocol_version: SESSION_EVENT_VERSION,
            session_id: SessionId("accounting-session".to_owned()),
            sequence_id: SequenceId(sequence),
            emitted_at: "2026-07-10T12:34:56.789Z".to_owned(),
            caused_by: None,
        };
        let usage = Usage {
            input_tokens: 11,
            output_tokens: 12,
            cache_read_tokens: 13,
            cache_write_tokens: 14,
            reasoning_tokens: 15,
        };
        let cost = Cost::AiCredits {
            credits_micros: 16,
            nominal_amount_micros: None,
            currency: None,
        };
        let entries = project_accounting(
            "accounting-session",
            &[
                EngineEvent::TurnFinished {
                    meta: meta(3),
                    turn_id: TurnId("1".to_owned()),
                    status: TurnStatus::Completed,
                    usage: usage.clone(),
                    cost: cost.clone(),
                },
                EngineEvent::CompactionAttemptFinished {
                    meta: meta(4),
                    summary_turn_id: TurnId("compact-attempt-1".to_owned()),
                    usage: usage.clone(),
                    cost: cost.clone(),
                },
                EngineEvent::CompactionFinished {
                    meta: meta(5),
                    summary_turn_id: TurnId("compact-1".to_owned()),
                    reclaimed_tokens: 20,
                    usage: Some(usage.clone()),
                    cost: Some(cost.clone()),
                },
            ],
        )
        .expect("accounting projection");

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].attribution, AccountingAttribution::Main);
        assert_eq!(entries[1].attribution, AccountingAttribution::Compaction);
        assert_eq!(entries[1].sequence_id, SequenceId(4));
        assert_eq!(entries[1].usage, usage);
        assert_eq!(entries[1].cost, cost);
        assert_eq!(entries[1].utc_day.as_str(), "2026-07-10");
        assert_eq!(entries[2].attribution, AccountingAttribution::Compaction);
        assert_eq!(entries[2].sequence_id, SequenceId(5));

        let root = tempdir().expect("accounting ledger root");
        let ledger = AccountingLedger::open(root.path()).expect("accounting ledger");
        ledger.reconcile(&entries).expect("initial reconciliation");
        ledger
            .reconcile(&entries)
            .expect("idempotent reconciliation");
        let persisted = ledger.entries().expect("persisted accounting entries");
        assert_eq!(persisted.len(), 3);
        assert_eq!(persisted[1].sequence_id, SequenceId(4));
        assert_eq!(persisted[2].sequence_id, SequenceId(5));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn historical_anthropic_prompt_shape_restores_cache_and_tool_schema_offline() {
        let root = tempdir().expect("prompt metadata root");
        let session_id = "historical-anthropic";
        std::fs::create_dir_all(root.path().join("sessions").join(session_id))
            .expect("session directory");
        let journal = Arc::new(
            PromptShapeJournal::open(root.path(), session_id).expect("prompt-shape journal"),
        );
        journal.set_active_turn(TurnId("1".to_owned()));
        let system = Turn {
            role: Role::System,
            blocks: vec![Block::Text {
                text: "stable historical policy".to_owned(),
            }],
            meta: TurnMeta::default(),
        };
        let user = Turn {
            role: Role::User,
            blocks: vec![Block::Text {
                text: "HISTORICAL_PROMPT_SECRET".to_owned(),
            }],
            meta: TurnMeta::default(),
        };
        let tool = ToolDefinition {
            name: "historic_read".to_owned(),
            description: "Historical read schema".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"legacy_path": {"type": "string"}},
                "required": ["legacy_path"]
            }),
        };
        let request = ProviderRequest {
            model: "fast".to_owned(),
            turns: vec![system.clone(), user.clone()],
            tools: vec![tool.clone()],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 512,
            temperature: None,
            thinking: ThinkingLevel::Off,
            cache_hint: Some(rw_core::runtime_support::CacheHint {
                stable_prefix_turns: 1,
                tools_in_prefix: true,
            }),
        };
        journal
            .record_request("fast", &request, CacheBreakpointSupport::Explicit)
            .expect("record prompt shape");
        let metadata = std::fs::read_to_string(
            root.path()
                .join("sessions")
                .join(session_id)
                .join("prompt-shapes.json"),
        )
        .expect("prompt-shape metadata");
        assert!(!metadata.contains("HISTORICAL_PROMPT_SECRET"));
        let (profile, record) = journal
            .shape_for_turn(1)
            .expect("historical shape lookup")
            .expect("historical shape");
        assert_eq!(profile.cache_support, CacheBreakpointSupport::Explicit);
        assert_eq!(profile.cache_hint, request.cache_hint);
        assert_eq!(
            profile.cache_breakpoints,
            vec![PromptCacheBreakpoint {
                after_item_id: Some("system:0".to_owned()),
            }]
        );
        assert_eq!(profile.tools, vec![tool]);
        assert_eq!(
            journal
                .latest_shape()
                .expect("latest prompt shape")
                .expect("recorded latest prompt shape"),
            (profile.clone(), record.clone())
        );

        let provider: Arc<dyn Provider> = Arc::new(
            ScriptProvider::new("anthropic-history".to_owned(), Vec::new(), 0)
                .with_cache_support(profile.cache_support),
        );
        let model: Arc<dyn ModelDriver> = Arc::new(ProviderModel::new(
            provider,
            rw_core::CompactionConfig::default(),
            rw_core::BudgetConfig::default(),
        ));
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let actor = SessionActor::spawn(SessionActorConfig {
            session_id: SessionId(session_id.to_owned()),
            workspace_root: workspace,
            additional_workspace_roots: Vec::new(),
            workspace_generation: 0,
            initial_session_context: vec![system],
            model_alias: profile.model_alias.clone(),
            model,
            tools: historical_tool_registry(&profile).expect("historical tools"),
            permissions: Arc::new(PermissionGate::new(PermissionDecision::Allow)),
            hooks: Arc::new(builtin_hook_dispatcher().expect("hooks")),
            commands: Arc::new(builtin_command_registry().expect("commands")),
            event_sink: Arc::new(rw_core::NoopSessionEventSink::default()),
            event_clock: Arc::new(SystemEventClock),
            secret_redactor: Arc::new(rw_core::NoopSecretRedactor),
            checkpoints: Arc::new(rw_core::NoopMutationCheckpointCoordinator),
            folder_trust: Arc::new(rw_core::NoopFolderTrustController),
            workspace_roots: Arc::new(rw_core::NoopWorkspaceRootController),
            recovered: rw_core::SessionRecoveredState {
                conversation: vec![user],
                ..rw_core::SessionRecoveredState::default()
            },
            max_turns: 1,
            identical_tool_failure_limit: 1,
            max_output_tokens: 512,
            thinking: ThinkingLevel::Off,
            event_capacity: 32,
        })
        .expect("historical prompt actor");
        let dump = actor.dump_prompt(None).await.expect("historical dump");
        assert_eq!(dump.tools[0].input_schema, profile.tools[0].input_schema);
        assert_eq!(dump.cache_breakpoints.len(), 1);
        let tools = dump
            .tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
            })
            .collect::<Vec<_>>();
        validate_historical_prompt_shape(&dump, &tools, &profile, &record)
            .expect("recorded prompt shape must validate");
        assert_eq!(
            prompt_request_fingerprint(
                &dump.model_alias.0,
                &dump.turns,
                &tools,
                profile.cache_hint,
                profile.cache_support,
                &profile.cache_breakpoints,
            )
            .expect("dump fingerprint"),
            record.request_fingerprint
        );
        assert_ne!(
            prompt_request_fingerprint(
                &dump.model_alias.0,
                &dump.turns,
                &tools,
                profile.cache_hint,
                CacheBreakpointSupport::Automatic,
                &profile.cache_breakpoints,
            )
            .expect("provider-managed cache fingerprint"),
            record.request_fingerprint,
            "explicit and provider-managed cache modes must not share a fingerprint"
        );

        let mut mismatched_profile = profile.clone();
        mismatched_profile.cache_hint = Some(CacheHint {
            stable_prefix_turns: 2,
            tools_in_prefix: true,
        });
        mismatched_profile.cache_breakpoints = cache_breakpoints_for_hint(
            mismatched_profile.cache_hint,
            mismatched_profile.cache_support,
        );
        let mismatched_record = PromptShapeRecord {
            profile_id: hash_serialized(&mismatched_profile).expect("mismatched profile id"),
            request_fingerprint: prompt_request_fingerprint(
                &dump.model_alias.0,
                &dump.turns,
                &tools,
                mismatched_profile.cache_hint,
                mismatched_profile.cache_support,
                &mismatched_profile.cache_breakpoints,
            )
            .expect("mismatched fingerprint"),
        };
        let error = validate_historical_prompt_shape(
            &dump,
            &tools,
            &mismatched_profile,
            &mismatched_record,
        )
        .expect_err("a different stable boundary must fail closed");
        assert!(error.to_string().contains("recorded cache behavior"));
    }

    #[test]
    fn prompt_shape_sidecar_rejects_tampering_and_missing_profile_references() {
        let root = tempdir().expect("prompt metadata root");
        let session_id = "tampered-prompt-shape";
        let session_directory = root.path().join("sessions").join(session_id);
        std::fs::create_dir_all(&session_directory).expect("session directory");
        let journal = PromptShapeJournal::open(root.path(), session_id).expect("shape journal");
        journal.set_active_turn(TurnId("1".to_owned()));
        let request = ProviderRequest {
            model: "fast".to_owned(),
            turns: vec![Turn {
                role: Role::System,
                blocks: vec![Block::Text {
                    text: "stable policy".to_owned(),
                }],
                meta: TurnMeta::default(),
            }],
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 128,
            temperature: None,
            thinking: ThinkingLevel::Off,
            cache_hint: Some(CacheHint {
                stable_prefix_turns: 1,
                tools_in_prefix: false,
            }),
        };
        journal
            .record_request("fast", &request, CacheBreakpointSupport::Explicit)
            .expect("record prompt shape");

        let path = session_directory.join("prompt-shapes.json");
        let pristine = std::fs::read(&path).expect("prompt-shape bytes");
        let mut tampered: PromptShapeState =
            serde_json::from_slice(&pristine).expect("prompt-shape state");
        let profile_id = tampered.records["1"].profile_id.clone();
        tampered
            .profiles
            .get_mut(&profile_id)
            .expect("recorded profile")
            .cache_breakpoints[0]
            .after_item_id = Some("system:9".to_owned());
        std::fs::write(
            &path,
            serde_json::to_vec(&tampered).expect("tampered state"),
        )
        .expect("write tampered state");
        let error = PromptShapeJournal::open(root.path(), session_id)
            .expect_err("tampered profile must fail closed");
        assert!(error.to_string().contains("profile id does not match"));

        let mut missing_profile: PromptShapeState =
            serde_json::from_slice(&pristine).expect("prompt-shape state");
        missing_profile
            .records
            .get_mut("1")
            .expect("recorded turn")
            .profile_id = "0".repeat(64);
        std::fs::write(
            &path,
            serde_json::to_vec(&missing_profile).expect("missing profile state"),
        )
        .expect("write missing profile state");
        let error = PromptShapeJournal::open(root.path(), session_id)
            .expect_err("missing profile reference must fail closed");
        assert!(error.to_string().contains("references a missing profile"));
    }

    #[test]
    fn offline_provider_model_replays_subscription_and_ai_credit_accounting() {
        let capabilities = serde_json::json!({
            "tool_calling": true,
            "vision": false,
            "thinking": false,
            "cache_breakpoints": "none",
            "max_context_tokens": 128_000,
            "max_output_tokens": 16384,
            "wire_mode": "normalized_replay"
        });
        let metadata = |accounting: serde_json::Value, pricing: serde_json::Value| {
            serde_json::from_value::<rw_core::ProviderModelMetadata>(serde_json::json!({
                "capabilities": capabilities.clone(),
                "pricing": pricing,
                "accounting": accounting
            }))
            .expect("provider metadata fixture")
        };
        let usage = rw_core::ModelTokenUsage {
            input_tokens: 2,
            output_tokens: 0,
            cache_read_tokens: 1,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        };
        let subscription: Arc<dyn Provider> = Arc::new(
            ScriptProvider::new("subscription-replay".to_owned(), Vec::new(), 0)
                .with_model_metadata(metadata(
                    serde_json::json!({"kind": "subscription_quota"}),
                    serde_json::Value::Null,
                )),
        );
        let subscription_model = ProviderModel::new(
            subscription,
            rw_core::CompactionConfig::default(),
            rw_core::BudgetConfig::default(),
        );
        assert!(matches!(
            subscription_model.cost("fast", usage),
            Cost::SubscriptionQuota { used: Some(used), unit: Some(unit) }
                if used == "3" && unit == "tokens"
        ));

        let credits: Arc<dyn Provider> = Arc::new(
            ScriptProvider::new("credit-replay".to_owned(), Vec::new(), 0).with_model_metadata(
                metadata(
                    serde_json::json!({
                        "kind": "ai_credits",
                        "micros_usd_per_credit": 2
                    }),
                    serde_json::json!({
                        "display_name": "credit fixture",
                        "input_per_million_micros_usd": 1_000_000,
                        "output_per_million_micros_usd": 1_000_000,
                        "cache_read_per_million_micros_usd": 0
                    }),
                ),
            ),
        );
        let credit_model = ProviderModel::new(
            credits,
            rw_core::CompactionConfig::default(),
            rw_core::BudgetConfig::default(),
        );
        assert!(matches!(
            credit_model.cost("fast", usage),
            Cost::AiCredits {
                credits_micros: 1_000_000,
                nominal_amount_micros: Some(nominal),
                currency: Some(currency),
            } if nominal == "2" && currency == "USD"
        ));
    }

    #[test]
    fn protocol_interaction_queue_preserves_question_then_permission_order() {
        let mut interactions = VecDeque::from([
            PendingInteraction::Question {
                id: QuestionId("question-first".to_owned()),
                prompt: "first?".to_owned(),
                options: Vec::new(),
            },
            PendingInteraction::Permission {
                tool_call_id: "permission-second".to_owned(),
                capabilities: vec![ToolCapability::ReadFilesystem],
                rationale: "fixture".to_owned(),
                binding: None,
            },
        ]);
        let Some(PendingInteraction::Question { id, .. }) = interactions.pop_front() else {
            panic!("question must remain first");
        };
        assert_eq!(id.0, "question-first");
        assert!(matches!(
            interactions.pop_front(),
            Some(PendingInteraction::Permission { tool_call_id, .. })
                if tool_call_id == "permission-second"
        ));
    }

    #[test]
    fn per_session_checkpoint_namespaces_isolate_pending_recovery() {
        let root = tempdir().expect("root");
        let workspace_a = root.path().join("workspace-a");
        let workspace_b = root.path().join("workspace-b");
        std::fs::create_dir_all(&workspace_a).expect("workspace a");
        std::fs::create_dir_all(&workspace_b).expect("workspace b");
        std::fs::write(workspace_a.join("file.txt"), "a-before").expect("a file");
        std::fs::write(workspace_b.join("file.txt"), "b-before").expect("b file");
        let storage = root.path().join("storage");
        let session_a = "session-a";
        let session_b = "session-b";
        let root_a = checkpoint_root(&storage, &workspace_a, session_a);
        let root_b = checkpoint_root(&storage, &workspace_b, session_b);
        assert_ne!(root_a, root_b);
        let store_a = CheckpointStore::open(&root_a, &workspace_a).expect("store a");
        let store_b = CheckpointStore::open(&root_b, &workspace_b).expect("store b");
        let pending_a = store_a
            .begin_opaque_mutation(session_a, 1)
            .expect("pending a");
        let pending_b = store_b
            .begin_opaque_mutation(session_b, 1)
            .expect("pending b");
        std::fs::write(workspace_a.join("file.txt"), "a-after").expect("mutate a");
        std::fs::write(workspace_b.join("file.txt"), "b-after").expect("mutate b");

        let recovered = store_a.recover_opaque_mutations().expect("recover a only");
        assert_eq!(recovered.len(), 1);
        assert!(
            store_a.finish_opaque_mutation(&pending_a).is_err(),
            "a marker was consumed by its recovery"
        );
        store_b
            .finish_opaque_mutation(&pending_b)
            .expect("b marker must remain untouched");
        assert_eq!(
            std::fs::read_to_string(workspace_b.join("file.txt")).expect("b file"),
            "b-after"
        );

        for (store, session, workspace, prefix) in [
            (&store_a, session_a, &workspace_a, "a"),
            (&store_b, session_b, &workspace_b, "b"),
        ] {
            std::fs::write(workspace.join("file.txt"), format!("{prefix}-zero"))
                .expect("reset file");
            store
                .checkpoint_known(session, 10, [PathBuf::from("file.txt")])
                .expect("turn one checkpoint");
            std::fs::write(workspace.join("file.txt"), format!("{prefix}-one"))
                .expect("turn one edit");
            store
                .checkpoint_known(session, 11, [PathBuf::from("file.txt")])
                .expect("turn two checkpoint");
            std::fs::write(workspace.join("file.txt"), format!("{prefix}-two"))
                .expect("turn two edit");
        }
        let rewind_a = store_a
            .prepare_rewind(session_a, 9, "rewind-a-zero")
            .expect("stage rewind a");
        let rewind_b = store_b
            .prepare_rewind(session_b, 9, "rewind-b-zero")
            .expect("stage rewind b");
        let recovered_a = store_a.recover_rewinds().expect("recover rewind a only");
        assert_eq!(recovered_a.len(), 1);
        assert_eq!(recovered_a[0].handle, rewind_a);
        assert_eq!(
            std::fs::read_to_string(workspace_a.join("file.txt")).expect("rewound a"),
            "a-zero"
        );
        assert_eq!(
            std::fs::read_to_string(workspace_b.join("file.txt")).expect("untouched b"),
            "b-two"
        );
        store_b.apply_rewind(&rewind_b).expect("apply rewind b");
        assert_eq!(
            std::fs::read_to_string(workspace_b.join("file.txt")).expect("rewound b"),
            "b-zero"
        );
        store_a.acknowledge_rewind(&rewind_a).expect("ack rewind a");
        store_b.acknowledge_rewind(&rewind_b).expect("ack rewind b");
    }

    #[tokio::test]
    async fn durable_coordinator_rewinds_ten_edits_to_turn_three_byte_exactly() {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::write(workspace.join("state.txt"), b"turn-0\n").expect("initial state");
        let session = SessionId("session-rewind".to_owned());
        let store = Arc::new(
            CheckpointStore::open(
                &checkpoint_root(root.path(), &workspace, &session.0),
                &workspace,
            )
            .expect("checkpoint store"),
        );
        let coordinator = DurableCheckpointCoordinator::new(store);
        for turn in 1..=10_u64 {
            let checkpoint = coordinator
                .begin(
                    &session,
                    turn,
                    &format!("edit-{turn}"),
                    &MutationScope::Paths(vec![PathBuf::from("state.txt")]),
                )
                .await
                .expect("begin checkpoint");
            std::fs::write(
                workspace.join("state.txt"),
                format!("turn-{turn}\n").as_bytes(),
            )
            .expect("edit state");
            coordinator
                .finish(&checkpoint, MutationCheckpointOutcome::Completed)
                .await
                .expect("finish checkpoint");
        }
        let rewind = coordinator
            .prepare_apply_rewind(&session, 3, "rewind-test-3")
            .await
            .expect("apply rewind");
        assert_eq!(
            std::fs::read(workspace.join("state.txt")).expect("rewound bytes"),
            b"turn-3\n"
        );
        coordinator
            .acknowledge_rewind(&rewind)
            .await
            .expect("ack rewind");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn multi_root_checkpoints_restore_known_and_opaque_added_root_mutations() {
        let root = tempdir().expect("root");
        let primary = root.path().join("primary");
        let added = root.path().join("added");
        std::fs::create_dir_all(&primary).expect("primary");
        std::fs::create_dir_all(&added).expect("added");
        let primary = std::fs::canonicalize(primary).expect("canonical primary");
        let added = std::fs::canonicalize(added).expect("canonical added");
        let parent_sentinel = root.path().join("parent.txt");
        std::fs::write(&parent_sentinel, b"parent-before").expect("parent sentinel");
        let target = added.join("state.bin");
        std::fs::write(&target, b"added-before\0bytes").expect("added target");
        let session = SessionId("session-multi-root-rewind".to_owned());
        let checkpoint_root = checkpoint_root(root.path(), &primary, &session.0);
        let stores = open_checkpoint_stores(&checkpoint_root, &[primary.clone(), added.clone()])
            .expect("multi-root stores");
        assert!(
            open_checkpoint_stores(&checkpoint_root, &[added.clone(), primary.clone()]).is_err(),
            "persisted root order must reject reorder/replacement"
        );
        let coordinator = DurableCheckpointCoordinator::from_stores(stores);

        let known = coordinator
            .begin(
                &session,
                1,
                "known-added",
                &MutationScope::Paths(vec![PathBuf::from("@root/1/state.bin")]),
            )
            .await
            .expect("known checkpoint");
        std::fs::write(&target, b"known-after").expect("known mutation");
        coordinator
            .finish(&known, MutationCheckpointOutcome::Completed)
            .await
            .expect("finish known");
        let rewind = coordinator
            .prepare_apply_rewind(&session, 0, "rewind-known-added")
            .await
            .expect("rewind known");
        assert_eq!(
            std::fs::read(&target).expect("known restored"),
            b"added-before\0bytes"
        );
        coordinator
            .acknowledge_rewind(&rewind)
            .await
            .expect("ack known rewind");

        let sibling = coordinator
            .begin(
                &session,
                2,
                "known-added-sibling",
                &MutationScope::Paths(vec![PathBuf::from("../added/state.bin")]),
            )
            .await
            .expect("sibling checkpoint");
        std::fs::write(&target, b"sibling-after").expect("sibling mutation");
        coordinator
            .finish(&sibling, MutationCheckpointOutcome::Completed)
            .await
            .expect("finish sibling");
        let rewind = coordinator
            .prepare_apply_rewind(&session, 1, "rewind-known-sibling")
            .await
            .expect("rewind sibling");
        assert_eq!(
            std::fs::read(&target).expect("sibling restored"),
            b"added-before\0bytes"
        );
        coordinator
            .acknowledge_rewind(&rewind)
            .await
            .expect("ack sibling rewind");

        let escaped = coordinator
            .begin(
                &session,
                3,
                "parent-escape",
                &MutationScope::Paths(vec![PathBuf::from("@root/1/../parent.txt")]),
            )
            .await;
        assert!(
            escaped.is_err(),
            "checkpoint confinement must block parent escape"
        );
        assert_eq!(
            std::fs::read(&parent_sentinel).expect("parent remains"),
            b"parent-before"
        );

        let git = |arguments: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&added)
                .args(arguments)
                .status()
                .expect("git command");
            assert!(status.success(), "git command failed: {arguments:?}");
        };
        git(&["init", "--quiet"]);
        git(&["add", "state.bin"]);
        let opaque = coordinator
            .begin(&session, 4, "opaque-added", &MutationScope::OpaqueWorkspace)
            .await
            .expect("opaque checkpoint");
        std::fs::write(&target, b"opaque-after").expect("opaque mutation");
        coordinator
            .finish(&opaque, MutationCheckpointOutcome::Completed)
            .await
            .expect("finish opaque");
        let rewind = coordinator
            .prepare_apply_rewind(&session, 3, "rewind-opaque-added")
            .await
            .expect("rewind opaque");
        assert_eq!(
            std::fs::read(&target).expect("opaque restored"),
            b"added-before\0bytes"
        );
        coordinator
            .acknowledge_rewind(&rewind)
            .await
            .expect("ack opaque rewind");
        assert_eq!(
            std::fs::read(&parent_sentinel).expect("parent final"),
            b"parent-before"
        );
    }

    #[tokio::test]
    async fn runtime_trust_controller_persists_grant_and_revoke_for_slash_commands() {
        let root = tempdir().expect("root");
        let workspaces = [root.path().join("workspace"), root.path().join("added")];
        let configs = workspaces
            .each_ref()
            .map(|workspace| workspace.join(".rottweiler/config.toml"));
        for (index, config) in configs.iter().enumerate() {
            std::fs::create_dir_all(config.parent().expect("project parent"))
                .expect("project directory");
            std::fs::write(config, format!("[models]\ndefault = \"fast-{index}\"\n"))
                .expect("project config");
        }
        let workspaces = workspaces
            .map(|workspace| std::fs::canonicalize(workspace).expect("canonical workspace"));
        let ledger = root.path().join("private/trust.json");
        let controller = RuntimeFolderTrustController::new(ledger.clone(), workspaces.to_vec());

        let status = controller
            .execute(FolderTrustOperation::Status)
            .await
            .expect("status");
        assert_eq!(status.matches("state: Untrusted").count(), 2);
        for (index, workspace) in workspaces.iter().enumerate() {
            assert!(status.contains(&format!("@root/{index}")));
            assert!(!status.contains(&workspace.to_string_lossy().to_string()));
        }
        let preview = controller
            .execute(FolderTrustOperation::Grant { confirmation: None })
            .await
            .expect("grant preview");
        let stale_token = preview
            .split("`/trust grant ")
            .nth(1)
            .and_then(|tail| tail.split('`').next())
            .expect("confirmation token")
            .to_owned();
        std::fs::write(&configs[1], "[models]\ndefault = \"changed\"\n")
            .expect("change after preview");
        assert!(
            controller
                .execute(FolderTrustOperation::Grant {
                    confirmation: Some(stale_token),
                })
                .await
                .is_err(),
            "changed inventory must invalidate the bound confirmation"
        );
        assert!(
            !ledger.is_file(),
            "stale confirmation must not grant any root"
        );

        let preview = controller
            .execute(FolderTrustOperation::Grant { confirmation: None })
            .await
            .expect("fresh preview");
        assert!(preview.contains("config.toml"));
        let token = preview
            .split("`/trust grant ")
            .nth(1)
            .and_then(|tail| tail.split('`').next())
            .expect("fresh confirmation token")
            .to_owned();
        let granted = controller
            .execute(FolderTrustOperation::Grant {
                confirmation: Some(token),
            })
            .await
            .expect("confirmed grant");
        assert_eq!(granted.matches("state: Trusted").count(), 2);
        assert!(granted.contains("state: Trusted"));
        assert!(granted.contains("activates in the next session"));
        assert!(ledger.is_file(), "grant must persist the trust ledger");

        let revoked = controller
            .execute(FolderTrustOperation::Revoke)
            .await
            .expect("revoke");
        assert_eq!(revoked.matches("state: Untrusted").count(), 2);
        assert!(revoked.contains("unloads in the next session"));
        for output in [&preview, &granted, &revoked] {
            for workspace in &workspaces {
                assert!(!output.contains(&workspace.to_string_lossy().to_string()));
            }
        }
    }

    #[test]
    fn aborted_workspace_root_generation_is_retry_clean() {
        let root = tempdir().expect("root");
        let primary = root.path().join("primary");
        let added = root.path().join("added");
        let checkpoint = root.path().join("checkpoint");
        std::fs::create_dir(&primary).expect("primary");
        std::fs::create_dir(&added).expect("added");
        let primary = std::fs::canonicalize(primary).expect("canonical primary");
        let added = std::fs::canonicalize(added).expect("canonical added");
        open_checkpoint_stores(&checkpoint, std::slice::from_ref(&primary))
            .expect("base generation");
        let appended = vec![primary.clone(), added];
        append_checkpoint_root_generation(
            &checkpoint,
            std::slice::from_ref(&primary),
            &appended,
            1,
            2,
        )
        .expect("prepare generation");
        abort_checkpoint_root_generation(&checkpoint, 1).expect("abort generation");
        let recovered = load_checkpoint_root_generation(&checkpoint)
            .expect("load base")
            .expect("base generation");
        assert_eq!(recovered.generation, 0);
        assert_eq!(recovered.roots, vec![primary.clone()]);
        append_checkpoint_root_generation(
            &checkpoint,
            std::slice::from_ref(&primary),
            &appended,
            1,
            2,
        )
        .expect("retry same generation");
        abort_checkpoint_root_generation(&checkpoint, 1).expect("cleanup retry");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn live_root_generation_immediately_swaps_tools_sandbox_and_checkpoints() {
        let root = tempdir().expect("root");
        let primary = root.path().join("primary");
        let added = root.path().join("added");
        let private = root.path().join("private");
        std::fs::create_dir_all(&primary).expect("primary");
        std::fs::create_dir_all(&added).expect("added");
        std::fs::create_dir_all(&private).expect("private");
        let primary = std::fs::canonicalize(primary).expect("canonical primary");
        let added = std::fs::canonicalize(added).expect("canonical added");
        let checkpoint_root = private.join("checkpoint");
        open_checkpoint_stores(&checkpoint_root, std::slice::from_ref(&primary))
            .expect("initial checkpoint mapping");
        let lease = Arc::new(
            ExecutionLease::acquire(private.join("execution.lock")).expect("execution lease"),
        );
        let approvals = private.join("approvals.json");
        let permissions = Arc::new(
            PermissionGate::from_config(rw_core::PermissionConfig::default())
                .with_workspace_roots([&primary])
                .with_project_approval_file(approvals),
        );
        let controller = RuntimeWorkspaceRootController {
            checkpoint_root: checkpoint_root.clone(),
            question_asker: Arc::new(HeadlessQuestionAsker),
            offline: false,
            global_proxy: None,
            command_fixture_mode: CommandFixtureMode::Live,
            execution_lease: lease,
            command_safety: Arc::new(CommandSafetyClassifier::default()),
            trust_store_path: private.join("trust.json"),
        };
        let generation = rw_core::WorkspaceRootController::append_root(
            &controller,
            &added,
            std::slice::from_ref(&primary),
            0,
            1,
            permissions,
        )
        .await
        .expect("prepare generation");
        rw_core::WorkspaceRootController::commit_generation(&controller, 1)
            .await
            .expect("commit generation");
        let context = ToolContext::from_workspace_roots(&generation.roots).expect("tool context");
        let session = SessionId("live-root-test".to_owned());

        let known = generation
            .checkpoints
            .begin(
                &session,
                1,
                "write-added",
                &MutationScope::Paths(vec![PathBuf::from("@root/1/created.txt")]),
            )
            .await
            .expect("known checkpoint");
        generation
            .tools
            .resolve("write")
            .expect("write tool")
            .execute(
                &context,
                serde_json::json!({"path":"@root/1/created.txt","content":"live-root"}),
            )
            .await
            .expect("write added root");
        generation
            .checkpoints
            .finish(&known, MutationCheckpointOutcome::Completed)
            .await
            .expect("finish known");
        let listing = generation
            .tools
            .resolve("ls")
            .expect("ls tool")
            .execute(&context, serde_json::json!({"path":"."}))
            .await
            .expect("search roots");
        assert!(listing.content.contains("@root/1/created.txt"));
        assert!(
            generation
                .tools
                .resolve("write")
                .expect("write tool")
                .execute(
                    &context,
                    serde_json::json!({"path":"@root/1/../parent.txt","content":"escape"}),
                )
                .await
                .is_err()
        );
        let rewind = generation
            .checkpoints
            .prepare_apply_rewind(&session, 0, "rewind-live-root")
            .await
            .expect("rewind added root");
        assert!(!added.join("created.txt").exists());
        generation
            .checkpoints
            .acknowledge_rewind(&rewind)
            .await
            .expect("ack rewind");

        let opaque = generation
            .checkpoints
            .begin(&session, 2, "bash-added", &MutationScope::OpaqueWorkspace)
            .await
            .expect("opaque checkpoint");
        generation
            .tools
            .resolve("bash")
            .expect("bash tool")
            .execute(
                &context,
                serde_json::json!({"command":"printf shell > shell.txt","cwd":"@root/1"}),
            )
            .await
            .expect("sandboxed bash in added root");
        generation
            .checkpoints
            .finish(&opaque, MutationCheckpointOutcome::Completed)
            .await
            .expect("finish opaque");
        assert_eq!(
            std::fs::read(added.join("shell.txt")).expect("bash output"),
            b"shell"
        );
        let escaped = generation
            .tools
            .resolve("bash")
            .expect("bash tool")
            .execute(
                &context,
                serde_json::json!({"command":"printf escape > ../parent-shell.txt","cwd":"@root/1"}),
            )
            .await
            .expect("sandbox reports command exit");
        assert!(escaped.content.contains("exit code:"));
        assert!(!root.path().join("parent-shell.txt").exists());
        let rewind = generation
            .checkpoints
            .prepare_apply_rewind(&session, 1, "rewind-live-root-bash")
            .await
            .expect("rewind bash root");
        assert!(!added.join("shell.txt").exists());
        generation
            .checkpoints
            .acknowledge_rewind(&rewind)
            .await
            .expect("ack bash rewind");

        let pending = RuntimeWorkspaceRootController {
            checkpoint_root: checkpoint_root.clone(),
            question_asker: Arc::new(HeadlessQuestionAsker),
            offline: false,
            global_proxy: None,
            command_fixture_mode: CommandFixtureMode::Live,
            execution_lease: Arc::new(
                ExecutionLease::acquire(private.join("execution-2.lock")).expect("second lease"),
            ),
            command_safety: Arc::new(CommandSafetyClassifier::default()),
            trust_store_path: private.join("trust.json"),
        };
        let third = root.path().join("third");
        std::fs::create_dir(&third).expect("third root");
        let third = std::fs::canonicalize(third).expect("canonical third");
        let _prepared = rw_core::WorkspaceRootController::append_root(
            &pending,
            &third,
            &generation.roots,
            1,
            2,
            Arc::clone(&generation.permissions),
        )
        .await
        .expect("prepare uncommitted generation");
        let recovered =
            restore_persisted_workspace_roots(&checkpoint_root, &primary, &generation.roots, 1)
                .expect("recover committed generation")
                .expect("generation");
        assert_eq!(recovered.roots, generation.roots);
        assert!(!recovered.roots.contains(&third));
    }

    #[tokio::test]
    async fn rewind_event_reprojects_ephemeral_todo_state() {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let session = SessionId("session-todo-rewind".to_owned());
        let todo = Arc::new(TodoTool::new(ToolLimits::default()));
        let call = Turn {
            role: Role::Assistant,
            blocks: vec![Block::ToolCall {
                id: ToolCallId("todo-1".to_owned()),
                name: "todo".to_owned(),
                args: serde_json::json!({
                    "action": "replace",
                    "items": [{"id": "one", "content": "kept until rewind"}]
                }),
            }],
            meta: TurnMeta::default(),
        };
        let result = Turn {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                id: ToolCallId("todo-1".to_owned()),
                output: ToolOutput::Text {
                    text: "ok".to_owned(),
                },
                is_error: false,
            }],
            meta: TurnMeta::default(),
        };
        restore_todo_state(&[call.clone(), result.clone()], &workspace, &session, &todo)
            .await
            .expect("restore todo");
        let context = ToolContext::new(&workspace)
            .expect("tool context")
            .with_session_id(session.clone());
        let before = todo
            .execute(&context, serde_json::json!({"action": "list"}))
            .await
            .expect("list before rewind");
        assert_eq!(before.data["count"], 1);

        let log = SessionEventLog::open(root.path(), &session.0).expect("event log");
        let sink = DurableEventSink::new(log, root.path().to_owned(), session.0.clone())
            .expect("durable sink");
        sink.bind_todo(TodoRestoreBinding {
            todo: Arc::clone(&todo),
            workspace: workspace.clone(),
            session_id: session.clone(),
        });
        let fixture_meta = |sequence: u64| EventMeta {
            protocol_version: SESSION_EVENT_VERSION,
            session_id: session.clone(),
            sequence_id: SequenceId(sequence),
            emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
            caused_by: None,
        };
        for event in [
            EngineEvent::TurnStarted {
                meta: fixture_meta(0),
                turn_id: TurnId("1".to_owned()),
            },
            EngineEvent::ConversationTurnCommitted {
                meta: fixture_meta(1),
                agent_turn: 1,
                turn: call,
            },
            EngineEvent::ConversationTurnCommitted {
                meta: fixture_meta(2),
                agent_turn: 1,
                turn: result,
            },
            EngineEvent::TurnFinished {
                meta: fixture_meta(3),
                turn_id: TurnId("1".to_owned()),
                status: TurnStatus::Completed,
                usage: Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    reasoning_tokens: 0,
                },
                cost: Cost::Monetary {
                    amount_micros: 0,
                    currency: "USD".to_owned(),
                },
            },
            EngineEvent::ConversationRewound {
                meta: fixture_meta(4),
                to_agent_turn: 0,
                operation_id: "rewind-todo-0".to_owned(),
                unrestorable_paths: Vec::new(),
            },
        ] {
            sink.append(event).await.expect("fixture event append");
        }
        let after = todo
            .execute(&context, serde_json::json!({"action": "list"}))
            .await
            .expect("list after rewind");
        assert_eq!(after.data["count"], 0);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn session_handle_rewind_restores_ten_agent_edits_to_turn_three() {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let storage = root.path().join("storage");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let session = SessionId("session-direct-rewind".to_owned());
        let mut scripts = Vec::new();
        for turn in 1..=10_u64 {
            let id = format!("write-{turn}");
            scripts.push(vec![
                ProviderEvent::ToolCallStart {
                    id: id.clone(),
                    name: "write".to_owned(),
                },
                ProviderEvent::ToolCallEnd {
                    id,
                    arguments: serde_json::json!({
                        "path": "state.txt",
                        "content": format!("turn-{turn}\n"),
                    }),
                },
                ProviderEvent::Finished {
                    reason: FinishReason::ToolCalls,
                },
            ]);
            scripts.push(vec![ProviderEvent::Finished {
                reason: FinishReason::Stop,
            }]);
        }
        let scripted: Arc<dyn Provider> =
            Arc::new(ScriptProvider::new("direct-rewind".to_owned(), scripts, 0));
        let model: Arc<dyn ModelDriver> = Arc::new(ProviderModel::new(
            scripted,
            rw_core::CompactionConfig::default(),
            rw_core::BudgetConfig::default(),
        ));
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(WriteTool::new(ToolLimits::default())))
            .expect("write tool");
        let log = SessionEventLog::open(&storage, &session.0).expect("event log");
        let sink = Arc::new(
            DurableEventSink::new(log, storage.clone(), session.0.clone()).expect("durable sink"),
        );
        let checkpoints = Arc::new(DurableCheckpointCoordinator::new(Arc::new(
            CheckpointStore::open(
                &checkpoint_root(&storage, &workspace, &session.0),
                &workspace,
            )
            .expect("checkpoint store"),
        )));
        let actor = SessionActor::spawn(SessionActorConfig {
            session_id: session,
            workspace_root: workspace.clone(),
            additional_workspace_roots: Vec::new(),
            workspace_generation: 0,
            initial_session_context: Vec::new(),
            model_alias: "fast".to_owned(),
            model,
            tools: Arc::new(registry),
            permissions: Arc::new(PermissionGate::new(PermissionDecision::Allow)),
            hooks: Arc::new(builtin_hook_dispatcher().expect("hooks")),
            commands: Arc::new(builtin_command_registry().expect("commands")),
            event_sink: sink,
            event_clock: Arc::new(SystemEventClock),
            secret_redactor: Arc::new(rw_core::NoopSecretRedactor),
            checkpoints,
            folder_trust: Arc::new(rw_core::NoopFolderTrustController),
            workspace_roots: Arc::new(rw_core::NoopWorkspaceRootController),
            recovered: rw_core::SessionRecoveredState::default(),
            max_turns: 4,
            identical_tool_failure_limit: 5,
            max_output_tokens: 1024,
            thinking: ThinkingLevel::Off,
            event_capacity: 256,
        })
        .expect("session actor");
        let mut events = actor.subscribe();
        for turn in 1..=10_u64 {
            actor
                .send_message(format!("edit number {turn}"))
                .await
                .expect("start turn");
            loop {
                let event = events.recv().await.expect("turn event");
                if matches!(
                    event,
                    EngineEvent::TurnFinished {
                        turn_id,
                        status: TurnStatus::Completed,
                        ..
                    } if turn_id.0 == turn.to_string()
                ) {
                    break;
                }
            }
        }
        actor.rewind(3).await.expect("direct rewind");
        loop {
            let event = events.recv().await.expect("rewind event");
            if matches!(
                event,
                EngineEvent::ConversationRewound {
                    to_agent_turn: 3,
                    ..
                }
            ) {
                break;
            }
        }
        assert_eq!(
            std::fs::read(workspace.join("state.txt")).expect("rewound file"),
            b"turn-3\n"
        );
    }

    #[test]
    fn credential_shaped_environment_values_join_the_shared_redaction_set() {
        let redactor = FixtureRedactor::default();
        for (name, value) in [
            ("OPENAI_API_KEY", "api-canary"),
            ("MY_TOKEN", "token-canary"),
            ("SERVICE_SECRET", "secret-canary"),
            ("DB_PASSWORD", "password-canary"),
            ("SIGNING_PRIVATE_KEY", "private-key-canary"),
            ("NORMAL_SETTING", "visible-canary"),
            ("EMPTY_TOKEN", ""),
        ] {
            register_credential_environment_value(&redactor, name, value);
        }
        let redacted = redactor.redact_text(
            "api-canary token-canary secret-canary password-canary private-key-canary visible-canary",
        );
        for secret in [
            "api-canary",
            "token-canary",
            "secret-canary",
            "password-canary",
            "private-key-canary",
        ] {
            assert!(!redacted.contains(secret));
        }
        assert!(redacted.contains("visible-canary"));
        assert!(!credential_shaped_environment_name("MAX_TOKENS"));
        assert!(!credential_shaped_environment_name("TOKEN_COUNT"));
    }
}
