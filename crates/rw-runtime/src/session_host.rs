//! CLI composition for the headless multi-session engine host.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
#[cfg(unix)]
use std::{
    ffi::OsStr,
    io::Read as _,
    os::{
        fd::OwnedFd,
        unix::{
            ffi::OsStrExt as _,
            fs::{MetadataExt as _, PermissionsExt as _},
            process::CommandExt as _,
        },
    },
    process::{Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::Instant,
};

use async_trait::async_trait;
#[cfg(unix)]
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
#[cfg(unix)]
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rw_core::{
    AttachmentData, CachedModelCatalog, CommandDescriptor, CompletedForkOperation, Config,
    CreateSessionRequest, EngineEvent, ForkOperationKey, ForkOperationState, ForkSessionRequest,
    HostError, HostQueryService, HostedSession, ModelAlias, ModelCatalogError,
    ModelCatalogSnapshot, ModelCatalogSource, PermissionDecision, PreparedForkOperation,
    ProviderAuthAttempt, ProviderAuthChallenge, ProviderAuthCompletion, ProviderLogin,
    ProviderLoginCancellation, ProviderModelCatalogSource, SessionDescriptor, SessionFactory,
    SessionId, TranscriptFormat, UserSettingDescriptor, WorkspaceDiff, WorkspaceFileMatch,
    WorkspaceFilePreview, WorkspaceStatus, begin_provider_login, builtin_command_registry,
    merge_model_catalog_provider, project_session_events, retain_model_catalog_provider,
};
use rw_store::catalog_cache::{load_model_catalog_cache, store_model_catalog_cache};
use rw_store::config::ConfigLoader;
use rw_store::session::{SessionIndex, SessionStoreError, UtcTimestamp};
use rw_types::{PermissionModeDescriptor as PermissionMode, config::ThinkingLevel};
use serde::{Deserialize, Serialize};

use crate::session_runtime::{
    HostedProviderMode, HostedSessionComposition, compose_hosted_actor,
    fork_hosted_session_storage, load_session_metadata_any, load_session_workspace_roots,
    new_session_id, remove_forked_session_storage,
};

const MAX_SEARCH_RESULTS: usize = 1_000;
const MAX_SEARCH_QUERY_BYTES: usize = 1_024;
const MAX_SESSION_RESULTS: usize = 10_000;
#[cfg(test)]
const MAX_PROVIDER_DISPLAY_NAME_BYTES: usize = 256;
#[cfg(unix)]
const MAX_SEARCH_ENTRIES: usize = 50_000;
#[cfg(unix)]
const MAX_IGNORE_FILE_BYTES: usize = 128 * 1024;
#[cfg(unix)]
const MAX_IGNORE_PATTERNS_PER_DIRECTORY: usize = 1_024;
#[cfg(unix)]
const MAX_IGNORE_PATTERN_BYTES: usize = 1_024;
#[cfg(unix)]
const MAX_GITDIR_POINTER_BYTES: usize = 4 * 1024;
#[cfg(unix)]
const MAX_GIT_STATUS_BYTES: usize = 512 * 1024;
#[cfg(unix)]
const MAX_CHANGED_PATHS: usize = 1_000;
#[cfg(unix)]
const GIT_STATUS_DEADLINE: Duration = Duration::from_millis(500);
#[cfg(unix)]
const GIT_READER_DEADLINE: Duration = Duration::from_millis(50);
#[cfg(unix)]
const GIT_DIFF_DEADLINE: Duration = Duration::from_millis(500);
const MAX_TEXT_PREVIEW_BYTES: usize = 1024 * 1024;
const MAX_PREVIEW_BYTES: usize = 5 * 1024 * 1024;
const QUERY_DEADLINE: Duration = Duration::from_millis(100);
// Durable session queries run on Tokio's shared blocking pool. Keep them
// bounded, but do not count ordinary blocking-pool scheduling contention
// against the interactive workspace-picker budget above.
const SESSION_QUERY_DEADLINE: Duration = Duration::from_secs(2);
const SESSION_INDEX_SEARCH_MAX_ATTEMPTS: usize = 5;
const SESSION_INDEX_SEARCH_RETRY_DELAY: Duration = Duration::from_millis(5);
const WORKSPACE_STATUS_DEADLINE: Duration = Duration::from_millis(750);
const WORKSPACE_DIFF_DEADLINE: Duration = Duration::from_secs(2);
const SESSION_EXPORT_DEADLINE: Duration = Duration::from_secs(30);
const FORK_JOURNAL_VERSION: u16 = 1;
const MAX_COMPLETED_FORK_OPERATIONS: usize = 4_096;
const MAX_PENDING_FORK_OPERATIONS: usize = 32;
const MAX_FORK_TEMP_FILES: usize = 16;
const MAX_FORK_JOURNAL_BYTES: usize = 256 * 1024;
fn unix_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ForkOperationJournal {
    version: u16,
    operation_id: String,
    stable_operation_id: String,
    client_id: rw_core::ClientId,
    request_id: rw_core::RequestId,
    payload_hash: String,
    updated_unix_ms: u64,
    parent: SessionDescriptor,
    child_model: ModelAlias,
    child_workspace_generation: u64,
    child_roots_digest: String,
    child_session_id: SessionId,
    at_turn: rw_core::TurnId,
    through_sequence: Option<rw_core::SequenceId>,
    include_idle_tail: bool,
    driver_client_id: rw_core::ClientId,
    canonical_workspace: PathBuf,
    workspace_digest: String,
    state: ForkJournalState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "phase")]
enum ForkJournalState {
    Prepared,
    StorageCommitted,
    Completed { result: Box<ForkJournalResult> },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ForkJournalResult {
    protocol_version: u16,
    command_ack_emitted_at: String,
    fork_event_emitted_at: String,
    acknowledged_session_id: SessionId,
    outcome: rw_core::CommandOutcome,
    parent_session_id: SessionId,
    child: SessionDescriptor,
    at_turn: rw_core::TurnId,
}

struct ExpectedForkState {
    model: ModelAlias,
    workspace_generation: u64,
    roots_digest: String,
    modes: rw_ext::ModeRegistry,
}

impl ForkJournalState {
    fn rank(&self) -> u8 {
        match self {
            Self::Prepared => 0,
            Self::StorageCommitted => 1,
            Self::Completed { .. } => 2,
        }
    }
}

struct ForkJournalLock {
    #[cfg(unix)]
    _file: fs::File,
    #[cfg(not(unix))]
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[derive(Clone)]
pub struct RuntimeHostOptions {
    pub storage_root: PathBuf,
    pub credentials_path: PathBuf,
    pub config: Config,
    pub allowed_workspaces: Vec<PathBuf>,
    pub permission_mode: Option<PermissionMode>,
    pub max_turns: usize,
    pub provider_mode: HostedProviderMode,
    pub dangerously_trust: bool,
    pub wait_for_execution_lease: bool,
}

impl RuntimeHostOptions {
    /// Loads reusable host composition options from the effective environment.
    ///
    /// # Errors
    /// Returns an error when configuration or private storage cannot be resolved.
    pub fn from_environment(
        allowed_workspaces: Vec<PathBuf>,
        dangerously_trust: bool,
        permission_mode: Option<PermissionMode>,
        max_turns: usize,
        provider_mode: HostedProviderMode,
        wait_for_execution_lease: bool,
    ) -> Result<Self, HostError> {
        let loader = ConfigLoader::from_environment()
            .map_err(|error| HostError::Persistence(error.to_string()))?;
        let credentials_path = loader.credentials_path().clone();
        let storage_root = credentials_path
            .parent()
            .ok_or_else(|| HostError::Persistence("configuration root is unavailable".to_owned()))?
            .to_path_buf();
        let loader = if dangerously_trust {
            loader.dangerously_trust_project()
        } else {
            loader
        };
        let config = loader
            .load()
            .map_err(|error| HostError::Persistence(error.to_string()))?
            .config;
        Ok(Self {
            storage_root,
            credentials_path,
            config,
            allowed_workspaces,
            permission_mode,
            max_turns,
            provider_mode,
            dangerously_trust,
            wait_for_execution_lease,
        })
    }
}

#[derive(Clone)]
pub struct RuntimeSessionFactory {
    wasm_workers: Arc<rw_ext::WasmWorkerPool>,
    index_pool: Arc<rw_tools::WorkspaceIndexPool>,
    journal_reads: Arc<crate::journal_reads::JournalReads>,
    options: Arc<RuntimeHostOptions>,
    allowed_workspaces: Arc<Vec<PathBuf>>,
    model_catalog: Arc<CachedModelCatalog>,
}

struct PersistingModelCatalogSource {
    inner: Arc<dyn ModelCatalogSource>,
    cache_path: PathBuf,
}

enum EditableSettingKey {
    KeybindingPreset,
    ProjectDefaultModel,
    Theme,
    ModelThinking(String),
    AutomaticCompaction,
    DefaultPermission,
    SessionCostCap,
    DailyCostCap,
    SessionTokenCap,
    DailyTokenCap,
    TokenRateAlarm,
    BudgetWarning,
    McpServerEnabled(String),
    McpAddHttp(String),
}

impl EditableSettingKey {
    const KEYBINDING_PRESET: &'static str = "ui.keybindings.preset";
    const PROJECT_DEFAULT_MODEL: &'static str = "project.models.default";
    const THEME: &'static str = "ui.theme";
    const MODEL_THINKING_PREFIX: &'static str = "models.thinking.";
    const AUTOMATIC_COMPACTION: &'static str = "compaction.auto";
    const DEFAULT_PERMISSION: &'static str = "permissions.default";
    const SESSION_COST_CAP: &'static str = "budget.session_cost_cap_micros_usd";
    const DAILY_COST_CAP: &'static str = "budget.daily_cost_cap_micros_usd";
    const SESSION_TOKEN_CAP: &'static str = "budget.session_token_cap";
    const DAILY_TOKEN_CAP: &'static str = "budget.daily_token_cap";
    const TOKEN_RATE_ALARM: &'static str = "budget.token_rate_alarm_per_minute";
    const BUDGET_WARNING: &'static str = "budget.warn_at_percent";
    const MCP_SERVER_PREFIX: &'static str = "mcp.servers.";
    const MCP_SERVER_ENABLED_SUFFIX: &'static str = ".enabled";
    const MCP_ADD_HTTP_PREFIX: &'static str = "mcp.add_http.";

    fn parse(key: &str) -> Option<Self> {
        let fixed = match key {
            Self::KEYBINDING_PRESET => Some(Self::KeybindingPreset),
            Self::PROJECT_DEFAULT_MODEL => Some(Self::ProjectDefaultModel),
            Self::THEME => Some(Self::Theme),
            Self::AUTOMATIC_COMPACTION => Some(Self::AutomaticCompaction),
            Self::DEFAULT_PERMISSION => Some(Self::DefaultPermission),
            Self::SESSION_COST_CAP => Some(Self::SessionCostCap),
            Self::DAILY_COST_CAP => Some(Self::DailyCostCap),
            Self::SESSION_TOKEN_CAP => Some(Self::SessionTokenCap),
            Self::DAILY_TOKEN_CAP => Some(Self::DailyTokenCap),
            Self::TOKEN_RATE_ALARM => Some(Self::TokenRateAlarm),
            Self::BUDGET_WARNING => Some(Self::BudgetWarning),
            _ => None,
        };
        if let Some(fixed) = fixed {
            return Some(fixed);
        }
        if let Some(alias) = key.strip_prefix(Self::MODEL_THINKING_PREFIX) {
            return (!alias.is_empty()).then(|| Self::ModelThinking(alias.to_owned()));
        }
        if let Some(server) = key
            .strip_prefix(Self::MCP_SERVER_PREFIX)
            .and_then(|suffix| suffix.strip_suffix(Self::MCP_SERVER_ENABLED_SUFFIX))
            .filter(|server| {
                !server.contains('.') && rw_types::McpServerId::validate(server).is_ok()
            })
        {
            return Some(Self::McpServerEnabled(server.to_owned()));
        }
        key.strip_prefix(Self::MCP_ADD_HTTP_PREFIX)
            .filter(|server| rw_types::McpServerId::validate(server).is_ok())
            .map(|server| Self::McpAddHttp(server.to_owned()))
    }

    fn render(&self) -> String {
        match self {
            Self::KeybindingPreset => Self::KEYBINDING_PRESET.to_owned(),
            Self::ProjectDefaultModel => Self::PROJECT_DEFAULT_MODEL.to_owned(),
            Self::Theme => Self::THEME.to_owned(),
            Self::ModelThinking(alias) => format!("{}{alias}", Self::MODEL_THINKING_PREFIX),
            Self::AutomaticCompaction => Self::AUTOMATIC_COMPACTION.to_owned(),
            Self::DefaultPermission => Self::DEFAULT_PERMISSION.to_owned(),
            Self::SessionCostCap => Self::SESSION_COST_CAP.to_owned(),
            Self::DailyCostCap => Self::DAILY_COST_CAP.to_owned(),
            Self::SessionTokenCap => Self::SESSION_TOKEN_CAP.to_owned(),
            Self::DailyTokenCap => Self::DAILY_TOKEN_CAP.to_owned(),
            Self::TokenRateAlarm => Self::TOKEN_RATE_ALARM.to_owned(),
            Self::BudgetWarning => Self::BUDGET_WARNING.to_owned(),
            Self::McpServerEnabled(server) => format!(
                "{}{server}{}",
                Self::MCP_SERVER_PREFIX,
                Self::MCP_SERVER_ENABLED_SUFFIX
            ),
            Self::McpAddHttp(server) => format!("{}{server}", Self::MCP_ADD_HTTP_PREFIX),
        }
    }
}

#[async_trait]
impl ModelCatalogSource for PersistingModelCatalogSource {
    async fn discover(&self) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        let snapshot = self.inner.discover().await?;
        let cache_path = self.cache_path.clone();
        let cached = snapshot.clone();
        // The cache is explicitly non-authoritative. A live catalog remains a
        // successful result even if the private cache cannot be refreshed.
        let _ =
            tokio::task::spawn_blocking(move || store_model_catalog_cache(&cache_path, &cached))
                .await;
        Ok(snapshot)
    }

    async fn discover_provider(
        &self,
        provider: &str,
    ) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        let snapshot = self.inner.discover_provider(provider).await?;
        let cache_path = self.cache_path.clone();
        let provider = provider.to_owned();
        let cached = snapshot.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let durable = if let Some(base) = load_model_catalog_cache(&cache_path).ok().flatten() {
                merge_model_catalog_provider(base, cached, &provider)
            } else {
                let mut scoped = cached;
                retain_model_catalog_provider(&mut scoped, &provider);
                scoped
            };
            store_model_catalog_cache(&cache_path, &durable)
        })
        .await;
        Ok(snapshot)
    }
}

impl RuntimeSessionFactory {
    /// Builds a reusable session factory over an authorized workspace set.
    ///
    /// # Errors
    /// Returns an error when options are invalid or durable runtime state is unsafe.
    pub fn new(mut options: RuntimeHostOptions) -> Result<Self, HostError> {
        if options.max_turns == 0 || options.allowed_workspaces.is_empty() {
            return Err(HostError::Protocol(
                "host requires a turn limit and at least one authorized workspace".to_owned(),
            ));
        }
        let mut allowed = options
            .allowed_workspaces
            .iter()
            .map(|root| {
                fs::canonicalize(root)
                    .map_err(|_| HostError::Protocol("authorized workspace is invalid".to_owned()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        allowed.sort();
        allowed.dedup();
        options.allowed_workspaces.clone_from(&allowed);
        crate::session_runtime::initialize_private_storage_root(&options.storage_root)
            .map_err(|_| HostError::Persistence("host storage could not initialize".to_owned()))?;
        let live_source = ProviderModelCatalogSource::system_from_pricing_path(
            options.credentials_path.clone(),
            options.storage_root.join("models.toml"),
            options.config.clone(),
        );
        let catalog_cache_path = options.storage_root.join("model-catalog.json");
        let initial_catalog = load_model_catalog_cache(&catalog_cache_path)
            .ok()
            .flatten()
            .or_else(|| Some(ProviderModelCatalogSource::placeholder(&options.config)));
        let source: Arc<dyn ModelCatalogSource> = Arc::new(PersistingModelCatalogSource {
            inner: Arc::new(live_source),
            cache_path: catalog_cache_path,
        });
        let factory = Self {
            wasm_workers: rw_ext::WasmWorkerPool::new(),
            index_pool: Arc::new(rw_tools::WorkspaceIndexPool::default()),
            journal_reads: crate::journal_reads::JournalReads::new(&options.storage_root)
                .map_err(|error| HostError::Persistence(error.to_string()))?,
            options: Arc::new(options),
            allowed_workspaces: Arc::new(allowed),
            model_catalog: Arc::new(CachedModelCatalog::with_initial(source, initial_catalog)),
        };
        factory.recover_fork_operations()?;
        Ok(factory)
    }

    fn export_session_blocking(
        &self,
        session: &SessionDescriptor,
        format: TranscriptFormat,
        output_path: &Path,
        force: bool,
    ) -> Result<String, HostError> {
        let lease = self
            .journal_reads
            .capture(&session.session_id.0)
            .map_err(|error| HostError::Query(error.to_string()))?;
        let (events, _) = crate::history::load_events_from_view(
            &lease.view,
            &session.session_id.0,
            crate::history::MAX_HISTORY_BYTES,
        )
        .map_err(|error| HostError::Query(error.to_string()))?;
        let redactor = rw_providers::FixtureRedactor::default();
        crate::session_runtime::register_credential_environment(&redactor);
        let exported =
            crate::history::export_transcript(&session.session_id.0, &events, format, &redactor)
                .map_err(|error| HostError::Query(error.to_string()))?;
        let resolved = crate::history::write_transcript_export(
            &self.options.storage_root,
            output_path,
            &exported,
            force,
        )
        .map_err(|error| HostError::Query(error.to_string()))?;
        resolved.into_os_string().into_string().map_err(|_| {
            HostError::Query("resolved export output path is not valid UTF-8".to_owned())
        })
    }

    fn fork_journal_directory(&self) -> PathBuf {
        self.options
            .storage_root
            .join("control")
            .join("fork-operations")
    }

    fn settings_loader(&self) -> rw_store::config::ConfigLoader {
        rw_store::config::ConfigLoader::new(
            self.options.credentials_path.with_file_name("config.toml"),
            self.allowed_workspaces[0].join(".rottweiler/config.toml"),
        )
    }

    fn settings_loader_for(&self, workspace: &Path) -> rw_store::config::ConfigLoader {
        rw_store::config::ConfigLoader::new(
            self.options.credentials_path.with_file_name("config.toml"),
            workspace.join(".rottweiler/config.toml"),
        )
    }

    fn requested_model_for_compose(
        &self,
        workspace: &Path,
        model: Option<ModelAlias>,
        resume: bool,
    ) -> Result<Option<String>, HostError> {
        match (resume, model) {
            (true, model) => Ok(model.map(|model| model.0)),
            (false, Some(model)) => Ok(Some(model.0)),
            (false, None) => self
                .settings_loader_for(workspace)
                .tui_project_model()
                .map_err(|error| HostError::Persistence(error.to_string())),
        }
    }

    fn setting_descriptors(
        loaded: &rw_store::config::LoadedConfig,
        session: &SessionDescriptor,
        project_model: Option<&str>,
        keybinding_preset: &str,
        mcp_servers: &[(String, bool)],
    ) -> Vec<UserSettingDescriptor> {
        let alias = if loaded.config.models.aliases.contains_key(&session.model.0) {
            &session.model.0
        } else {
            &loaded.config.models.default
        };
        let theme_key = EditableSettingKey::Theme.render();
        let thinking_key = EditableSettingKey::ModelThinking(alias.to_owned()).render();
        let compaction_key = EditableSettingKey::AutomaticCompaction.render();
        let permission_key = EditableSettingKey::DefaultPermission.render();
        let provenance = |key: &str| {
            loaded
                .provenance(key)
                .map_or_else(|| "built-in".to_owned(), ToString::to_string)
        };
        let mut settings = vec![
            UserSettingDescriptor {
                key: EditableSettingKey::KeybindingPreset.render(),
                label: "Keybinding preset".to_owned(),
                value: keybinding_preset.to_owned(),
                choices: ["standard", "vim"].map(str::to_owned).to_vec(),
                provenance: "user keybindings".to_owned(),
                applies_immediately: false,
            },
            UserSettingDescriptor {
                key: EditableSettingKey::ProjectDefaultModel.render(),
                label: "Project default model".to_owned(),
                value: project_model.unwrap_or("not set").to_owned(),
                choices: project_model.into_iter().map(str::to_owned).collect(),
                provenance: "private project preference".to_owned(),
                applies_immediately: false,
            },
            UserSettingDescriptor {
                key: theme_key.clone(),
                label: "Theme".to_owned(),
                value: loaded.config.ui.theme.clone(),
                choices: Vec::new(),
                provenance: provenance(&theme_key),
                applies_immediately: false,
            },
            UserSettingDescriptor {
                key: thinking_key.clone(),
                label: format!("Thinking · {alias}"),
                value: loaded
                    .config
                    .models
                    .thinking
                    .get(alias)
                    .copied()
                    .unwrap_or_default()
                    .as_str()
                    .to_owned(),
                choices: [
                    ThinkingLevel::Off,
                    ThinkingLevel::Low,
                    ThinkingLevel::Medium,
                    ThinkingLevel::High,
                ]
                .map(|level| level.as_str().to_owned())
                .to_vec(),
                provenance: provenance(&thinking_key),
                applies_immediately: false,
            },
            UserSettingDescriptor {
                key: compaction_key.clone(),
                label: "Automatic compaction".to_owned(),
                value: loaded.config.compaction.auto.to_string(),
                choices: vec!["true".to_owned(), "false".to_owned()],
                provenance: provenance(&compaction_key),
                applies_immediately: false,
            },
            UserSettingDescriptor {
                key: permission_key.clone(),
                label: "Default permission".to_owned(),
                value: loaded.config.permissions.default.as_str().to_owned(),
                choices: [
                    PermissionDecision::Ask,
                    PermissionDecision::Allow,
                    PermissionDecision::Deny,
                ]
                .map(|decision| decision.as_str().to_owned())
                .to_vec(),
                provenance: provenance(&permission_key),
                applies_immediately: false,
            },
        ];
        settings.extend(budget_setting_descriptors(loaded));
        settings.extend(
            mcp_servers
                .iter()
                .map(|(server, enabled)| UserSettingDescriptor {
                    key: EditableSettingKey::McpServerEnabled(server.clone()).render(),
                    label: format!("MCP · {server}"),
                    value: enabled.to_string(),
                    choices: ["true", "false"].map(str::to_owned).to_vec(),
                    provenance: "user MCP configuration".to_owned(),
                    applies_immediately: false,
                }),
        );
        settings
    }

    fn fork_operation_id(key: &ForkOperationKey) -> String {
        let mut input = b"rw-fork-operation-v1\0".to_vec();
        input.extend_from_slice(&(key.operation_id.len() as u64).to_be_bytes());
        input.extend_from_slice(key.operation_id.as_bytes());
        blake3::hash(&input).to_hex().to_string()
    }

    fn ensure_fork_journal_directory(&self) -> Result<PathBuf, HostError> {
        let control = self.options.storage_root.join("control");
        for directory in [&control, &self.fork_journal_directory()] {
            match fs::symlink_metadata(directory) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(HostError::Persistence(
                        "fork journal path is unsafe".to_owned(),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match fs::create_dir(directory) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                            let metadata = fs::symlink_metadata(directory).map_err(|_| {
                                HostError::Persistence(
                                    "fork journal path is unavailable".to_owned(),
                                )
                            })?;
                            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                                return Err(HostError::Persistence(
                                    "fork journal path is unsafe".to_owned(),
                                ));
                            }
                        }
                        Err(_) => {
                            return Err(HostError::Persistence(
                                "fork journal could not initialize".to_owned(),
                            ));
                        }
                    }
                }
                Err(_) => {
                    return Err(HostError::Persistence(
                        "fork journal path is unavailable".to_owned(),
                    ));
                }
            }
        }
        let directory = self.fork_journal_directory();
        #[cfg(unix)]
        fs::set_permissions(
            &directory,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .map_err(|_| HostError::Persistence("fork journal permissions failed".to_owned()))?;
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|_| HostError::Persistence("fork journal is unavailable".to_owned()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(HostError::Persistence(
                "fork journal path is unsafe".to_owned(),
            ));
        }
        Ok(directory)
    }

    fn acquire_fork_journal_lock(&self) -> Result<ForkJournalLock, HostError> {
        let directory = self.ensure_fork_journal_directory()?;
        let control = directory.parent().ok_or_else(|| {
            HostError::Persistence("fork control directory is unavailable".to_owned())
        })?;
        #[cfg(unix)]
        {
            let descriptor = rustix::fs::open(
                control.join("fork-operations.lock"),
                rustix::fs::OFlags::RDWR
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::from_raw_mode(0o600),
            )
            .map_err(|_| HostError::Persistence("fork journal lock is unsafe".to_owned()))?;
            let stat = rustix::fs::fstat(&descriptor).map_err(|_| {
                HostError::Persistence("fork journal lock is unavailable".to_owned())
            })?;
            if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
                || stat.st_nlink != 1
                || stat.st_mode & 0o077 != 0
            {
                return Err(HostError::Persistence(
                    "fork journal lock is not private and regular".to_owned(),
                ));
            }
            let file = fs::File::from(descriptor);
            rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
                .map_err(|_| HostError::Persistence("fork journal lock failed".to_owned()))?;
            Ok(ForkJournalLock { _file: file })
        }
        #[cfg(not(unix))]
        {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            Ok(ForkJournalLock {
                _guard: LOCK
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            })
        }
    }

    fn is_lower_hex(value: &str) -> bool {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    fn expected_fork_state(
        &self,
        request: &ForkSessionRequest,
        workspace: &Path,
    ) -> Result<ExpectedForkState, HostError> {
        let metadata =
            load_session_metadata_any(&self.options.storage_root, &request.parent.session_id.0)
                .map_err(|_| {
                    HostError::Persistence("fork parent metadata is unavailable".to_owned())
                })?;
        if metadata.workspace != workspace {
            return Err(HostError::Persistence(
                "fork parent workspace does not match its operation".to_owned(),
            ));
        }
        let lease = self
            .journal_reads
            .capture(&request.parent.session_id.0)
            .map_err(|error| HostError::Persistence(error.to_string()))?;
        let (envelopes, _) = crate::history::load_events_from_view(
            &lease.view,
            &request.parent.session_id.0,
            crate::history::MAX_HISTORY_BYTES,
        )
        .map_err(|error| HostError::Persistence(error.to_string()))?;
        let fork_turn = request
            .at_turn
            .0
            .parse::<u64>()
            .map_err(|_| HostError::Persistence("fork turn is invalid".to_owned()))?;
        let boundary = if request.include_idle_tail {
            request.through_sequence
        } else if fork_turn == 0 {
            None
        } else {
            Some(
                envelopes
                    .iter()
                    .rev()
                    .find_map(|envelope| match &envelope.event {
                        EngineEvent::TurnFinished { turn_id, .. }
                            if *turn_id == request.at_turn =>
                        {
                            Some(envelope.sequence)
                        }
                        _ => None,
                    })
                    .ok_or_else(|| {
                        HostError::Persistence("fork turn boundary is unavailable".to_owned())
                    })?,
            )
        };
        let inherited_count = boundary.map_or(Ok(0_usize), |sequence| {
            usize::try_from(sequence.0)
                .map_err(|_| HostError::Persistence("fork boundary is invalid".to_owned()))?
                .checked_add(1)
                .ok_or_else(|| HostError::Persistence("fork boundary is invalid".to_owned()))
        })?;
        let inherited = envelopes.get(..inherited_count).ok_or_else(|| {
            HostError::Persistence("fork boundary exceeds its event log".to_owned())
        })?;
        let inherited_events = inherited
            .iter()
            .map(|envelope| envelope.event.clone())
            .collect::<Vec<_>>();
        // This generic pass consumes only the non-policy workspace generation
        // required to locate the historical extension roots. Mutation-capable
        // state is accepted only after the registry-aware pass below.
        let workspace_projection = project_session_events(&inherited_events)
            .map_err(|_| HostError::Persistence("fork event projection failed".to_owned()))?;
        let roots = crate::session_runtime::load_checkpoint_roots_exact(
            &crate::session_runtime::checkpoint_root(
                &self.options.storage_root,
                workspace,
                &request.parent.session_id.0,
            ),
            workspace_projection.workspace_generation,
        )
        .map_err(|_| HostError::Persistence("fork root generation is unavailable".to_owned()))?
        .ok_or_else(|| {
            HostError::Persistence("fork root generation is not committed".to_owned())
        })?;
        let (user_home, user_rottweiler) =
            crate::session_runtime::extension_user_roots(&self.options.credentials_path);
        let catalog = crate::session_runtime::discover_runtime_extensions(
            &roots,
            &self.options.storage_root.join("trust.json"),
            &user_home,
            &user_rottweiler,
            self.options.dangerously_trust,
        )
        .map_err(|_| HostError::Persistence("fork mode registry is unavailable".to_owned()))?;
        let validated = crate::mode_recovery::compose_and_project(&catalog, &inherited_events)
            .map_err(|_| HostError::Persistence("fork mode projection failed".to_owned()))?;
        let recovered = validated.recovered;
        let modes = validated.modes;
        let roots_digest =
            blake3::hash(&serde_json::to_vec(&roots).map_err(|_| {
                HostError::Persistence("fork roots could not serialize".to_owned())
            })?)
            .to_hex()
            .to_string();
        Ok(ExpectedForkState {
            model: ModelAlias(recovered.model_alias.unwrap_or(metadata.model_alias)),
            workspace_generation: recovered.workspace_generation,
            roots_digest,
            modes,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn validate_fork_journal(
        &self,
        journal: &ForkOperationJournal,
        path: &Path,
    ) -> Result<(), HostError> {
        let key = ForkOperationKey {
            operation_id: journal.stable_operation_id.clone(),
            client_id: journal.client_id.clone(),
            request_id: journal.request_id.clone(),
            payload_hash: journal.payload_hash.clone(),
        };
        let expected_id = Self::fork_operation_id(&key);
        let expected_filename = format!("{expected_id}.json");
        let canonical = self.authorize_workspace_path(&journal.canonical_workspace)?;
        let workspace_digest = blake3::hash(canonical.as_os_str().as_encoded_bytes())
            .to_hex()
            .to_string();
        let safe_text = |value: &str| {
            !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
        };
        let safe_session = |value: &str| {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        };
        let safe_operation = |value: &str| {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        };
        if journal.version != FORK_JOURNAL_VERSION
            || !Self::is_lower_hex(&journal.operation_id)
            || journal.operation_id != expected_id
            || path.file_name().and_then(|name| name.to_str()) != Some(&expected_filename)
            || !Self::is_lower_hex(&journal.payload_hash)
            || !Self::is_lower_hex(&journal.workspace_digest)
            || !Self::is_lower_hex(&journal.child_roots_digest)
            || journal.workspace_digest != workspace_digest
            || canonical != journal.canonical_workspace
            || !safe_text(&journal.client_id.0)
            || !safe_text(&journal.request_id.0)
            || !safe_operation(&journal.stable_operation_id)
            || !safe_text(&journal.child_model.0)
            || !safe_session(&journal.parent.session_id.0)
            || !safe_session(&journal.child_session_id.0)
            || journal.driver_client_id != journal.client_id
            || journal.at_turn.0.parse::<u64>().is_err()
            || workspace_name(&canonical) != journal.parent.workspace_name
        {
            return Err(HostError::Persistence(
                "fork journal validation failed".to_owned(),
            ));
        }
        if let ForkJournalState::Completed { result } = &journal.state {
            crate::session_runtime::validate_forked_session_commit(
                &self.options.storage_root,
                &journal.canonical_workspace,
                &journal.child_session_id.0,
                &journal.operation_id,
                &journal.parent.session_id.0,
            )
            .map_err(|_| {
                HostError::Persistence("completed fork storage validation failed".to_owned())
            })?;
            let child_metadata =
                load_session_metadata_any(&self.options.storage_root, &journal.child_session_id.0)
                    .map_err(|_| {
                        HostError::Persistence("completed fork metadata is unavailable".to_owned())
                    })?;
            let child_roots_digest = blake3::hash(
                &serde_json::to_vec(&child_metadata.workspace_roots).map_err(|_| {
                    HostError::Persistence("completed fork roots could not serialize".to_owned())
                })?,
            )
            .to_hex()
            .to_string();
            if result.protocol_version != rw_core::PROTOCOL_VERSION
                || UtcTimestamp::parse(result.command_ack_emitted_at.clone()).is_err()
                || UtcTimestamp::parse(result.fork_event_emitted_at.clone()).is_err()
                || result.outcome != rw_core::CommandOutcome::Accepted
                || result.acknowledged_session_id != journal.parent.session_id
                || result.parent_session_id != journal.parent.session_id
                || result.child.session_id != journal.child_session_id
                || result.child.workspace_name != journal.parent.workspace_name
                || result.child.model != journal.child_model
                || child_metadata.workspace_generation != journal.child_workspace_generation
                || child_roots_digest != journal.child_roots_digest
                || result
                    .child
                    .driver_client_id
                    .as_ref()
                    .is_none_or(|driver| !safe_text(&driver.0))
                || result.child.shell_active
                || !safe_text(&result.child.workspace_name)
                || !safe_text(&result.child.model.0)
                || result.at_turn != journal.at_turn
            {
                return Err(HostError::Persistence(
                    "completed fork result does not match its prepared operation".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn fork_journal_path(&self, key: &ForkOperationKey) -> Result<PathBuf, HostError> {
        Ok(self
            .ensure_fork_journal_directory()?
            .join(format!("{}.json", Self::fork_operation_id(key))))
    }

    #[cfg(unix)]
    fn read_fork_journal_file(&self, filename: &std::ffi::OsStr) -> Result<Vec<u8>, HostError> {
        let root = rustix::fs::open(
            &self.options.storage_root,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| HostError::Persistence("storage root is unsafe".to_owned()))?;
        let control = rustix::fs::openat(
            &root,
            "control",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| HostError::Persistence("fork control directory is unsafe".to_owned()))?;
        let directory = rustix::fs::openat(
            &control,
            "fork-operations",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| HostError::Persistence("fork journal directory is unsafe".to_owned()))?;
        let file = rustix::fs::openat(
            &directory,
            filename,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| HostError::Persistence("fork journal file is unsafe".to_owned()))?;
        let stat = rustix::fs::fstat(&file)
            .map_err(|_| HostError::Persistence("fork journal metadata failed".to_owned()))?;
        if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
            || stat.st_nlink != 1
            || stat.st_mode & 0o077 != 0
            || usize::try_from(stat.st_size).unwrap_or(usize::MAX) > MAX_FORK_JOURNAL_BYTES
        {
            return Err(HostError::Persistence(
                "fork journal file is not private and regular".to_owned(),
            ));
        }
        let mut bytes = Vec::with_capacity(
            usize::try_from(stat.st_size)
                .unwrap_or(MAX_FORK_JOURNAL_BYTES)
                .min(MAX_FORK_JOURNAL_BYTES),
        );
        fs::File::from(file)
            .take((MAX_FORK_JOURNAL_BYTES as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| HostError::Persistence("fork journal could not read".to_owned()))?;
        if bytes.len() > MAX_FORK_JOURNAL_BYTES {
            return Err(HostError::Persistence(
                "fork journal exceeds its byte limit".to_owned(),
            ));
        }
        Ok(bytes)
    }

    #[cfg(not(unix))]
    fn read_fork_journal_file(&self, filename: &std::ffi::OsStr) -> Result<Vec<u8>, HostError> {
        let path = self.ensure_fork_journal_directory()?.join(filename);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| HostError::Persistence("fork journal file is unavailable".to_owned()))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || usize::try_from(metadata.len()).unwrap_or(usize::MAX) > MAX_FORK_JOURNAL_BYTES
        {
            return Err(HostError::Persistence(
                "fork journal file is unsafe".to_owned(),
            ));
        }
        fs::read(path).map_err(|_| HostError::Persistence("fork journal could not read".to_owned()))
    }

    fn load_fork_journal_unlocked(
        &self,
        key: &ForkOperationKey,
    ) -> Result<Option<ForkOperationJournal>, HostError> {
        let path = self.fork_journal_path(key)?;
        if !path.exists() {
            return Ok(None);
        }
        let filename = path.file_name().ok_or_else(|| {
            HostError::Persistence("fork journal filename is unavailable".to_owned())
        })?;
        let bytes = self.read_fork_journal_file(filename)?;
        let journal: ForkOperationJournal = serde_json::from_slice(&bytes)
            .map_err(|_| HostError::Persistence("fork journal is corrupt".to_owned()))?;
        self.validate_fork_journal(&journal, &path)?;
        let operation_id = Self::fork_operation_id(key);
        if journal.version != FORK_JOURNAL_VERSION
            || journal.operation_id != operation_id
            || journal.stable_operation_id != key.operation_id
        {
            return Err(HostError::Persistence(
                "fork journal identity is corrupt".to_owned(),
            ));
        }
        if journal.payload_hash != key.payload_hash {
            return Err(HostError::RequestConflict);
        }
        Ok(Some(journal))
    }

    #[cfg(test)]
    fn load_fork_journal(
        &self,
        key: &ForkOperationKey,
    ) -> Result<Option<ForkOperationJournal>, HostError> {
        let _lock = self.acquire_fork_journal_lock()?;
        self.load_fork_journal_unlocked(key)
    }

    fn persist_fork_journal(
        path: &Path,
        journal: &ForkOperationJournal,
        replace: bool,
    ) -> Result<(), HostError> {
        let bytes = serde_json::to_vec_pretty(journal)
            .map_err(|_| HostError::Persistence("fork journal could not serialize".to_owned()))?;
        if bytes.len() > MAX_FORK_JOURNAL_BYTES {
            return Err(HostError::Persistence(
                "fork journal exceeds its byte limit".to_owned(),
            ));
        }
        let directory = path.parent().ok_or_else(|| {
            HostError::Persistence("fork journal directory is unavailable".to_owned())
        })?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = directory.join(format!(".fork-{}-{nonce}.tmp", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|_| HostError::Persistence("fork journal could not create".to_owned()))?;
        let result = (|| {
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| HostError::Persistence("fork journal could not sync".to_owned()))?;
            #[cfg(unix)]
            {
                let directory_file = fs::File::open(directory).map_err(|_| {
                    HostError::Persistence("fork journal directory is unavailable".to_owned())
                })?;
                let temporary_name = temporary.file_name().ok_or_else(|| {
                    HostError::Persistence("fork journal temporary name is unavailable".to_owned())
                })?;
                let final_name = path.file_name().ok_or_else(|| {
                    HostError::Persistence("fork journal filename is unavailable".to_owned())
                })?;
                let rename = if replace {
                    rustix::fs::renameat(
                        &directory_file,
                        temporary_name,
                        &directory_file,
                        final_name,
                    )
                } else {
                    rustix::fs::renameat_with(
                        &directory_file,
                        temporary_name,
                        &directory_file,
                        final_name,
                        rustix::fs::RenameFlags::NOREPLACE,
                    )
                };
                rename.map_err(|error| {
                    if !replace && error == rustix::io::Errno::EXIST {
                        HostError::RequestConflict
                    } else {
                        HostError::Persistence("fork journal update could not commit".to_owned())
                    }
                })?;
                rustix::fs::fsync(&directory_file).map_err(|_| {
                    HostError::Persistence("fork journal directory could not sync".to_owned())
                })?;
            }
            #[cfg(not(unix))]
            {
                if !replace && path.exists() {
                    return Err(HostError::RequestConflict);
                }
                fs::rename(&temporary, path).map_err(|_| {
                    HostError::Persistence("fork journal update could not commit".to_owned())
                })?;
                fs::File::open(directory)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|_| {
                        HostError::Persistence("fork journal directory could not sync".to_owned())
                    })?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn persist_new_fork_journal(
        path: &Path,
        journal: &ForkOperationJournal,
    ) -> Result<(), HostError> {
        Self::persist_fork_journal(path, journal, false)
    }

    fn same_fork_operation(left: &ForkOperationJournal, right: &ForkOperationJournal) -> bool {
        left.version == right.version
            && left.operation_id == right.operation_id
            && left.child_model == right.child_model
            && left.child_workspace_generation == right.child_workspace_generation
            && left.child_roots_digest == right.child_roots_digest
            && Self::journal_operation(left) == Self::journal_operation(right)
            && left.canonical_workspace == right.canonical_workspace
            && left.workspace_digest == right.workspace_digest
    }

    fn transition_fork_journal_unlocked(
        &self,
        candidate: &ForkOperationJournal,
    ) -> Result<ForkOperationJournal, HostError> {
        let key = ForkOperationKey {
            operation_id: candidate.stable_operation_id.clone(),
            client_id: candidate.client_id.clone(),
            request_id: candidate.request_id.clone(),
            payload_hash: candidate.payload_hash.clone(),
        };
        let current = self
            .load_fork_journal_unlocked(&key)?
            .ok_or_else(|| HostError::Persistence("fork operation was not prepared".to_owned()))?;
        if !Self::same_fork_operation(&current, candidate) {
            return Err(HostError::RequestConflict);
        }
        let current_rank = current.state.rank();
        let candidate_rank = candidate.state.rank();
        if current_rank >= candidate_rank {
            return Ok(current);
        }
        if candidate_rank != current_rank.saturating_add(1) {
            return Err(HostError::Persistence(
                "fork journal transition skipped a durable phase".to_owned(),
            ));
        }
        let path = self
            .ensure_fork_journal_directory()?
            .join(format!("{}.json", candidate.operation_id));
        self.validate_fork_journal(candidate, &path)?;
        Self::persist_fork_journal(&path, candidate, true)?;
        Ok(candidate.clone())
    }

    #[cfg(test)]
    fn force_replace_fork_journal_for_test(
        &self,
        journal: &ForkOperationJournal,
    ) -> Result<(), HostError> {
        let _lock = self.acquire_fork_journal_lock()?;
        let path = self
            .ensure_fork_journal_directory()?
            .join(format!("{}.json", journal.operation_id));
        Self::persist_fork_journal(&path, journal, true)
    }

    #[cfg(test)]
    fn transition_fork_journal_for_test(
        &self,
        journal: &ForkOperationJournal,
    ) -> Result<ForkOperationJournal, HostError> {
        let _lock = self.acquire_fork_journal_lock()?;
        self.transition_fork_journal_unlocked(journal)
    }

    fn journal_operation(journal: &ForkOperationJournal) -> PreparedForkOperation {
        PreparedForkOperation {
            key: ForkOperationKey {
                operation_id: journal.stable_operation_id.clone(),
                client_id: journal.client_id.clone(),
                request_id: journal.request_id.clone(),
                payload_hash: journal.payload_hash.clone(),
            },
            request: ForkSessionRequest {
                operation_key: ForkOperationKey {
                    operation_id: journal.stable_operation_id.clone(),
                    client_id: journal.client_id.clone(),
                    request_id: journal.request_id.clone(),
                    payload_hash: journal.payload_hash.clone(),
                },
                parent: journal.parent.clone(),
                child_session_id: journal.child_session_id.clone(),
                at_turn: journal.at_turn.clone(),
                through_sequence: journal.through_sequence,
                include_idle_tail: journal.include_idle_tail,
                driver_client_id: journal.driver_client_id.clone(),
            },
        }
    }

    fn completed_fork_result(result: &ForkJournalResult) -> CompletedForkOperation {
        CompletedForkOperation {
            protocol_version: result.protocol_version,
            command_ack_emitted_at: result.command_ack_emitted_at.clone(),
            fork_event_emitted_at: result.fork_event_emitted_at.clone(),
            acknowledged_session_id: result.acknowledged_session_id.clone(),
            outcome: result.outcome.clone(),
            parent_session_id: result.parent_session_id.clone(),
            child: result.child.clone(),
            at_turn: result.at_turn.clone(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn recover_fork_operations(&self) -> Result<(), HostError> {
        let _lock = self.acquire_fork_journal_lock()?;
        let directory = self.ensure_fork_journal_directory()?;
        let mut completed = Vec::new();
        let mut pending = 0_usize;
        let mut seen = 0_usize;
        let mut directory_changed = false;
        let entries = fs::read_dir(&directory)
            .map_err(|_| HostError::Persistence("fork journal could not scan".to_owned()))?;
        for entry in entries {
            let entry =
                entry.map_err(|_| HostError::Persistence("fork journal scan failed".to_owned()))?;
            let path = entry.path();
            seen = seen.saturating_add(1);
            if seen
                > MAX_COMPLETED_FORK_OPERATIONS + MAX_PENDING_FORK_OPERATIONS + MAX_FORK_TEMP_FILES
            {
                return Err(HostError::Persistence(
                    "fork journal exceeds its bounded capacity".to_owned(),
                ));
            }
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                HostError::Persistence("fork journal has a non-Unicode entry".to_owned())
            })?;
            if name.starts_with(".fork-")
                && Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
            {
                let metadata = fs::symlink_metadata(&path).map_err(|_| {
                    HostError::Persistence("fork journal temporary file is unsafe".to_owned())
                })?;
                #[cfg(unix)]
                let private_single_link = {
                    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
                    metadata.nlink() == 1 && metadata.permissions().mode().trailing_zeros() >= 6
                };
                #[cfg(not(unix))]
                let private_single_link = true;
                if metadata.file_type().is_symlink() || !metadata.is_file() || !private_single_link
                {
                    return Err(HostError::Persistence(
                        "fork journal temporary file is unsafe".to_owned(),
                    ));
                }
                fs::remove_file(path).map_err(|_| {
                    HostError::Persistence("fork journal temporary cleanup failed".to_owned())
                })?;
                directory_changed = true;
                continue;
            }
            let stem = name.strip_suffix(".json").ok_or_else(|| {
                HostError::Persistence("fork journal has an unexpected entry".to_owned())
            })?;
            if !Self::is_lower_hex(stem) {
                return Err(HostError::Persistence(
                    "fork journal filename is invalid".to_owned(),
                ));
            }
            let metadata = fs::symlink_metadata(&path).map_err(|_| {
                HostError::Persistence("fork journal entry is unavailable".to_owned())
            })?;
            #[cfg(unix)]
            let single_link = {
                use std::os::unix::fs::MetadataExt as _;
                metadata.nlink() == 1
            };
            #[cfg(not(unix))]
            let single_link = true;
            if metadata.file_type().is_symlink() || !metadata.is_file() || !single_link {
                return Err(HostError::Persistence(
                    "fork journal entry is unsafe".to_owned(),
                ));
            }
            let bytes = self.read_fork_journal_file(&entry.file_name())?;
            let mut journal: ForkOperationJournal = serde_json::from_slice(&bytes)
                .map_err(|_| HostError::Persistence("fork journal is corrupt".to_owned()))?;
            self.validate_fork_journal(&journal, &path)?;
            match journal.state {
                ForkJournalState::Prepared => {
                    let metadata = self
                        .options
                        .storage_root
                        .join("sessions")
                        .join(&journal.child_session_id.0)
                        .join("metadata.json");
                    if metadata.is_file() {
                        crate::session_runtime::validate_forked_session_commit(
                            &self.options.storage_root,
                            &journal.canonical_workspace,
                            &journal.child_session_id.0,
                            &journal.operation_id,
                            &journal.parent.session_id.0,
                        )
                        .map_err(|_| {
                            HostError::Persistence(
                                "committed fork storage failed recovery validation".to_owned(),
                            )
                        })?;
                        journal.state = ForkJournalState::StorageCommitted;
                        journal.updated_unix_ms = unix_millis();
                        self.transition_fork_journal_unlocked(&journal)?;
                        pending = pending.saturating_add(1);
                    } else {
                        remove_forked_session_storage(
                            &self.options.storage_root,
                            &journal.canonical_workspace,
                            &journal.child_session_id.0,
                        )
                        .map_err(|_| {
                            HostError::Persistence("partial fork cleanup failed".to_owned())
                        })?;
                        // The durable operation remains authoritative even before
                        // child metadata exists, so retry reuses the same child id.
                        pending = pending.saturating_add(1);
                    }
                }
                ForkJournalState::StorageCommitted => {
                    crate::session_runtime::validate_forked_session_commit(
                        &self.options.storage_root,
                        &journal.canonical_workspace,
                        &journal.child_session_id.0,
                        &journal.operation_id,
                        &journal.parent.session_id.0,
                    )
                    .map_err(|_| {
                        HostError::Persistence(
                            "committed fork storage failed recovery validation".to_owned(),
                        )
                    })?;
                    pending = pending.saturating_add(1);
                }
                ForkJournalState::Completed { .. } => {
                    crate::session_runtime::validate_forked_session_commit(
                        &self.options.storage_root,
                        &journal.canonical_workspace,
                        &journal.child_session_id.0,
                        &journal.operation_id,
                        &journal.parent.session_id.0,
                    )
                    .map_err(|_| {
                        HostError::Persistence(
                            "completed fork storage failed recovery validation".to_owned(),
                        )
                    })?;
                    completed.push((journal.updated_unix_ms, journal.operation_id, path));
                }
            }
        }
        if pending > MAX_PENDING_FORK_OPERATIONS {
            return Err(HostError::Persistence(
                "too many unfinished fork operations require recovery".to_owned(),
            ));
        }
        completed.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
        let remove = completed
            .len()
            .saturating_sub(MAX_COMPLETED_FORK_OPERATIONS);
        for (_, _, path) in completed.into_iter().take(remove) {
            fs::remove_file(path)
                .map_err(|_| HostError::Persistence("fork journal retention failed".to_owned()))?;
            directory_changed = true;
        }
        if directory_changed {
            fs::File::open(&directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| {
                    HostError::Persistence("fork journal cleanup could not sync".to_owned())
                })?;
        }
        Ok(())
    }

    fn enforce_live_fork_limits_unlocked(&self, prune_completed: bool) -> Result<(), HostError> {
        let directory = self.ensure_fork_journal_directory()?;
        let mut pending = 0_usize;
        let mut completed = Vec::new();
        let mut seen = 0_usize;
        for entry in fs::read_dir(&directory)
            .map_err(|_| HostError::Persistence("fork journal could not scan".to_owned()))?
        {
            let entry =
                entry.map_err(|_| HostError::Persistence("fork journal scan failed".to_owned()))?;
            seen = seen.saturating_add(1);
            if seen
                > MAX_COMPLETED_FORK_OPERATIONS + MAX_PENDING_FORK_OPERATIONS + MAX_FORK_TEMP_FILES
            {
                return Err(HostError::Persistence(
                    "fork journal exceeds its bounded capacity".to_owned(),
                ));
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let journal: ForkOperationJournal =
                serde_json::from_slice(&self.read_fork_journal_file(&entry.file_name())?)
                    .map_err(|_| HostError::Persistence("fork journal is corrupt".to_owned()))?;
            self.validate_fork_journal(&journal, &path)?;
            match journal.state {
                ForkJournalState::Completed { .. } => {
                    completed.push((journal.updated_unix_ms, journal.operation_id, path));
                }
                ForkJournalState::Prepared | ForkJournalState::StorageCommitted => {
                    pending = pending.saturating_add(1);
                }
            }
        }
        if !prune_completed && pending >= MAX_PENDING_FORK_OPERATIONS {
            return Err(HostError::SessionCapacity);
        }
        if prune_completed && completed.len() > MAX_COMPLETED_FORK_OPERATIONS {
            completed.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
            let remove = completed.len() - MAX_COMPLETED_FORK_OPERATIONS;
            for (_, _, path) in completed.into_iter().take(remove) {
                fs::remove_file(path).map_err(|_| {
                    HostError::Persistence("fork journal retention failed".to_owned())
                })?;
            }
            fs::File::open(&directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| {
                    HostError::Persistence("fork journal retention could not sync".to_owned())
                })?;
        }
        Ok(())
    }

    fn authorize_workspace(&self, requested: &str) -> Result<PathBuf, HostError> {
        let requested = Path::new(requested);
        if !requested.is_absolute() {
            return Err(HostError::Protocol(
                "workspace must be an absolute path on the engine host".to_owned(),
            ));
        }
        let canonical = fs::canonicalize(requested)
            .map_err(|_| HostError::Protocol("workspace is unavailable".to_owned()))?;
        if !self
            .allowed_workspaces
            .iter()
            .any(|root| canonical == *root || canonical.starts_with(root))
        {
            return Err(HostError::Protocol(
                "workspace is outside the authorized roots".to_owned(),
            ));
        }
        Ok(canonical)
    }

    fn workspace_for_session(&self, descriptor: &SessionDescriptor) -> Result<PathBuf, HostError> {
        let metadata =
            load_session_metadata_any(&self.options.storage_root, &descriptor.session_id.0)
                .map_err(|_| {
                    HostError::Query("session workspace metadata is unavailable".to_owned())
                })?;
        let workspace = self.authorize_workspace_path(&metadata.workspace)?;
        if workspace_name(&workspace) != descriptor.workspace_name {
            return Err(HostError::Query(
                "session workspace descriptor does not match durable metadata".to_owned(),
            ));
        }
        Ok(workspace)
    }

    fn workspace_roots_for_session(
        &self,
        descriptor: &SessionDescriptor,
    ) -> Result<Vec<PathBuf>, HostError> {
        let primary = self.workspace_for_session(descriptor)?;
        let configured = load_session_workspace_roots(
            &self.journal_reads,
            &self.options.storage_root,
            &primary,
            &descriptor.session_id.0,
        )
        .map_err(|_| HostError::Query("session workspace roots are unavailable".to_owned()))?;
        let mut roots = Vec::with_capacity(configured.len());
        for (index, root) in configured.into_iter().enumerate() {
            let canonical = self.authorize_workspace_path(&root)?;
            if index == 0 && canonical != primary {
                return Err(HostError::Query(
                    "session primary workspace root changed".to_owned(),
                ));
            }
            roots.push(canonical);
        }
        Ok(roots)
    }

    fn authorize_workspace_path(&self, workspace: &Path) -> Result<PathBuf, HostError> {
        let canonical = fs::canonicalize(workspace)
            .map_err(|_| HostError::Query("session workspace is unavailable".to_owned()))?;
        if self
            .allowed_workspaces
            .iter()
            .any(|root| canonical == *root || canonical.starts_with(root))
        {
            Ok(canonical)
        } else {
            Err(HostError::Query(
                "session workspace is outside authorized roots".to_owned(),
            ))
        }
    }

    async fn compose(
        &self,
        session_id: SessionId,
        workspace: PathBuf,
        model: Option<ModelAlias>,
        resume: bool,
    ) -> Result<HostedSession, HostError> {
        let requested_model = self.requested_model_for_compose(&workspace, model, resume)?;
        let runtime = compose_hosted_actor(HostedSessionComposition {
            wasm_workers: Arc::clone(&self.wasm_workers),
            index_pool: Arc::clone(&self.index_pool),
            journal_reads: Arc::clone(&self.journal_reads),
            workspace: workspace.clone(),
            additional_workspaces: Vec::new(),
            allowed_workspace_roots: self
                .allowed_workspaces
                .iter()
                .filter(|root| **root != workspace)
                .cloned()
                .collect(),
            storage_root: self.options.storage_root.clone(),
            credentials_path: self.options.credentials_path.clone(),
            config: self.options.config.clone(),
            session_id: session_id.clone(),
            requested_model,
            resume,
            permission_mode: self.options.permission_mode,
            max_turns: self.options.max_turns,
            provider_mode: self.options.provider_mode.clone(),
            dangerously_trust: self.options.dangerously_trust,
            wait_for_execution_lease: self.options.wait_for_execution_lease,
        })
        .await
        .map_err(|error| {
            tracing::error!(session_id = %session_id.0, reason = %error, "hosted session composition failed");
            HostError::Persistence("session runtime could not be composed".to_owned())
        })?;
        let session = HostedSession::new(
            SessionDescriptor {
                session_id,
                title: "New session".to_owned(),
                workspace_name: workspace_name(&workspace),
                model: ModelAlias(runtime.model_alias),
                driver_client_id: runtime.driver_client_id,
                shell_active: runtime.shell_active,
            },
            runtime.handle,
        );
        let session = if let Some(model_catalog) = runtime.model_catalog {
            session.with_model_catalog(model_catalog)
        } else {
            session
        };
        let session = session
            .with_runtime_services(runtime.runtime_services)
            .with_subagents(runtime.subagents);
        Ok(if let Some(mcp) = runtime.mcp {
            session.with_mcp(mcp)
        } else {
            session
        })
    }

    fn persisted_descriptor(&self, session_id: &str) -> Result<SessionDescriptor, HostError> {
        let metadata = load_session_metadata_any(&self.options.storage_root, session_id)
            .map_err(|_| HostError::Persistence("session metadata is unavailable".to_owned()))?;
        let workspace = self.authorize_workspace_path(&metadata.workspace)?;
        Ok(SessionDescriptor {
            session_id: SessionId(session_id.to_owned()),
            title: SessionIndex::open(&self.options.storage_root)
                .ok()
                .and_then(|index| index.get(session_id).ok().flatten())
                .map(|summary| summary.title)
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| "New session".to_owned()),
            workspace_name: workspace_name(&workspace),
            model: ModelAlias(metadata.model_alias),
            // Persisted sessions are inactive until resumed. Live descriptors
            // from the host registry replace these entries after opening.
            driver_client_id: None,
            shell_active: false,
        })
    }

    fn persisted_sessions_blocking(&self) -> Result<Vec<SessionDescriptor>, HostError> {
        let sessions = self.options.storage_root.join("sessions");
        let entries = match fs::read_dir(sessions) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => {
                return Err(HostError::Persistence(
                    "session directory could not be listed".to_owned(),
                ));
            }
        };
        let mut descriptors = Vec::new();
        for entry in entries.take(MAX_SESSION_RESULTS).flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Some(session_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if let Ok(descriptor) = self.persisted_descriptor(&session_id) {
                descriptors.push(descriptor);
            }
        }
        descriptors.sort_by(|left, right| left.session_id.0.cmp(&right.session_id.0));
        Ok(descriptors)
    }

    fn search_sessions_blocking(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<(Vec<SessionDescriptor>, bool), SessionStoreError> {
        let requested =
            usize::try_from(limit).map_err(|_| SessionStoreError::SearchLimitTooLarge)?;
        let rows = SessionIndex::search_read_only(
            &self.options.storage_root,
            query,
            requested.saturating_add(1),
        )?;
        let truncated = rows.len() > requested;
        let descriptors = rows
            .into_iter()
            .take(requested)
            .filter_map(|row| self.persisted_descriptor(&row.id).ok())
            .collect();
        Ok((descriptors, truncated))
    }

    async fn search_sessions_with_retry(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<(Vec<SessionDescriptor>, bool), HostError> {
        for attempt in 1..=SESSION_INDEX_SEARCH_MAX_ATTEMPTS {
            let factory = self.clone();
            let query = query.to_owned();
            let result = tokio::task::spawn_blocking(move || {
                factory.search_sessions_blocking(&query, limit)
            })
            .await
            .map_err(|_| HostError::Query("session search worker failed".to_owned()))?;
            match result {
                Ok((rows, _)) if rows.is_empty() && attempt < SESSION_INDEX_SEARCH_MAX_ATTEMPTS => {
                    tokio::time::sleep(SESSION_INDEX_SEARCH_RETRY_DELAY).await;
                }
                Ok(result) => return Ok(result),
                Err(SessionStoreError::UnsafeSessionIndex)
                    if attempt < SESSION_INDEX_SEARCH_MAX_ATTEMPTS =>
                {
                    tokio::time::sleep(SESSION_INDEX_SEARCH_RETRY_DELAY).await;
                }
                Err(error) => {
                    tracing::warn!(
                        reason = %error,
                        attempt,
                        "hosted session index search failed"
                    );
                    return Err(HostError::Query("session index search failed".to_owned()));
                }
            }
        }
        Err(HostError::Query("session index search failed".to_owned()))
    }
}

#[async_trait]
impl SessionFactory for RuntimeSessionFactory {
    async fn shutdown(&self) -> Result<(), HostError> {
        self.wasm_workers.shutdown().await;
        Ok(())
    }

    fn allocate_session_id(&self) -> Result<SessionId, HostError> {
        new_session_id()
            .map(SessionId)
            .map_err(|_| HostError::Persistence("session id allocation failed".to_owned()))
    }

    async fn create(&self, request: CreateSessionRequest) -> Result<HostedSession, HostError> {
        let workspace = self.authorize_workspace(&request.workspace)?;
        self.compose(request.session_id, workspace, request.model, false)
            .await
    }

    async fn resume(&self, session_id: &SessionId) -> Result<HostedSession, HostError> {
        let metadata = load_session_metadata_any(&self.options.storage_root, &session_id.0)
            .map_err(|_| HostError::Persistence("session metadata is unavailable".to_owned()))?;
        let workspace = self.authorize_workspace_path(&metadata.workspace)?;
        self.compose(session_id.clone(), workspace, None, true)
            .await
    }

    async fn load_fork_operation(
        &self,
        key: &ForkOperationKey,
    ) -> Result<ForkOperationState, HostError> {
        let factory = self.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            let Some(journal) = factory.load_fork_journal_unlocked(&key)? else {
                return Ok(ForkOperationState::Missing);
            };
            match journal.state {
                ForkJournalState::Prepared | ForkJournalState::StorageCommitted => Ok(
                    ForkOperationState::Pending(Self::journal_operation(&journal)),
                ),
                ForkJournalState::Completed { result } => {
                    Ok(ForkOperationState::Completed(CompletedForkOperation {
                        protocol_version: result.protocol_version,
                        command_ack_emitted_at: result.command_ack_emitted_at,
                        fork_event_emitted_at: result.fork_event_emitted_at,
                        acknowledged_session_id: result.acknowledged_session_id,
                        outcome: result.outcome,
                        parent_session_id: result.parent_session_id,
                        child: result.child,
                        at_turn: result.at_turn,
                    }))
                }
            }
        })
        .await
        .map_err(|_| HostError::Persistence("fork journal worker failed".to_owned()))?
    }

    async fn prepare_fork_operation(
        &self,
        operation: PreparedForkOperation,
    ) -> Result<PreparedForkOperation, HostError> {
        let workspace = self.workspace_for_session(&operation.request.parent)?;
        let factory = self.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            if let Some(existing) = factory.load_fork_journal_unlocked(&operation.key)? {
                return Ok(Self::journal_operation(&existing));
            }
            factory.enforce_live_fork_limits_unlocked(false)?;
            if operation.request.operation_key != operation.key {
                return Err(HostError::Protocol(
                    "fork operation key does not match its request".to_owned(),
                ));
            }
            let operation_id = Self::fork_operation_id(&operation.key);
            let child = factory.expected_fork_state(&operation.request, &workspace)?;
            let journal = ForkOperationJournal {
                version: FORK_JOURNAL_VERSION,
                operation_id,
                stable_operation_id: operation.key.operation_id.clone(),
                client_id: operation.key.client_id.clone(),
                request_id: operation.key.request_id.clone(),
                payload_hash: operation.key.payload_hash.clone(),
                updated_unix_ms: unix_millis(),
                parent: operation.request.parent.clone(),
                child_model: child.model,
                child_workspace_generation: child.workspace_generation,
                child_roots_digest: child.roots_digest,
                child_session_id: operation.request.child_session_id.clone(),
                at_turn: operation.request.at_turn.clone(),
                through_sequence: operation.request.through_sequence,
                include_idle_tail: operation.request.include_idle_tail,
                driver_client_id: operation.request.driver_client_id.clone(),
                workspace_digest: blake3::hash(workspace.as_os_str().as_encoded_bytes())
                    .to_hex()
                    .to_string(),
                canonical_workspace: workspace,
                state: ForkJournalState::Prepared,
            };
            let path = factory.fork_journal_path(&operation.key)?;
            match Self::persist_new_fork_journal(&path, &journal) {
                Ok(()) => Ok(operation),
                Err(HostError::RequestConflict) => factory
                    .load_fork_journal_unlocked(&operation.key)?
                    .map(|existing| Self::journal_operation(&existing))
                    .ok_or_else(|| {
                        HostError::Persistence("fork journal creation raced".to_owned())
                    }),
                Err(error) => Err(error),
            }
        })
        .await
        .map_err(|_| HostError::Persistence("fork journal worker failed".to_owned()))?
    }

    async fn fork(&self, request: ForkSessionRequest) -> Result<HostedSession, HostError> {
        let workspace = self.workspace_for_session(&request.parent)?;
        let through_turn =
            request.at_turn.0.parse::<u64>().map_err(|_| {
                HostError::Protocol("fork turn must be an unsigned decimal".to_owned())
            })?;
        let storage_root = self.options.storage_root.clone();
        let workspace_for_fork = workspace.clone();
        let parent_session_id = request.parent.session_id.0.clone();
        let child_session = request.child_session_id.clone();
        let child_session_id = child_session.0.clone();
        let fork_child_session_id = child_session_id.clone();
        let through_sequence = request.through_sequence;
        let include_idle_tail = request.include_idle_tail;
        let driver_client_id = request.driver_client_id.clone();
        let operation_key = request.operation_key.clone();
        let factory = self.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            let mut journal = factory
                .load_fork_journal_unlocked(&operation_key)?
                .ok_or_else(|| {
                    HostError::Persistence("fork operation was not prepared".to_owned())
                })?;
            if Self::journal_operation(&journal).request != request {
                return Err(HostError::RequestConflict);
            }
            if matches!(journal.state, ForkJournalState::Prepared) {
                // Recompose at commit time so extension changes between
                // prepare and fork cannot bypass the persisted fingerprint.
                let expected = factory.expected_fork_state(&request, &workspace_for_fork)?;
                let operation_id = journal.operation_id.clone();
                fork_hosted_session_storage(
                    &factory.journal_reads,
                    &storage_root,
                    &workspace_for_fork,
                    &parent_session_id,
                    &fork_child_session_id,
                    through_turn,
                    through_sequence,
                    include_idle_tail,
                    driver_client_id,
                    Some(&operation_id),
                    &expected.modes,
                )
                .map_err(|error| {
                    tracing::error!(reason = %error, "session fork storage failed");
                    HostError::Persistence("session fork could not be persisted".to_owned())
                })?;
                journal.state = ForkJournalState::StorageCommitted;
                journal.updated_unix_ms = unix_millis();
                factory.transition_fork_journal_unlocked(&journal)?;
            }
            Ok(())
        })
        .await
        .map_err(|_| HostError::Persistence("fork storage worker failed".to_owned()))??;
        self.compose(child_session, workspace, None, true).await
    }

    async fn complete_fork_operation(
        &self,
        key: &ForkOperationKey,
        result: &CompletedForkOperation,
    ) -> Result<CompletedForkOperation, HostError> {
        let factory = self.clone();
        let key = key.clone();
        let result = result.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            let mut journal = factory.load_fork_journal_unlocked(&key)?.ok_or_else(|| {
                HostError::Persistence("fork operation was not prepared".to_owned())
            })?;
            if journal.child_session_id != result.child.session_id
                || journal.parent.session_id != result.parent_session_id
                || journal.at_turn != result.at_turn
            {
                return Err(HostError::SessionIdentityMismatch);
            }
            if let ForkJournalState::Completed { result: existing } = &journal.state {
                if existing.child != result.child || existing.outcome != result.outcome {
                    return Err(HostError::RequestConflict);
                }
                return Ok(Self::completed_fork_result(existing));
            }
            journal.state = ForkJournalState::Completed {
                result: Box::new(ForkJournalResult {
                    protocol_version: result.protocol_version,
                    command_ack_emitted_at: result.command_ack_emitted_at,
                    fork_event_emitted_at: result.fork_event_emitted_at,
                    acknowledged_session_id: result.acknowledged_session_id,
                    outcome: result.outcome,
                    parent_session_id: result.parent_session_id,
                    child: result.child,
                    at_turn: result.at_turn,
                }),
            };
            journal.updated_unix_ms = unix_millis();
            let committed = factory.transition_fork_journal_unlocked(&journal)?;
            factory.enforce_live_fork_limits_unlocked(true)?;
            let ForkJournalState::Completed { result } = committed.state else {
                return Err(HostError::Persistence(
                    "fork completion did not reach its durable phase".to_owned(),
                ));
            };
            Ok(Self::completed_fork_result(&result))
        })
        .await
        .map_err(|_| HostError::Persistence("fork journal worker failed".to_owned()))?
    }

    async fn abandon_prepared_fork_operation(
        &self,
        key: &ForkOperationKey,
    ) -> Result<(), HostError> {
        let factory = self.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = factory.acquire_fork_journal_lock()?;
            let Some(journal) = factory.load_fork_journal_unlocked(&key)? else {
                return Ok(());
            };
            if !matches!(journal.state, ForkJournalState::Prepared) {
                return Ok(());
            }
            remove_forked_session_storage(
                &factory.options.storage_root,
                &journal.canonical_workspace,
                &journal.child_session_id.0,
            )
            .map_err(|_| HostError::Persistence("partial fork cleanup failed".to_owned()))?;
            let path = factory
                .ensure_fork_journal_directory()?
                .join(format!("{}.json", journal.operation_id));
            fs::remove_file(path).map_err(|_| {
                HostError::Persistence("prepared fork journal cleanup failed".to_owned())
            })?;
            fs::File::open(factory.fork_journal_directory())
                .map_err(|_| {
                    HostError::Persistence("prepared fork directory is unavailable".to_owned())
                })?
                .sync_all()
                .map_err(|_| {
                    HostError::Persistence("prepared fork cleanup could not sync".to_owned())
                })
        })
        .await
        .map_err(|_| HostError::Persistence("fork journal worker failed".to_owned()))?
    }

    async fn persisted_sessions(&self) -> Result<Vec<SessionDescriptor>, HostError> {
        let factory = self.clone();
        tokio::time::timeout(
            SESSION_QUERY_DEADLINE,
            tokio::task::spawn_blocking(move || factory.persisted_sessions_blocking()),
        )
        .await
        .map_err(|_| HostError::Query("session listing deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("session listing worker failed".to_owned()))?
    }

    async fn search_persisted_sessions(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<(Vec<SessionDescriptor>, bool), HostError> {
        tokio::time::timeout(
            SESSION_QUERY_DEADLINE,
            self.search_sessions_with_retry(query, limit),
        )
        .await
        .map_err(|_| HostError::Query("session search deadline exceeded".to_owned()))?
    }
}

#[async_trait]
impl HostQueryService for RuntimeSessionFactory {
    async fn command_descriptors(&self) -> Result<Vec<CommandDescriptor>, HostError> {
        let registry = builtin_command_registry().map_err(HostError::from)?;
        Ok(registry
            .descriptors()
            .map(|descriptor| CommandDescriptor {
                name: descriptor.name().to_owned(),
                description: descriptor.description().to_owned(),
                usage: descriptor.argument_hint().unwrap_or_default().to_owned(),
                source: rw_core::CommandSource::default(),
            })
            .collect())
    }

    async fn model_catalog(
        &self,
        refresh: bool,
        selected_model: Option<&str>,
        resolved_model: Option<&str>,
    ) -> Result<ModelCatalogSnapshot, HostError> {
        let mut catalog = self
            .model_catalog
            .get(refresh)
            .await
            .map_err(|error| HostError::Query(error.to_string()))?;
        overlay_catalog_current(&mut catalog, selected_model, resolved_model);
        Ok(catalog)
    }

    async fn user_settings(
        &self,
        session: &SessionDescriptor,
    ) -> Result<Vec<UserSettingDescriptor>, HostError> {
        let workspace = self.workspace_for_session(session)?;
        let config_loader = self.settings_loader_for(&workspace);
        let project_loader = config_loader.clone();
        let effective = tokio::task::spawn_blocking(move || config_loader.load())
            .await
            .map_err(|_| HostError::Query("user settings worker failed".to_owned()))?
            .map_err(|error| HostError::Query(error.to_string()))?;
        let project_model = project_loader
            .tui_project_model()
            .map_err(|error| HostError::Query(error.to_string()))?;
        let keybinding_preset = project_loader
            .tui_keybinding_preset()
            .map_err(|error| HostError::Query(error.to_string()))?;
        let mcp_servers = project_loader
            .tui_mcp_servers()
            .map_err(|error| HostError::Query(error.to_string()))?;
        Ok(Self::setting_descriptors(
            &effective,
            session,
            project_model.as_deref(),
            &keybinding_preset,
            &mcp_servers,
        ))
    }

    async fn set_user_setting(
        &self,
        session: &SessionDescriptor,
        key: &str,
        value: &str,
    ) -> Result<Vec<UserSettingDescriptor>, HostError> {
        let workspace = self.workspace_for_session(session)?;
        let config_loader = self.settings_loader_for(&workspace);
        let project_loader = config_loader.clone();
        let setting_key = EditableSettingKey::parse(key).ok_or_else(|| {
            HostError::Persistence(
                rw_store::config::ConfigError::InvalidUserSetting {
                    key: key.to_owned(),
                    reason: "key or value is outside the safe TUI settings allowlist".to_owned(),
                }
                .to_string(),
            )
        })?;
        let rendered_key = setting_key.render();
        let value = value.to_owned();
        let project_model_write = matches!(&setting_key, EditableSettingKey::ProjectDefaultModel);
        let persisted_project_model = project_model_write.then(|| value.clone());
        let effective = tokio::task::spawn_blocking(move || match setting_key {
            EditableSettingKey::ProjectDefaultModel => {
                config_loader.persist_tui_project_model(&value)
            }
            EditableSettingKey::KeybindingPreset => {
                config_loader.persist_tui_keybinding_preset(&value)?;
                config_loader.load()
            }
            EditableSettingKey::McpServerEnabled(server) => {
                let enabled = match value.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(rw_store::config::ConfigError::InvalidUserSetting {
                            key: rendered_key,
                            reason: "MCP enablement must be true or false".to_owned(),
                        });
                    }
                };
                config_loader.persist_tui_mcp_enabled(&server, enabled)?;
                config_loader.load()
            }
            EditableSettingKey::McpAddHttp(server) => {
                config_loader.persist_tui_mcp_http_server(&server, &value)?;
                config_loader.load()
            }
            _ => config_loader.persist_tui_setting(&rendered_key, &value),
        })
        .await
        .map_err(|_| HostError::Persistence("user setting worker failed".to_owned()))?
        .map_err(|error| HostError::Persistence(error.to_string()))?;
        let project_model = if let Some(model) = persisted_project_model {
            Some(model)
        } else {
            project_loader
                .tui_project_model()
                .map_err(|error| HostError::Query(error.to_string()))?
        };
        let keybinding_preset = project_loader
            .tui_keybinding_preset()
            .map_err(|error| HostError::Query(error.to_string()))?;
        let mcp_servers = project_loader
            .tui_mcp_servers()
            .map_err(|error| HostError::Query(error.to_string()))?;
        Ok(Self::setting_descriptors(
            &effective,
            session,
            project_model.as_deref(),
            &keybinding_preset,
            &mcp_servers,
        ))
    }

    async fn persist_project_model_selection(
        &self,
        session: &SessionDescriptor,
        model: &ModelAlias,
    ) -> Result<(), HostError> {
        let workspace = self.workspace_for_session(session)?;
        let loader = self.settings_loader_for(&workspace);
        let model = model.0.clone();
        tokio::task::spawn_blocking(move || loader.persist_tui_project_model(&model))
            .await
            .map_err(|_| HostError::Persistence("project model worker failed".to_owned()))?
            .map_err(|error| HostError::Persistence(error.to_string()))?;
        Ok(())
    }

    async fn begin_provider_auth(&self, provider: &str) -> Result<ProviderAuthAttempt, HostError> {
        match begin_provider_login(provider)
            .await
            .map_err(|error| HostError::Query(error.to_string()))?
        {
            ProviderLogin::OAuth(login) => {
                let challenge = ProviderAuthChallenge::Oauth {
                    authorization_url: login.authorization_url().to_owned(),
                    redirect_uri: login.redirect_uri().to_owned(),
                };
                let warnings = login.warnings().to_vec();
                let provider = provider.to_owned();
                let completion = Box::pin(async move {
                    let prepared = login
                        .prepare()
                        .await
                        .map_err(|error| HostError::Query(error.to_string()))?;
                    Ok(ProviderAuthCompletion::new(
                        provider,
                        "provider authentication completed".to_owned(),
                        Vec::new(),
                    )
                    .with_persistence(move || {
                        prepared
                            .persist()
                            .map(|result| result.warnings)
                            .map_err(|_| {
                                HostError::Persistence(
                                    "provider credential storage failed".to_owned(),
                                )
                            })
                    }))
                });
                Ok(ProviderAuthAttempt::new(
                    challenge,
                    warnings,
                    completion,
                    Arc::new(|| {}),
                ))
            }
            ProviderLogin::GitHubCopilot(login) => {
                let challenge = ProviderAuthChallenge::DeviceFlow {
                    verification_uri: login.verification_uri().to_owned(),
                    user_code: login.user_code().to_owned(),
                };
                let warnings = login.warnings().to_vec();
                let cancellation = ProviderLoginCancellation::default();
                let poll_cancellation = cancellation.clone();
                let provider = provider.to_owned();
                let completion = Box::pin(async move {
                    let prepared = login
                        .prepare(&poll_cancellation)
                        .await
                        .map_err(|error| HostError::Query(error.to_string()))?;
                    Ok(ProviderAuthCompletion::new(
                        provider,
                        "provider authentication completed".to_owned(),
                        Vec::new(),
                    )
                    .with_persistence(move || {
                        prepared
                            .persist()
                            .map(|result| result.warnings)
                            .map_err(|_| {
                                HostError::Persistence(
                                    "provider credential storage failed".to_owned(),
                                )
                            })
                    }))
                });
                Ok(ProviderAuthAttempt::new(
                    challenge,
                    warnings,
                    completion,
                    Arc::new(move || cancellation.cancel()),
                ))
            }
        }
    }

    async fn configure_builtin_provider(
        &self,
        profile: rw_core::BuiltinProviderProfile,
    ) -> Result<(), HostError> {
        let config_loader = self.settings_loader();
        tokio::task::spawn_blocking(move || {
            config_loader.configure_provider_profile(profile.canonical_id(), profile.config_kind())
        })
        .await
        .map_err(|_| HostError::Persistence("built-in provider setup worker failed".to_owned()))?
        .map_err(|error| HostError::Persistence(error.to_string()))?;
        Ok(())
    }

    async fn search_workspace_files(
        &self,
        session: &SessionDescriptor,
        query: &str,
        limit: u32,
    ) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
        let workspaces = self.workspace_roots_for_session(session)?;
        let query = query.to_owned();
        let limit = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .clamp(1, MAX_SEARCH_RESULTS);
        tokio::time::timeout(
            QUERY_DEADLINE,
            tokio::task::spawn_blocking(move || search_workspaces(&workspaces, &query, limit)),
        )
        .await
        .map_err(|_| HostError::Query("workspace search deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("workspace search worker failed".to_owned()))?
    }

    async fn preview_workspace_file(
        &self,
        session: &SessionDescriptor,
        path: &str,
        max_bytes: u32,
    ) -> Result<WorkspaceFilePreview, HostError> {
        let workspaces = self.workspace_roots_for_session(session)?;
        let (root_index, relative) = split_virtual_path(path)?;
        let workspace = workspaces
            .get(root_index)
            .cloned()
            .ok_or_else(|| HostError::Query("workspace root index is not authorized".to_owned()))?;
        let rendered_path = path.to_owned();
        let maximum = usize::try_from(max_bytes)
            .unwrap_or(usize::MAX)
            .min(MAX_PREVIEW_BYTES);
        if maximum == 0 {
            return Err(HostError::Query(
                "preview byte limit must not be zero".to_owned(),
            ));
        }
        tokio::time::timeout(
            QUERY_DEADLINE,
            tokio::task::spawn_blocking(move || {
                let mut preview = preview_file(&workspace, &relative, maximum)?;
                preview.path = rendered_path;
                Ok(preview)
            }),
        )
        .await
        .map_err(|_| HostError::Query("workspace preview deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("workspace preview worker failed".to_owned()))?
    }

    async fn workspace_status(
        &self,
        session: &SessionDescriptor,
    ) -> Result<WorkspaceStatus, HostError> {
        let workspace = self.workspace_for_session(session)?;
        let name = session.workspace_name.clone();
        tokio::time::timeout(
            WORKSPACE_STATUS_DEADLINE,
            tokio::task::spawn_blocking(move || read_workspace_status(&workspace, name)),
        )
        .await
        .map_err(|_| HostError::Query("workspace status deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("workspace status worker failed".to_owned()))?
    }

    async fn workspace_diff(
        &self,
        session: &SessionDescriptor,
        path: &str,
        max_bytes: u32,
    ) -> Result<WorkspaceDiff, HostError> {
        let workspaces = self.workspace_roots_for_session(session)?;
        let (root_index, relative) = split_virtual_path(path)?;
        let workspace = workspaces
            .get(root_index)
            .cloned()
            .ok_or_else(|| HostError::Query("workspace root index is not authorized".to_owned()))?;
        let rendered_path = path.to_owned();
        let maximum = usize::try_from(max_bytes)
            .unwrap_or(usize::MAX)
            .min(MAX_PREVIEW_BYTES);
        if maximum == 0 {
            return Err(HostError::Query(
                "workspace diff byte limit must not be zero".to_owned(),
            ));
        }
        tokio::time::timeout(
            WORKSPACE_DIFF_DEADLINE,
            tokio::task::spawn_blocking(move || {
                let mut diff = read_workspace_diff(&workspace, &relative, maximum)?;
                diff.path = rendered_path;
                Ok(diff)
            }),
        )
        .await
        .map_err(|_| HostError::Query("workspace diff deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("workspace diff worker failed".to_owned()))?
    }

    async fn export_session(
        &self,
        session: &SessionDescriptor,
        format: TranscriptFormat,
        output_path: &str,
        force: bool,
    ) -> Result<String, HostError> {
        let output_path = PathBuf::from(output_path);
        if !output_path.is_absolute() {
            return Err(HostError::Query(
                "export output path must be absolute".to_owned(),
            ));
        }
        let factory = self.clone();
        let session = session.clone();
        tokio::time::timeout(
            SESSION_EXPORT_DEADLINE,
            tokio::task::spawn_blocking(move || {
                factory.export_session_blocking(&session, format, &output_path, force)
            }),
        )
        .await
        .map_err(|_| HostError::Query("session export deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("session export worker failed".to_owned()))?
    }
}

fn overlay_catalog_current(
    catalog: &mut ModelCatalogSnapshot,
    selected_model: Option<&str>,
    resolved_model: Option<&str>,
) {
    let current = selected_model
        .filter(|selected| selected.contains('/'))
        .or(resolved_model)
        .or(selected_model);
    if let Some(current) = current {
        for model in &mut catalog.models {
            model.current = model.id == current
                || catalog.aliases.iter().any(|alias| {
                    alias.alias.0 == current && alias.candidates.first() == Some(&model.id)
                });
        }
    }
    if let Some(selected) = selected_model {
        for alias in &mut catalog.aliases {
            alias.current = alias.alias.0 == selected;
        }
    }
}

/// Rounds down to whole cents for display; TUI-authored values are exact multiples of 10,000 micros and round-trip exactly.
fn format_cost_cap(micros: Option<u64>) -> String {
    let Some(micros) = micros else {
        return "Unlimited".to_owned();
    };
    let cents = micros / 10_000;
    format!("${}.{:02}", cents / 100, cents % 100)
}

fn format_token_limit(tokens: Option<u64>) -> String {
    tokens.map_or_else(|| "Unlimited".to_owned(), |tokens| tokens.to_string())
}

fn budget_setting_descriptors(
    loaded: &rw_store::config::LoadedConfig,
) -> [UserSettingDescriptor; 6] {
    let session_cost_key = EditableSettingKey::SessionCostCap.render();
    let daily_cost_key = EditableSettingKey::DailyCostCap.render();
    let warning_key = EditableSettingKey::BudgetWarning.render();
    let session_token_key = EditableSettingKey::SessionTokenCap.render();
    let daily_token_key = EditableSettingKey::DailyTokenCap.render();
    let token_rate_key = EditableSettingKey::TokenRateAlarm.render();
    let provenance = |key: &str| {
        loaded
            .provenance(key)
            .map_or_else(|| "built-in".to_owned(), ToString::to_string)
    };
    [
        UserSettingDescriptor {
            key: session_cost_key.clone(),
            label: "Session cost cap".to_owned(),
            value: format_cost_cap(loaded.config.budget.session_cost_cap_micros_usd),
            choices: Vec::new(),
            provenance: provenance(&session_cost_key),
            applies_immediately: false,
        },
        UserSettingDescriptor {
            key: daily_cost_key.clone(),
            label: "Daily cost cap".to_owned(),
            value: format_cost_cap(loaded.config.budget.daily_cost_cap_micros_usd),
            choices: Vec::new(),
            provenance: provenance(&daily_cost_key),
            applies_immediately: false,
        },
        UserSettingDescriptor {
            key: session_token_key.clone(),
            label: "Session token cap".to_owned(),
            value: format_token_limit(loaded.config.budget.session_token_cap),
            choices: Vec::new(),
            provenance: provenance(&session_token_key),
            applies_immediately: false,
        },
        UserSettingDescriptor {
            key: daily_token_key.clone(),
            label: "Daily token cap".to_owned(),
            value: format_token_limit(loaded.config.budget.daily_token_cap),
            choices: Vec::new(),
            provenance: provenance(&daily_token_key),
            applies_immediately: false,
        },
        UserSettingDescriptor {
            key: token_rate_key.clone(),
            label: "Token rate alarm".to_owned(),
            value: format_token_limit(loaded.config.budget.token_rate_alarm_per_minute),
            choices: Vec::new(),
            provenance: provenance(&token_rate_key),
            applies_immediately: false,
        },
        UserSettingDescriptor {
            key: warning_key.clone(),
            label: "Budget warning".to_owned(),
            value: format!("{}%", loaded.config.budget.warn_at_percent),
            choices: Vec::new(),
            provenance: provenance(&warning_key),
            applies_immediately: false,
        },
    ]
}

fn workspace_name(workspace: &Path) -> String {
    workspace
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace")
        .to_owned()
}

#[cfg(test)]
fn configured_alias_providers(candidates: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    candidates
        .iter()
        .filter_map(|candidate| candidate.split_once('/').map(|(provider, _)| provider))
        .filter(|provider| {
            !provider.is_empty()
                && provider.len() <= MAX_PROVIDER_DISPLAY_NAME_BYTES
                && !provider.chars().any(char::is_control)
        })
        .filter(|provider| seen.insert((*provider).to_owned()))
        .map(str::to_owned)
        .collect()
}

fn safe_relative_path(value: &str) -> Result<PathBuf, HostError> {
    let path = Path::new(value);
    if value.is_empty() || path.is_absolute() {
        return Err(HostError::Query(
            "workspace path must be a non-empty normalized relative path".to_owned(),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(HostError::Query(
                "workspace path must be a non-empty normalized relative path".to_owned(),
            ));
        };
        normalized.push(name);
    }
    if normalized.as_os_str().is_empty() {
        return Err(HostError::Query(
            "workspace path must be a non-empty normalized relative path".to_owned(),
        ));
    }
    Ok(normalized)
}

fn split_virtual_path(value: &str) -> Result<(usize, PathBuf), HostError> {
    let normalized = safe_relative_path(value)?;
    let mut components = normalized.components();
    let Some(Component::Normal(first)) = components.next() else {
        return Err(HostError::Query("workspace path is invalid".to_owned()));
    };
    if first != "@root" {
        return Ok((0, normalized));
    }
    let Some(Component::Normal(index)) = components.next() else {
        return Err(HostError::Query(
            "virtual workspace path must use @root/<index>/...".to_owned(),
        ));
    };
    let index = index
        .to_str()
        .and_then(|index| index.parse::<usize>().ok())
        .filter(|index| *index > 0)
        .ok_or_else(|| HostError::Query("workspace root index must be positive".to_owned()))?;
    let relative = components.fold(PathBuf::new(), |path, component| {
        path.join(component.as_os_str())
    });
    if relative.as_os_str().is_empty() {
        return Err(HostError::Query(
            "virtual workspace path must name a file".to_owned(),
        ));
    }
    Ok((index, relative))
}

#[cfg(unix)]
fn search_workspaces(
    workspaces: &[PathBuf],
    query: &str,
    limit: usize,
) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
    let mut combined = Vec::new();
    let mut truncated = false;
    for (index, workspace) in workspaces.iter().enumerate() {
        let remaining = limit.saturating_sub(combined.len());
        if remaining == 0 {
            truncated = true;
            break;
        }
        let (mut matches, root_truncated) = search_workspace(workspace, query, remaining)?;
        if index > 0 {
            for item in &mut matches {
                item.path = format!("@root/{index}/{}", item.path);
            }
        }
        combined.extend(matches);
        truncated |= root_truncated;
        if truncated || combined.len() >= limit {
            break;
        }
    }
    combined.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((combined, truncated))
}

#[cfg(not(unix))]
fn search_workspaces(
    _workspaces: &[PathBuf],
    _query: &str,
    _limit: usize,
) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
    Err(HostError::Query(
        "safe workspace search is unavailable on this platform".to_owned(),
    ))
}

#[cfg(unix)]
fn preview_file(
    workspace: &Path,
    relative: &Path,
    maximum: usize,
) -> Result<WorkspaceFilePreview, HostError> {
    let root = open_workspace_directory(workspace)?;
    let file = open_relative_regular_file(&root, relative)?;
    let stat = rustix::fs::fstat(&file)
        .map_err(|_| HostError::Query("workspace file metadata is unavailable".to_owned()))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(HostError::Query(
            "workspace preview accepts regular files only".to_owned(),
        ));
    }
    let total_bytes = usize::try_from(stat.st_size).unwrap_or(usize::MAX);
    if total_bytes > maximum {
        return Err(HostError::Query(
            "workspace file exceeds the preview byte limit".to_owned(),
        ));
    }
    let mut bytes = Vec::with_capacity(total_bytes.min(maximum));
    fs::File::from(file)
        .take(u64::try_from(maximum).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| HostError::Query("workspace file could not be read".to_owned()))?;
    if bytes.len() > maximum {
        return Err(HostError::Query(
            "workspace file exceeded the preview byte limit while reading".to_owned(),
        ));
    }
    if let Some(media_type) = workspace_image_media_type(&bytes) {
        return Ok(WorkspaceFilePreview {
            path: relative.to_string_lossy().into_owned(),
            media_type: media_type.to_owned(),
            data: AttachmentData::InlineBase64 {
                data: BASE64_STANDARD.encode(&bytes),
            },
            total_bytes: u64::try_from(total_bytes).unwrap_or(u64::MAX),
            truncated: false,
        });
    }
    if total_bytes > MAX_TEXT_PREVIEW_BYTES {
        return Err(HostError::Query(
            "text attachment exceeds the 1 MiB message limit".to_owned(),
        ));
    }
    if bytes.contains(&0) {
        return Err(HostError::Query(
            "this binary file type cannot be attached".to_owned(),
        ));
    }
    let content = String::from_utf8(bytes)
        .map_err(|_| HostError::Query("binary workspace files are not previewed".to_owned()))?;
    Ok(WorkspaceFilePreview {
        path: relative.to_string_lossy().into_owned(),
        media_type: "text/plain".to_owned(),
        data: AttachmentData::Text { content },
        total_bytes: u64::try_from(total_bytes).unwrap_or(u64::MAX),
        truncated: false,
    })
}

#[cfg(unix)]
fn workspace_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(not(unix))]
fn preview_file(
    _workspace: &Path,
    _relative: &Path,
    _maximum: usize,
) -> Result<WorkspaceFilePreview, HostError> {
    Err(HostError::Query(
        "safe workspace preview is unavailable on this platform".to_owned(),
    ))
}

#[cfg(unix)]
#[derive(Clone, Default)]
struct IgnoreRules(Option<Arc<IgnoreRuleNode>>);

#[cfg(unix)]
struct IgnoreRuleNode {
    matcher: Gitignore,
    parent: Option<Arc<IgnoreRuleNode>>,
}

#[cfg(unix)]
impl IgnoreRules {
    fn with_matcher(&self, matcher: Gitignore) -> Self {
        if matcher.is_empty() {
            return self.clone();
        }
        Self(Some(Arc::new(IgnoreRuleNode {
            matcher,
            parent: self.0.clone(),
        })))
    }

    fn is_ignored(&self, relative: &Path, is_directory: bool) -> bool {
        let mut current = self.0.as_deref();
        while let Some(node) = current {
            let matched = node.matcher.matched(relative, is_directory);
            if matched.is_ignore() {
                return true;
            }
            if matched.is_whitelist() {
                return false;
            }
            current = node.parent.as_deref();
        }
        false
    }
}

#[cfg(unix)]
#[derive(Clone, Default)]
struct WorkspaceIgnoreRules {
    git: IgnoreRules,
    tool: IgnoreRules,
}

#[cfg(unix)]
impl WorkspaceIgnoreRules {
    fn with_directory(
        &self,
        directory: &OwnedFd,
        relative_directory: &Path,
        root: bool,
        workspace: &Path,
    ) -> Result<Self, ()> {
        let matcher_root = if relative_directory.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative_directory
        };
        let mut git_builder = GitignoreBuilder::new(matcher_root);
        let mut git_patterns = 0_usize;
        if root {
            add_git_info_exclude(&mut git_builder, directory, workspace, &mut git_patterns)?;
        }
        add_ignore_file(
            &mut git_builder,
            directory,
            Path::new(".gitignore"),
            &mut git_patterns,
        )?;
        let git_matcher = git_builder.build().map_err(|_| ())?;

        let mut tool_builder = GitignoreBuilder::new(matcher_root);
        let mut tool_patterns = 0_usize;
        add_ignore_file(
            &mut tool_builder,
            directory,
            Path::new(".ignore"),
            &mut tool_patterns,
        )?;
        let tool_matcher = tool_builder.build().map_err(|_| ())?;
        Ok(Self {
            git: self.git.with_matcher(git_matcher),
            tool: self.tool.with_matcher(tool_matcher),
        })
    }

    fn is_ignored(&self, relative: &Path, is_directory: bool) -> bool {
        // Tool-specific whitelists must never revive paths excluded by Git.
        self.git.is_ignored(relative, is_directory) || self.tool.is_ignored(relative, is_directory)
    }
}

#[cfg(unix)]
enum IgnoreFile {
    Missing,
    Content(String),
    Unsafe,
}

#[cfg(unix)]
enum GitInfoExclude {
    Missing,
    Strict(String),
    External(String),
}

#[cfg(unix)]
fn read_bounded_ignore_file(directory: &OwnedFd, relative: &Path) -> IgnoreFile {
    let components = relative.components().collect::<Vec<_>>();
    let Ok(mut parent) = rustix::fs::openat(
        directory,
        ".",
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) else {
        return IgnoreFile::Unsafe;
    };
    let mut file = None;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return IgnoreFile::Unsafe;
        };
        let final_component = index.saturating_add(1) == components.len();
        let mut flags = rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC;
        if !final_component {
            flags |= rustix::fs::OFlags::DIRECTORY;
        }
        match rustix::fs::openat(&parent, *name, flags, rustix::fs::Mode::empty()) {
            Ok(opened) if final_component => file = Some(opened),
            Ok(opened) => parent = opened,
            Err(error) if error == rustix::io::Errno::NOENT => return IgnoreFile::Missing,
            Err(_) => return IgnoreFile::Unsafe,
        }
    }
    let Some(file) = file else {
        return IgnoreFile::Unsafe;
    };
    let Ok(stat) = rustix::fs::fstat(&file) else {
        return IgnoreFile::Unsafe;
    };
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_size < 0
        || usize::try_from(stat.st_size).map_or(true, |size| size > MAX_IGNORE_FILE_BYTES)
    {
        return IgnoreFile::Unsafe;
    }
    let mut bytes = Vec::new();
    let Ok(maximum) = u64::try_from(MAX_IGNORE_FILE_BYTES) else {
        return IgnoreFile::Unsafe;
    };
    if fs::File::from(file)
        .take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
    {
        return IgnoreFile::Unsafe;
    }
    if bytes.len() > MAX_IGNORE_FILE_BYTES {
        return IgnoreFile::Unsafe;
    }
    String::from_utf8(bytes).map_or(IgnoreFile::Unsafe, IgnoreFile::Content)
}

#[cfg(unix)]
fn bounded_gitdir_path(content: &str, prefix: Option<&str>) -> Option<PathBuf> {
    if content.len() > MAX_GITDIR_POINTER_BYTES {
        return None;
    }
    let content = content.strip_suffix('\n').unwrap_or(content);
    let content = content.strip_suffix('\r').unwrap_or(content);
    let value = prefix.map_or(Some(content), |prefix| content.strip_prefix(prefix))?;
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    Some(PathBuf::from(value))
}

#[cfg(unix)]
fn open_linked_git_directory(base: &Path, path: &Path) -> Option<(PathBuf, OwnedFd)> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    let canonical = fs::canonicalize(path).ok()?;
    let directory = open_workspace_directory(&canonical).ok()?;
    Some((canonical, directory))
}

#[cfg(unix)]
fn linked_git_info_exclude(workspace: &Path, git_pointer: &str) -> Option<String> {
    let gitdir = bounded_gitdir_path(git_pointer, Some("gitdir: "))?;
    let (gitdir_path, gitdir) = open_linked_git_directory(workspace, &gitdir)?;
    let common = match read_bounded_ignore_file(&gitdir, Path::new("commondir")) {
        IgnoreFile::Missing => gitdir,
        IgnoreFile::Content(content) => {
            let common = bounded_gitdir_path(&content, None)?;
            open_linked_git_directory(&gitdir_path, &common)?.1
        }
        IgnoreFile::Unsafe => return None,
    };
    match read_bounded_ignore_file(&common, Path::new("info/exclude")) {
        IgnoreFile::Content(content) => Some(content),
        IgnoreFile::Missing | IgnoreFile::Unsafe => None,
    }
}

#[cfg(unix)]
fn read_git_info_exclude(directory: &OwnedFd, workspace: &Path) -> Result<GitInfoExclude, ()> {
    let metadata =
        match rustix::fs::statat(directory, ".git", rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(metadata) => metadata,
            Err(error) if error == rustix::io::Errno::NOENT => return Ok(GitInfoExclude::Missing),
            Err(_) => return Err(()),
        };
    let kind = rustix::fs::FileType::from_raw_mode(metadata.st_mode);
    if kind.is_dir() {
        return match read_bounded_ignore_file(directory, Path::new(".git/info/exclude")) {
            IgnoreFile::Missing => Ok(GitInfoExclude::Missing),
            IgnoreFile::Content(content) => Ok(GitInfoExclude::Strict(content)),
            IgnoreFile::Unsafe => Err(()),
        };
    }
    if !kind.is_file() {
        return Ok(GitInfoExclude::Missing);
    }
    let IgnoreFile::Content(pointer) = read_bounded_ignore_file(directory, Path::new(".git"))
    else {
        // An unavailable external gitdir must not make ordinary workspace
        // files disappear from the picker.
        return Ok(GitInfoExclude::Missing);
    };
    Ok(linked_git_info_exclude(workspace, &pointer)
        .map_or(GitInfoExclude::Missing, GitInfoExclude::External))
}

#[cfg(unix)]
fn valid_ignore_patterns(content: &str) -> bool {
    let mut builder = GitignoreBuilder::new(".");
    let mut patterns = 0;
    add_ignore_patterns(&mut builder, content, &mut patterns).is_ok() && builder.build().is_ok()
}

#[cfg(unix)]
fn add_git_info_exclude(
    builder: &mut GitignoreBuilder,
    directory: &OwnedFd,
    workspace: &Path,
    patterns: &mut usize,
) -> Result<(), ()> {
    match read_git_info_exclude(directory, workspace)? {
        GitInfoExclude::Strict(content) => add_ignore_patterns(builder, &content, patterns),
        GitInfoExclude::External(content) if valid_ignore_patterns(&content) => {
            add_ignore_patterns(builder, &content, patterns)
        }
        GitInfoExclude::Missing | GitInfoExclude::External(_) => Ok(()),
    }
}

#[cfg(unix)]
fn add_ignore_file(
    builder: &mut GitignoreBuilder,
    directory: &OwnedFd,
    relative: &Path,
    patterns: &mut usize,
) -> Result<(), ()> {
    match read_bounded_ignore_file(directory, relative) {
        IgnoreFile::Missing => Ok(()),
        IgnoreFile::Content(content) => add_ignore_patterns(builder, &content, patterns),
        IgnoreFile::Unsafe => Err(()),
    }
}

#[cfg(unix)]
fn add_ignore_patterns(
    builder: &mut GitignoreBuilder,
    content: &str,
    patterns: &mut usize,
) -> Result<(), ()> {
    for (index, line) in content.lines().enumerate() {
        if *patterns >= MAX_IGNORE_PATTERNS_PER_DIRECTORY {
            return Err(());
        }
        let line = if index == 0 {
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        if line.len() > MAX_IGNORE_PATTERN_BYTES {
            return Err(());
        }
        builder.add_line(None, line).map_err(|_| ())?;
        *patterns = patterns.saturating_add(1);
    }
    Ok(())
}

#[cfg(unix)]
fn fuzzy_path_matches(path: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let mut path = path.chars();
    query.chars().all(|needle| {
        path.by_ref()
            .any(|candidate| candidate.eq_ignore_ascii_case(&needle))
    })
}

#[cfg(unix)]
fn search_workspace(
    workspace: &Path,
    query: &str,
    limit: usize,
) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
    if query.len() > MAX_SEARCH_QUERY_BYTES {
        return Ok((Vec::new(), true));
    }
    let started = Instant::now();
    let root = open_workspace_directory(workspace)?;
    let mut pending = vec![(root, PathBuf::new(), WorkspaceIgnoreRules::default(), true)];
    let mut matches: BTreeMap<String, bool> = BTreeMap::new();
    let mut visited = 0_usize;
    let mut truncated = false;
    while let Some((directory, relative_directory, parent_rules, is_root)) = pending.pop() {
        if started.elapsed() >= QUERY_DEADLINE || visited >= MAX_SEARCH_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(rules) =
            parent_rules.with_directory(&directory, &relative_directory, is_root, workspace)
        else {
            // An unsafe ignore control file makes this entire subtree
            // indeterminate. Never expose entries by silently ignoring it.
            truncated = true;
            if !relative_directory.as_os_str().is_empty() {
                let subtree = relative_directory.to_string_lossy();
                matches.retain(|path, _| {
                    path.as_str() != subtree.as_ref()
                        && !path
                            .strip_prefix(subtree.as_ref())
                            .is_some_and(|suffix| suffix.starts_with('/'))
                });
            }
            continue;
        };
        let entries = rustix::fs::Dir::read_from(&directory)
            .map_err(|_| HostError::Query("workspace directory could not be read".to_owned()))?;
        for entry in entries {
            if started.elapsed() >= QUERY_DEADLINE || visited >= MAX_SEARCH_ENTRIES {
                truncated = true;
                break;
            }
            let entry = entry
                .map_err(|_| HostError::Query("workspace directory read failed".to_owned()))?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b".." | b".git") {
                continue;
            }
            let name = std::ffi::OsStr::from_bytes(name.to_bytes());
            let Some(name_text) = name.to_str() else {
                continue;
            };
            visited = visited.saturating_add(1);
            let Ok(child) = rustix::fs::openat(
                &directory,
                name,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            ) else {
                continue;
            };
            let Ok(stat) = rustix::fs::fstat(&child) else {
                continue;
            };
            let file_type = rustix::fs::FileType::from_raw_mode(stat.st_mode);
            if !file_type.is_file() && !file_type.is_dir() {
                continue;
            }
            let relative = relative_directory.join(name_text);
            if rules.is_ignored(&relative, file_type.is_dir()) {
                continue;
            }
            let rendered = relative.to_string_lossy().into_owned();
            if fuzzy_path_matches(&rendered, query) {
                matches.insert(rendered, file_type.is_dir());
                if matches.len() > limit {
                    let _ = matches.pop_last();
                    truncated = true;
                }
            }
            if file_type.is_dir() {
                pending.push((child, relative, rules.clone(), false));
            }
        }
        if started.elapsed() >= QUERY_DEADLINE || visited >= MAX_SEARCH_ENTRIES {
            break;
        }
    }
    let matches = matches
        .into_iter()
        .map(|(path, is_directory)| WorkspaceFileMatch { path, is_directory })
        .collect();
    Ok((matches, truncated))
}

#[cfg(not(unix))]
fn search_workspace(
    _workspace: &Path,
    _query: &str,
    _limit: usize,
) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
    Err(HostError::Query(
        "safe workspace search is unavailable on this platform".to_owned(),
    ))
}

fn read_workspace_status(
    workspace: &Path,
    workspace_name: String,
) -> Result<WorkspaceStatus, HostError> {
    let branch = read_git_branch(workspace)?;
    let (changed_paths, truncated) = read_git_changed_paths(workspace);
    Ok(WorkspaceStatus {
        workspace_name,
        branch,
        changed_paths,
        truncated,
    })
}

#[cfg(unix)]
struct GitCommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    overflow: bool,
}

#[cfg(unix)]
fn resolve_git_executable_from_candidates<'a>(
    candidates: impl IntoIterator<Item = &'a Path>,
) -> Option<PathBuf> {
    candidates.into_iter().find_map(|candidate| {
        let canonical = fs::canonicalize(candidate).ok()?;
        let metadata = fs::metadata(&canonical).ok()?;
        // Automatic status queries run without a user gesture. Only accept a
        // root-owned, executable system binary that cannot be replaced by an
        // unprivileged user. In particular, never execute a `git` selected
        // from the caller's user-writable PATH.
        (metadata.is_file()
            && metadata.uid() == 0
            && metadata.mode() & 0o022 == 0
            && metadata.permissions().mode() & 0o111 != 0)
            .then_some(canonical)
    })
}

#[cfg(unix)]
fn resolve_git_executable_for_caller_path(_caller_path: Option<&OsStr>) -> Option<PathBuf> {
    resolve_git_executable_from_candidates([Path::new("/usr/bin/git"), Path::new("/bin/git")])
}

#[cfg(unix)]
fn resolve_git_executable(_workspace: &Path) -> Option<PathBuf> {
    let caller_path = std::env::var_os("PATH");
    resolve_git_executable_for_caller_path(caller_path.as_deref())
}

#[cfg(unix)]
fn kill_git_process_group(child: &mut std::process::Child) {
    if let Ok(raw_pid) = i32::try_from(child.id())
        && let Some(pid) = rustix::process::Pid::from_raw(raw_pid)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn run_bounded_git(
    git: &Path,
    workspace: &Path,
    arguments: &[&OsStr],
    maximum: usize,
    deadline: Duration,
) -> Option<GitCommandOutput> {
    if !git.is_absolute() {
        return None;
    }
    let root = open_workspace_directory(workspace).ok()?;
    let root_stat = rustix::fs::fstat(&root).ok()?;
    let mut command = Command::new(git);
    command
        .current_dir(workspace)
        .arg("--no-optional-locks")
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "core.untrackedCache=false"])
        .args(["-c", "submodule.recurse=false"])
        .args(["-c", "diff.external="])
        .args(arguments)
        // Git itself is absolute and trusted. Restrict any helper lookup to
        // system locations as defense in depth against a hostile caller PATH.
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_PAGER", "cat")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut captured = Vec::new();
        let mut overflow = false;
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    let _ = sender.send(Some((captured, overflow)));
                    return;
                }
                Ok(read) => {
                    let remaining = maximum.saturating_sub(captured.len());
                    let retained = remaining.min(read);
                    captured.extend_from_slice(&buffer[..retained]);
                    overflow |= retained < read;
                }
                Err(_) => {
                    let _ = sender.send(None);
                    return;
                }
            }
        }
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < deadline => {
                thread::sleep(Duration::from_millis(1));
            }
            Ok(None) | Err(_) => {
                kill_git_process_group(&mut child);
                break None;
            }
        }
    };
    let status = status?;
    let output = if let Ok(output) = receiver.recv_timeout(GIT_READER_DEADLINE) {
        output
    } else {
        // A descendant inherited stdout after the Git leader exited.
        // Kill the isolated process group, then allow one bounded drain.
        kill_git_process_group(&mut child);
        receiver.recv_timeout(GIT_READER_DEADLINE).ok().flatten()
    };
    let (stdout, overflow) = output?;
    let identity_unchanged = open_workspace_directory(workspace)
        .and_then(|current| {
            rustix::fs::fstat(&current)
                .map_err(|_| HostError::Query("workspace identity is unavailable".to_owned()))
        })
        .is_ok_and(|current| {
            current.st_dev == root_stat.st_dev && current.st_ino == root_stat.st_ino
        });
    if !identity_unchanged {
        return None;
    }
    Some(GitCommandOutput {
        status,
        stdout,
        overflow,
    })
}

#[cfg(unix)]
fn read_git_changed_paths(workspace: &Path) -> (Vec<String>, bool) {
    let Ok(root) = open_workspace_directory(workspace) else {
        return (Vec::new(), true);
    };
    match rustix::fs::statat(&root, ".git", rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Err(error) if error == rustix::io::Errno::NOENT => return (Vec::new(), false),
        Ok(stat) => {
            let kind = rustix::fs::FileType::from_raw_mode(stat.st_mode);
            if !kind.is_file() && !kind.is_dir() {
                return (Vec::new(), true);
            }
        }
        Err(_) => return (Vec::new(), true),
    }
    let Some(git) = resolve_git_executable(workspace) else {
        return (Vec::new(), true);
    };
    let arguments = [
        OsStr::new("status"),
        OsStr::new("--porcelain=v1"),
        OsStr::new("-z"),
        OsStr::new("--untracked-files=all"),
        OsStr::new("--ignored=no"),
    ];
    let Some(output) = run_bounded_git(
        &git,
        workspace,
        &arguments,
        MAX_GIT_STATUS_BYTES,
        GIT_STATUS_DEADLINE,
    ) else {
        return (Vec::new(), true);
    };
    if !output.status.success() {
        return (Vec::new(), true);
    }
    parse_git_status(&output.stdout, output.overflow)
}

#[cfg(not(unix))]
fn read_git_changed_paths(_workspace: &Path) -> (Vec<String>, bool) {
    (Vec::new(), false)
}

#[cfg(unix)]
fn parse_git_status(bytes: &[u8], mut truncated: bool) -> (Vec<String>, bool) {
    let complete_bytes = match bytes.iter().rposition(|byte| *byte == 0) {
        Some(last_nul) => {
            truncated |= last_nul.saturating_add(1) != bytes.len();
            &bytes[..=last_nul]
        }
        None => {
            return (Vec::new(), !bytes.is_empty() || truncated);
        }
    };
    let mut paths = BTreeSet::new();
    let mut records = complete_bytes.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if record.len() < 4 || record[2] != b' ' {
            truncated = true;
            continue;
        }
        let status = &record[..2];
        let path = &record[3..];
        if status.iter().any(|code| matches!(*code, b'R' | b'C')) {
            // Porcelain v1 -z follows a rename/copy destination with its
            // source path. Only the destination is actionable in the UI.
            if records.next().is_none() {
                truncated = true;
            }
        }
        let Ok(path) = std::str::from_utf8(path) else {
            truncated = true;
            continue;
        };
        let Ok(path) = safe_relative_path(path) else {
            truncated = true;
            continue;
        };
        if path
            .components()
            .any(|component| component.as_os_str() == ".git")
        {
            continue;
        }
        let Some(path) = path.to_str() else {
            truncated = true;
            continue;
        };
        paths.insert(path.to_owned());
        if paths.len() >= MAX_CHANGED_PATHS {
            truncated |= records.any(|record| !record.is_empty());
            break;
        }
    }
    (paths.into_iter().collect(), truncated)
}

#[cfg(unix)]
// Keeping classification, identity-bound Git calls, and fail-closed branches
// together makes the security order auditable.
#[allow(clippy::too_many_lines)]
fn read_workspace_diff(
    workspace: &Path,
    relative: &Path,
    maximum: usize,
) -> Result<WorkspaceDiff, HostError> {
    let path = relative
        .to_str()
        .filter(|path| {
            !path.is_empty()
                && !path
                    .chars()
                    .any(|character| character.is_control() || character == '\0')
        })
        .ok_or_else(|| {
            HostError::Query("workspace diff path is not safely renderable".to_owned())
        })?;
    let root = open_workspace_directory(workspace)?;
    let git_marker = rustix::fs::statat(&root, ".git", rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| HostError::Query("workspace is not a readable Git repository".to_owned()))?;
    let marker_kind = rustix::fs::FileType::from_raw_mode(git_marker.st_mode);
    if !marker_kind.is_file() && !marker_kind.is_dir() {
        return Err(HostError::Query(
            "workspace Git metadata is unsafe".to_owned(),
        ));
    }
    let git = resolve_git_executable(workspace)
        .ok_or_else(|| HostError::Query("trusted Git executable is unavailable".to_owned()))?;
    let relative_os = relative.as_os_str();
    let tracked_arguments = [
        OsStr::new("ls-files"),
        OsStr::new("--error-unmatch"),
        OsStr::new("--"),
        relative_os,
    ];
    let tracked = run_bounded_git(&git, workspace, &tracked_arguments, 1, GIT_DIFF_DEADLINE)
        .ok_or_else(|| HostError::Query("Git path classification failed".to_owned()))?;
    if tracked.status.success() {
        let arguments = [
            OsStr::new("diff"),
            OsStr::new("--no-ext-diff"),
            OsStr::new("--no-textconv"),
            OsStr::new("--no-color"),
            OsStr::new("HEAD"),
            OsStr::new("--"),
            relative_os,
        ];
        let output = run_bounded_git(&git, workspace, &arguments, maximum, GIT_DIFF_DEADLINE)
            .ok_or_else(|| HostError::Query("Git diff failed".to_owned()))?;
        if !output.status.success() {
            return Err(HostError::Query("Git diff failed".to_owned()));
        }
        let binary = output
            .stdout
            .windows(12)
            .any(|window| window == b"Binary files")
            || output
                .stdout
                .windows(16)
                .any(|window| window == b"GIT binary patch");
        let (unified_diff, truncated, invalid_utf8) =
            if let Ok(diff) = String::from_utf8(output.stdout) {
                let (diff, truncated) = bounded_diff_text(diff, maximum, output.overflow);
                (diff, truncated, false)
            } else {
                let (diff, truncated) = bounded_diff_text(binary_diff(path), maximum, true);
                (diff, truncated, true)
            };
        return Ok(WorkspaceDiff {
            path: path.to_owned(),
            unified_diff,
            truncated,
            binary: binary || invalid_utf8,
        });
    }
    if tracked.status.code() != Some(1) {
        return Err(HostError::Query(
            "Git path classification failed".to_owned(),
        ));
    }

    let ignored_arguments = [
        OsStr::new("check-ignore"),
        OsStr::new("--quiet"),
        OsStr::new("--"),
        relative_os,
    ];
    let ignored = run_bounded_git(&git, workspace, &ignored_arguments, 1, GIT_DIFF_DEADLINE)
        .ok_or_else(|| HostError::Query("Git ignore classification failed".to_owned()))?;
    if ignored.status.success() {
        return Err(HostError::Query(
            "workspace diff refuses Git-ignored files".to_owned(),
        ));
    }
    if ignored.status.code() != Some(1) {
        return Err(HostError::Query(
            "Git ignore classification failed".to_owned(),
        ));
    }

    let file = open_relative_regular_file(&root, relative)?;
    let stat = rustix::fs::fstat(&file)
        .map_err(|_| HostError::Query("workspace file metadata is unavailable".to_owned()))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(HostError::Query(
            "workspace diff accepts regular files only".to_owned(),
        ));
    }
    let total = usize::try_from(stat.st_size).unwrap_or(usize::MAX);
    let mut bytes = Vec::new();
    fs::File::from(file)
        .take(u64::try_from(maximum).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| HostError::Query("workspace file could not be read".to_owned()))?;
    let source_truncated = total > maximum || bytes.len() > maximum;
    bytes.truncate(maximum);
    let executable = stat.st_mode & 0o111 != 0;
    if bytes.contains(&0) {
        let (unified_diff, truncated) =
            bounded_diff_text(binary_diff(path), maximum, source_truncated);
        return Ok(WorkspaceDiff {
            path: path.to_owned(),
            unified_diff,
            truncated,
            binary: true,
        });
    }
    let Ok(content) = String::from_utf8(bytes) else {
        let (unified_diff, truncated) = bounded_diff_text(binary_diff(path), maximum, true);
        return Ok(WorkspaceDiff {
            path: path.to_owned(),
            unified_diff,
            truncated,
            binary: true,
        });
    };
    let rendered = render_untracked_diff(path, &content, executable);
    let (unified_diff, truncated) = bounded_diff_text(rendered, maximum, source_truncated);
    Ok(WorkspaceDiff {
        path: path.to_owned(),
        unified_diff,
        truncated,
        binary: false,
    })
}

#[cfg(not(unix))]
fn read_workspace_diff(
    _workspace: &Path,
    _relative: &Path,
    _maximum: usize,
) -> Result<WorkspaceDiff, HostError> {
    Err(HostError::Query(
        "safe workspace diff is unavailable on this platform".to_owned(),
    ))
}

#[cfg(unix)]
fn render_untracked_diff(path: &str, content: &str, executable: bool) -> String {
    let line_count = content.lines().count().max(1);
    let mut diff = format!(
        "diff --git a/{path} b/{path}\nnew file mode {}\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{line_count} @@\n",
        if executable { "100755" } else { "100644" }
    );
    for line in content.split_inclusive('\n') {
        diff.push('+');
        diff.push_str(line);
    }
    if !content.is_empty() && !content.ends_with('\n') {
        diff.push_str("\n\\ No newline at end of file\n");
    }
    if content.is_empty() {
        diff.push_str("+\n");
    }
    diff
}

#[cfg(unix)]
fn binary_diff(path: &str) -> String {
    format!("diff --git a/{path} b/{path}\nBinary files /dev/null and b/{path} differ\n")
}

#[cfg(unix)]
fn bounded_diff_text(mut text: String, maximum: usize, mut truncated: bool) -> (String, bool) {
    if text.len() <= maximum {
        return (text, truncated);
    }
    truncated = true;
    let mut end = maximum;
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    end = text[..end].rfind('\n').map_or(0, |newline| newline + 1);
    text.truncate(end);
    (text, truncated)
}

#[cfg(unix)]
fn read_git_branch(workspace: &Path) -> Result<Option<String>, HostError> {
    open_workspace_directory(workspace)?;
    let Some(git) = resolve_git_executable(workspace) else {
        return Ok(None);
    };
    let symbolic = [
        OsStr::new("symbolic-ref"),
        OsStr::new("--quiet"),
        OsStr::new("--short"),
        OsStr::new("HEAD"),
    ];
    if let Some(output) = run_bounded_git(&git, workspace, &symbolic, 512, GIT_STATUS_DEADLINE)
        && output.status.success()
        && !output.overflow
        && let Some(branch) = safe_git_label(&output.stdout)
    {
        return Ok(Some(branch));
    }
    let detached = [
        OsStr::new("rev-parse"),
        OsStr::new("--short=12"),
        OsStr::new("HEAD"),
    ];
    if let Some(output) = run_bounded_git(&git, workspace, &detached, 64, GIT_STATUS_DEADLINE)
        && output.status.success()
        && !output.overflow
        && let Some(revision) = safe_git_label(&output.stdout)
    {
        return Ok(Some(format!("detached@{revision}")));
    }
    Ok(None)
}

#[cfg(unix)]
fn safe_git_label(bytes: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(bytes).ok()?.trim();
    if value.is_empty()
        || value.len() > 256
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return None;
    }
    Some(value.to_owned())
}

#[cfg(not(unix))]
fn read_git_branch(_workspace: &Path) -> Result<Option<String>, HostError> {
    Ok(None)
}

#[cfg(unix)]
fn open_workspace_directory(workspace: &Path) -> Result<OwnedFd, HostError> {
    rustix::fs::open(
        workspace,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| HostError::Query("workspace directory could not be opened safely".to_owned()))
}

#[cfg(unix)]
fn open_relative_regular_file(root: &OwnedFd, relative: &Path) -> Result<OwnedFd, HostError> {
    let components = relative.components().collect::<Vec<_>>();
    let mut directory = rustix::fs::openat(
        root,
        ".",
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| HostError::Query("workspace directory could not be opened safely".to_owned()))?;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(HostError::Query("workspace path is invalid".to_owned()));
        };
        let final_component = index.saturating_add(1) == components.len();
        let mut flags = rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC;
        if !final_component {
            flags |= rustix::fs::OFlags::DIRECTORY;
        }
        let opened = rustix::fs::openat(&directory, *name, flags, rustix::fs::Mode::empty())
            .map_err(|_| {
                HostError::Query("workspace path could not be opened safely".to_owned())
            })?;
        if final_component {
            return Ok(opened);
        }
        directory = opened;
    }
    Err(HostError::Query("workspace path is invalid".to_owned()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use rw_store::session::SessionEventLog;

    #[cfg(unix)]
    use std::time::Instant;

    use rw_core::{ModelAliasDescriptor, ModelCacheBehavior, ModelCapabilities, ModelDescriptor};
    use rw_store::session::{SessionProjection, SessionSummary as StoredSessionSummary};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn editable_setting_keys_round_trip_through_one_grammar() {
        for key in [
            "ui.keybindings.preset",
            "project.models.default",
            "ui.theme",
            "models.thinking.fast",
            "compaction.auto",
            "permissions.default",
            "budget.session_cost_cap_micros_usd",
            "budget.daily_cost_cap_micros_usd",
            "budget.warn_at_percent",
            "mcp.servers.docs.enabled",
            "mcp.add_http.docs",
        ] {
            let parsed = EditableSettingKey::parse(key)
                .unwrap_or_else(|| panic!("setting key should parse: {key}"));
            assert_eq!(parsed.render(), key);
        }

        for key in [
            "models.default",
            "models.thinking.",
            "mcp.add_http.",
            "mcp.add_http.has/slash",
            "mcp.servers..enabled",
            "mcp.servers.docs.enabled.extra",
            "mcp.servers.docs.with.dot.enabled",
        ] {
            assert!(EditableSettingKey::parse(key).is_none(), "parsed {key}");
        }
    }

    #[test]
    fn setting_descriptors_render_keys_from_the_editable_contract() {
        let root = tempdir().expect("root");
        let user = root.path().join("user/config.toml");
        let project = root.path().join("repo/.rottweiler/config.toml");
        fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        fs::write(
            &user,
            "[models]\ndefault = \"fast\"\n[models.aliases]\nfast = [\"openai/gpt-5-mini\"]\n",
        )
        .expect("user config");
        let loaded = ConfigLoader::new(user, project)
            .load()
            .expect("loaded config");
        let session = SessionDescriptor {
            session_id: SessionId("settings-contract".to_owned()),
            title: "Settings contract".to_owned(),
            workspace_name: "repo".to_owned(),
            model: ModelAlias("fast".to_owned()),
            driver_client_id: None,
            shell_active: false,
        };
        let settings = RuntimeSessionFactory::setting_descriptors(
            &loaded,
            &session,
            Some("openai/gpt-5-mini"),
            "vim",
            &[("docs".to_owned(), true)],
        );

        for descriptor in settings {
            let parsed = EditableSettingKey::parse(&descriptor.key)
                .unwrap_or_else(|| panic!("descriptor key should parse: {}", descriptor.key));
            assert_eq!(parsed.render(), descriptor.key);
        }
    }

    #[test]
    fn catalog_current_keeps_selected_alias_and_marks_actual_fallback_route() {
        let capabilities = ModelCapabilities {
            tool_calling: true,
            vision: false,
            thinking: false,
            cache_behavior: ModelCacheBehavior::None,
            max_context_tokens: None,
            max_output_tokens: None,
        };
        let model = |id: &str| ModelDescriptor {
            id: id.to_owned(),
            display_name: id.to_owned(),
            provider: id
                .split_once('/')
                .map_or("", |(provider, _)| provider)
                .to_owned(),
            aliases: vec![ModelAlias("fast".to_owned())],
            current: false,
            available: true,
            status: None,
            capabilities: capabilities.clone(),
        };
        let mut catalog = ModelCatalogSnapshot {
            aliases: vec![ModelAliasDescriptor {
                alias: ModelAlias("fast".to_owned()),
                candidates: vec!["primary/model".to_owned(), "fallback/model".to_owned()],
                current: false,
            }],
            models: vec![model("primary/model"), model("fallback/model")],
            providers: Vec::new(),
            cached: false,
            truncated: false,
        };
        overlay_catalog_current(&mut catalog, Some("fast"), Some("fallback/model"));
        assert!(catalog.aliases[0].current);
        assert!(!catalog.models[0].current);
        assert!(catalog.models[1].current);

        overlay_catalog_current(&mut catalog, Some("primary/model"), Some("fallback/model"));
        assert!(!catalog.aliases[0].current);
        assert!(catalog.models[0].current);
        assert!(!catalog.models[1].current);
    }

    fn factory(root: &Path, workspace: &Path) -> RuntimeSessionFactory {
        factory_with_allowed_workspaces(root, vec![workspace.to_path_buf()])
    }

    fn factory_with_allowed_workspaces(
        root: &Path,
        allowed_workspaces: Vec<PathBuf>,
    ) -> RuntimeSessionFactory {
        let storage_root = private_test_directory(&root.join("state"));
        RuntimeSessionFactory::new(RuntimeHostOptions {
            credentials_path: storage_root.join("credentials.json"),
            storage_root,
            config: Config::default(),
            allowed_workspaces,
            permission_mode: Some(PermissionMode::Strict),
            max_turns: 2,
            provider_mode: HostedProviderMode::DeterministicReplay {
                provider_name: "offline-host".to_owned(),
                scripts: Vec::new(),
                event_delay_ms: 0,
            },
            dangerously_trust: false,
            wait_for_execution_lease: false,
        })
        .expect("factory")
    }

    fn private_test_directory(path: &Path) -> PathBuf {
        fs::create_dir_all(path).expect("private test directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .expect("private test directory permissions");
        }
        fs::canonicalize(path).expect("canonical private test directory")
    }

    #[tokio::test]
    async fn factory_initialization_defers_pricing_catalog_parse() {
        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let storage_root = private_test_directory(&root.path().join("state"));
        fs::write(storage_root.join("models.toml"), "not valid pricing").expect("pricing fixture");

        let factory = RuntimeSessionFactory::new(RuntimeHostOptions {
            credentials_path: storage_root.join("credentials.json"),
            storage_root,
            config: Config::default(),
            allowed_workspaces: vec![workspace],
            permission_mode: Some(PermissionMode::Strict),
            max_turns: 2,
            provider_mode: HostedProviderMode::DeterministicReplay {
                provider_name: "offline-host".to_owned(),
                scripts: Vec::new(),
                event_delay_ms: 0,
            },
            dangerously_trust: false,
            wait_for_execution_lease: false,
        })
        .expect("readiness must not parse pricing");

        let error = factory
            .model_catalog(true, None, None)
            .await
            .expect_err("the first live catalog lookup must report invalid pricing");
        assert!(error.to_string().contains("invalid pricing table"));
    }

    #[test]
    fn durable_session_queries_tolerate_blocking_pool_scheduling_delay() {
        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let factory = factory(root.path(), &workspace);
        SessionIndex::open(&factory.options.storage_root)
            .and_then(|index| {
                index.upsert(&SessionProjection {
                    summary: StoredSessionSummary {
                        id: "scheduling-delay".to_owned(),
                        title: "Scheduling delay".to_owned(),
                        updated_unix_ms: 1,
                        cost_micros: 0,
                        turn_count: 1,
                    },
                    transcript: "durable query scheduling".to_owned(),
                    projected_through: None,
                })
            })
            .expect("searchable session index");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_time()
            .build()
            .expect("bounded test runtime");

        runtime.block_on(async move {
            let (started, running) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started.send(());
                std::thread::sleep(Duration::from_millis(250));
            });
            running.await.expect("blocking worker started");
            assert!(
                factory
                    .persisted_sessions()
                    .await
                    .expect("session list after scheduling delay")
                    .is_empty()
            );
            blocker.await.expect("first blocker");

            let (started, running) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started.send(());
                std::thread::sleep(Duration::from_millis(250));
            });
            running.await.expect("blocking worker started");
            let (sessions, truncated) = factory
                .search_persisted_sessions("scheduling", 10)
                .await
                .expect("session search after scheduling delay");
            assert!(sessions.is_empty());
            assert!(!truncated);
            blocker.await.expect("second blocker");
        });
    }

    #[tokio::test]
    async fn hosted_create_and_rename_are_immediately_searchable() {
        use rw_core::{
            ClientCommand, ClientId, ClientRole, CommandMeta, CommandOutcome, PROTOCOL_VERSION,
            RequestId,
        };

        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let factory = factory(root.path(), &workspace);
        SessionIndex::open(&factory.options.storage_root).expect("empty session index");
        let session_id = SessionId("hosted-search-freshness".to_owned());
        let driver = ClientId("hosted-search-driver".to_owned());
        let hosted = factory
            .create(CreateSessionRequest {
                session_id: session_id.clone(),
                workspace: workspace.display().to_string(),
                model: None,
            })
            .await
            .expect("hosted session");
        let mut events = hosted.handle().subscribe().expect("subscription");
        assert_eq!(
            hosted
                .handle()
                .dispatch(ClientCommand::AttachSession {
                    meta: CommandMeta {
                        protocol_version: PROTOCOL_VERSION,
                        client_id: driver.clone(),
                        request_id: RequestId("hosted-search-attach".to_owned()),
                    },
                    session_id: session_id.clone(),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                })
                .await
                .expect("attach"),
            CommandOutcome::Accepted
        );
        let (created, truncated) = factory
            .search_persisted_sessions("New session", 10)
            .await
            .expect("search created session");
        assert!(!truncated);
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].session_id, session_id);
        assert_eq!(
            hosted
                .handle()
                .dispatch(ClientCommand::RenameSession {
                    meta: CommandMeta {
                        protocol_version: PROTOCOL_VERSION,
                        client_id: driver,
                        request_id: RequestId("hosted-search-rename".to_owned()),
                    },
                    session_id: session_id.clone(),
                    title: "Durable Search Rename".to_owned(),
                })
                .await
                .expect("rename"),
            CommandOutcome::Accepted
        );
        loop {
            if matches!(
                events.recv().await.expect("rename event"),
                EngineEvent::SessionTitleUpdated { ref title, .. }
                    if title == "Durable Search Rename"
            ) {
                break;
            }
        }

        let (matches, truncated) = factory
            .search_persisted_sessions("Durable Search Rename", 10)
            .await
            .expect("search renamed session");
        assert!(!truncated);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].session_id, session_id);
        assert_eq!(matches[0].title, "Durable Search Rename");
    }

    #[test]
    fn session_export_uses_cli_renderer_redaction_and_atomic_force_semantics() {
        use rw_core::{EventMeta, PROTOCOL_VERSION, SequenceId};

        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let factory = factory(root.path(), &workspace);
        let session = SessionDescriptor {
            session_id: SessionId("golden".to_owned()),
            title: "Golden".to_owned(),
            workspace_name: workspace_name(&workspace),
            model: ModelAlias("fast".to_owned()),
            driver_client_id: Some(rw_core::ClientId("driver".to_owned())),
            shell_active: false,
        };
        let mut log = SessionEventLog::open(&factory.options.storage_root, "golden")
            .expect("session event log");
        log.append(EngineEvent::UiNotification {
            meta: EventMeta {
                protocol_version: PROTOCOL_VERSION,
                session_id: session.session_id.clone(),
                sequence_id: SequenceId(0),
                emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                caused_by: None,
            },
            plugin_id: "fixture".to_owned(),
            title: "<script>alert(1)</script>".to_owned(),
            message: "key sk-AbCdEf0123456789GhIjKlMn at /Users/alice/private".to_owned(),
        })
        .expect("fixture event");
        drop(log);

        let output_dir = tempdir().expect("output");
        let output = output_dir.path().join("transcript.md");
        let resolved = factory
            .export_session_blocking(&session, TranscriptFormat::Markdown, &output, false)
            .expect("first export");
        assert_eq!(
            resolved,
            fs::canonicalize(output_dir.path())
                .expect("canonical output")
                .join("transcript.md")
                .display()
                .to_string()
        );
        assert_eq!(
            fs::read(&output).expect("exported transcript"),
            include_bytes!("../tests/golden/history.md")
        );
        let rendered = fs::read_to_string(&output).expect("UTF-8 transcript");
        assert!(!rendered.contains("sk-AbCd"));
        assert!(!rendered.contains("/Users/alice"));

        let error = factory
            .export_session_blocking(&session, TranscriptFormat::Markdown, &output, false)
            .expect_err("existing output requires force");
        assert!(error.to_string().contains("pass --force"));
        fs::write(&output, b"replace me").expect("replacement canary");
        factory
            .export_session_blocking(&session, TranscriptFormat::Markdown, &output, true)
            .expect("forced export");
        assert_eq!(
            fs::read(&output).expect("forced transcript"),
            include_bytes!("../tests/golden/history.md")
        );

        assert!(
            factory
                .export_session_blocking(&session, TranscriptFormat::Json, Path::new("/"), false,)
                .is_err()
        );
    }

    #[cfg(unix)]
    fn git(workspace: &Path, arguments: &[&str]) {
        assert!(
            Command::new("git")
                .current_dir(workspace)
                .args(arguments)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("git fixture command")
                .success(),
            "git {arguments:?}"
        );
    }

    #[test]
    fn model_descriptors_expose_extension_provider_names_in_fallback_order() {
        let providers = configured_alias_providers(&[
            "openai-work/gpt-5".to_owned(),
            "extension-provider/model".to_owned(),
            "copilot/gpt-4.1".to_owned(),
            "openai-work/gpt-4.1".to_owned(),
            "malformed".to_owned(),
            "/missing-provider".to_owned(),
            "bad\nprovider/model".to_owned(),
            format!("{}/model", "x".repeat(MAX_PROVIDER_DISPLAY_NAME_BYTES + 1)),
        ]);
        assert_eq!(providers, ["openai-work", "extension-provider", "copilot"]);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_search_honors_git_nested_and_tool_ignore_files_but_keeps_hidden_files() {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(workspace.join(".git/info")).expect("git marker");
        fs::create_dir_all(workspace.join("ignored-dir")).expect("ignored directory");
        fs::create_dir_all(workspace.join("nested")).expect("nested directory");
        fs::write(
            workspace.join(".gitignore"),
            "ignored.txt\nignored-dir/*\nnested/*.rs\n",
        )
        .expect("gitignore");
        fs::write(
            workspace.join(".ignore"),
            "*.tmp\n!keep.tmp\n!ignored-dir/keep.rs\n",
        )
        .expect("tool ignore");
        fs::write(workspace.join(".git/info/exclude"), "info-excluded.rs\n")
            .expect("git info exclude");
        fs::write(workspace.join("ignored.txt"), "ignored").expect("ignored file");
        fs::write(workspace.join("ignored-dir/secret.rs"), "ignored").expect("ignored child");
        fs::write(workspace.join("ignored-dir/keep.rs"), "visible").expect("kept child");
        fs::write(workspace.join("scratch.tmp"), "ignored").expect("tool ignored file");
        fs::write(workspace.join("keep.tmp"), "visible").expect("tool whitelist");
        fs::write(workspace.join("info-excluded.rs"), "ignored").expect("info ignored file");
        fs::write(workspace.join(".hidden.rs"), "visible").expect("hidden file");
        fs::write(workspace.join("nested/.gitignore"), "!visible.rs\n").expect("nested gitignore");
        fs::write(workspace.join("nested/nested-ignored.rs"), "ignored")
            .expect("nested ignored file");
        fs::write(workspace.join("nested/visible.rs"), "visible").expect("visible file");
        fs::write(workspace.join(".git/HEAD"), "ref: refs/heads/main\n").expect("git internals");

        let (matches, truncated) = search_workspace(&workspace, "", 100).expect("search");
        assert!(!truncated);
        let paths = matches
            .into_iter()
            .map(|item| item.path)
            .collect::<BTreeSet<_>>();
        assert!(paths.contains(".hidden.rs"));
        assert!(paths.contains("keep.tmp"));
        assert!(paths.contains("nested/visible.rs"));
        assert!(!paths.contains("ignored.txt"));
        assert!(!paths.contains("ignored-dir/secret.rs"));
        assert!(!paths.contains("ignored-dir/keep.rs"));
        assert!(!paths.contains("scratch.tmp"));
        assert!(!paths.contains("info-excluded.rs"));
        assert!(!paths.contains("nested/nested-ignored.rs"));
        assert!(paths.iter().all(|path| !path.starts_with(".git/")));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_search_keeps_fuzzy_reachable_candidates_and_deterministic_bounds() {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(workspace.join("src")).expect("source directory");
        fs::write(workspace.join("src/main.rs"), "fn main() {}\n").expect("main source");
        fs::write(workspace.join("alpha.rs"), "alpha\n").expect("alpha");
        fs::write(workspace.join("beta.rs"), "beta\n").expect("beta");

        let (fuzzy, truncated) = search_workspace(&workspace, "smr", 10).expect("fuzzy pool");
        assert!(!truncated);
        assert!(fuzzy.iter().any(|item| item.path == "src/main.rs"));

        let (bounded, truncated) =
            search_workspace(&workspace, "", 2).expect("bounded deterministic pool");
        assert!(truncated);
        assert_eq!(
            bounded
                .iter()
                .map(|item| item.path.as_str())
                .collect::<Vec<_>>(),
            ["alpha.rs", "beta.rs"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_search_supports_linked_git_worktree_excludes() {
        let root = tempdir().expect("root");
        let repository = root.path().join("repository");
        let workspace = root.path().join("linked-worktree");
        fs::create_dir(&repository).expect("repository");
        git(&repository, &["init", "--quiet"]);
        git(&repository, &["config", "user.name", "Rottweiler Test"]);
        git(
            &repository,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(repository.join("tracked.rs"), "tracked\n").expect("tracked file");
        git(&repository, &["add", "tracked.rs"]);
        git(&repository, &["commit", "--quiet", "-m", "fixture"]);
        git(
            &repository,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked-fixture",
                workspace.to_str().expect("UTF-8 worktree path"),
            ],
        );
        fs::create_dir_all(repository.join(".git/info")).expect("Git info directory");
        fs::write(repository.join(".git/info/exclude"), "excluded.rs\n").expect("common exclude");
        fs::write(workspace.join("excluded.rs"), "excluded\n").expect("excluded file");
        fs::write(workspace.join("visible.rs"), "visible\n").expect("visible file");

        let (matches, truncated) = search_workspace(&workspace, "", 100).expect("search");
        assert!(!truncated);
        let paths = matches
            .into_iter()
            .map(|item| item.path)
            .collect::<BTreeSet<_>>();
        assert!(paths.contains("visible.rs"));
        assert!(paths.contains("tracked.rs"));
        assert!(!paths.contains("excluded.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_ignore_controls_fail_closed_for_only_the_affected_subtree() {
        use std::os::unix::fs::symlink;

        for fixture in ["symlink", "oversized", "invalid-utf8"] {
            let root = tempdir().expect("root");
            let workspace = root.path().join("workspace");
            fs::create_dir_all(workspace.join("bad")).expect("bad subtree");
            fs::write(workspace.join("safe.rs"), "safe").expect("safe sibling");
            fs::write(workspace.join("bad/secret.rs"), "secret").expect("secret file");
            match fixture {
                "symlink" => {
                    fs::write(root.path().join("outside-ignore"), "secret.rs\n")
                        .expect("outside ignore");
                    symlink(
                        root.path().join("outside-ignore"),
                        workspace.join("bad/.gitignore"),
                    )
                    .expect("ignore symlink");
                }
                "oversized" => fs::write(
                    workspace.join("bad/.gitignore"),
                    vec![b'x'; MAX_IGNORE_FILE_BYTES + 1],
                )
                .expect("oversized ignore"),
                "invalid-utf8" => fs::write(workspace.join("bad/.gitignore"), [0xff, b'\n'])
                    .expect("invalid ignore"),
                _ => unreachable!(),
            }

            let (matches, truncated) = search_workspace(&workspace, "", 100).expect("search");
            assert!(truncated, "{fixture}");
            let paths = matches
                .into_iter()
                .map(|item| item.path)
                .collect::<BTreeSet<_>>();
            assert!(paths.contains("safe.rs"), "{fixture}");
            assert!(!paths.contains("bad"), "{fixture}");
            assert!(!paths.contains("bad/secret.rs"), "{fixture}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn workspace_status_reports_modified_and_untracked_but_not_ignored_paths() {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        git(&workspace, &["init", "--quiet"]);
        git(&workspace, &["config", "user.name", "Rottweiler Test"]);
        git(
            &workspace,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(workspace.join(".gitignore"), "ignored.log\n").expect("gitignore");
        fs::write(workspace.join("tracked.rs"), "old\n").expect("tracked file");
        git(&workspace, &["add", ".gitignore", "tracked.rs"]);
        git(&workspace, &["commit", "--quiet", "-m", "fixture"]);
        fs::write(workspace.join("tracked.rs"), "new\n").expect("modified file");
        fs::write(workspace.join("untracked.rs"), "new\n").expect("untracked file");
        fs::write(workspace.join("ignored.log"), "ignored\n").expect("ignored file");

        let status = read_workspace_status(&workspace, "workspace".to_owned()).expect("status");
        assert!(!status.truncated);
        assert_eq!(status.changed_paths, ["tracked.rs", "untracked.rs"]);
        assert!(status.branch.is_some());

        fs::create_dir(workspace.join("nested")).expect("nested workspace");
        assert_eq!(
            read_git_branch(&workspace.join("nested")).expect("nested branch"),
            status.branch
        );

        let worktree = root.path().join("linked-worktree");
        git(
            &workspace,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked-branch",
                worktree.to_str().expect("worktree path"),
            ],
        );
        assert_eq!(
            read_git_branch(&worktree).expect("linked branch"),
            Some("linked-branch".to_owned())
        );

        git(&workspace, &["checkout", "--quiet", "--detach"]);
        let revision = Command::new("/usr/bin/git")
            .current_dir(&workspace)
            .args(["rev-parse", "--short=12", "HEAD"])
            .output()
            .expect("detached revision");
        assert!(revision.status.success());
        assert_eq!(
            read_git_branch(&workspace).expect("detached branch"),
            Some(format!(
                "detached@{}",
                String::from_utf8(revision.stdout)
                    .expect("UTF-8 revision")
                    .trim()
            ))
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_diff_covers_tracked_untracked_binary_ignored_and_truncated_files() {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        git(&workspace, &["init", "--quiet"]);
        git(&workspace, &["config", "user.name", "Rottweiler Test"]);
        git(
            &workspace,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(workspace.join(".gitignore"), "ignored.txt\n").expect("gitignore");
        fs::write(workspace.join("tracked.txt"), "old\n").expect("tracked text");
        fs::write(workspace.join("binary.bin"), [0, 1, 2]).expect("tracked binary");
        git(
            &workspace,
            &["add", ".gitignore", "tracked.txt", "binary.bin"],
        );
        git(&workspace, &["commit", "--quiet", "-m", "fixture"]);
        fs::write(workspace.join("tracked.txt"), "new\n").expect("modified text");
        fs::write(workspace.join("binary.bin"), [0, 1, 3]).expect("modified binary");
        fs::write(workspace.join("untracked.txt"), "hello\nworld\n").expect("untracked text");
        fs::write(workspace.join("ignored.txt"), "secret\n").expect("ignored text");
        fs::write(workspace.join("large.txt"), "line\n".repeat(1_000)).expect("large text");

        let tracked =
            read_workspace_diff(&workspace, Path::new("tracked.txt"), 8_192).expect("tracked diff");
        assert!(!tracked.binary);
        assert!(!tracked.truncated);
        assert!(tracked.unified_diff.contains("-old"));
        assert!(tracked.unified_diff.contains("+new"));

        let untracked = read_workspace_diff(&workspace, Path::new("untracked.txt"), 8_192)
            .expect("untracked diff");
        assert!(!untracked.binary);
        assert!(untracked.unified_diff.contains("--- /dev/null"));
        assert!(untracked.unified_diff.contains("+hello"));

        let binary =
            read_workspace_diff(&workspace, Path::new("binary.bin"), 8_192).expect("binary diff");
        assert!(binary.binary);
        assert!(binary.unified_diff.contains("Binary files"));

        let ignored = read_workspace_diff(&workspace, Path::new("ignored.txt"), 8_192)
            .expect_err("ignored diff must fail closed");
        assert!(ignored.to_string().contains("Git-ignored"));

        let large =
            read_workspace_diff(&workspace, Path::new("large.txt"), 128).expect("bounded diff");
        assert!(large.truncated);
        assert!(large.unified_diff.len() <= 128);
    }

    #[cfg(unix)]
    #[test]
    fn porcelain_parser_keeps_rename_destination_and_rejects_unsafe_paths() {
        let (paths, truncated) = parse_git_status(
            b"R  new.rs\0old.rs\0?? nested/untracked.rs\0?? ../escape\0?? partial.rs",
            false,
        );
        assert!(truncated);
        assert_eq!(paths, ["nested/untracked.rs", "new.rs"]);
    }

    #[cfg(unix)]
    #[test]
    fn git_resolution_rejects_user_owned_executables_and_uses_a_system_identity() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().expect("root");
        let hostile = root.path().join("git");
        fs::write(&hostile, "#!/bin/sh\nexit 0\n").expect("fake git");
        fs::set_permissions(&hostile, fs::Permissions::from_mode(0o700)).expect("fake git mode");

        assert!(resolve_git_executable_from_candidates([hostile.as_path()]).is_none());
        let hostile_path = std::env::join_paths([root.path()]).expect("hostile PATH");
        let resolved_for_hostile_path = resolve_git_executable_for_caller_path(Some(&hostile_path))
            .expect("system Git with hostile PATH");
        let system = resolve_git_executable(root.path()).expect("system Git identity");
        assert_eq!(resolved_for_hostile_path, system);
        assert_ne!(
            system,
            fs::canonicalize(hostile).expect("canonical hostile executable")
        );
        assert!(system.starts_with("/usr/bin") || system.starts_with("/bin"));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_git_kills_descendants_that_keep_stdout_open() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let git = root.path().join("fake-git");
        fs::write(
            &git,
            "#!/bin/sh\nsleep 5 &\nprintf '?? held.rs\\0'\nexit 0\n",
        )
        .expect("fake git");
        fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).expect("fake git mode");
        let started = Instant::now();
        let output = run_bounded_git(&git, &workspace, &[], 1024, Duration::from_secs(2))
            .expect("bounded output");
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(output.stdout, b"?? held.rs\0");
        assert!(!output.overflow);
    }

    #[tokio::test]
    async fn workspace_preview_fails_closed_for_traversal_symlink_and_binary_without_path_leakage()
    {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        fs::write(workspace.join("safe.txt"), "safe").expect("safe file");
        fs::write(workspace.join("binary.bin"), [0, 1, 2]).expect("binary file");
        #[cfg(unix)]
        std::os::unix::fs::symlink("safe.txt", workspace.join("link.txt")).expect("symlink");

        for path in ["../safe.txt", "/etc/passwd"] {
            let error = safe_relative_path(path).expect_err("unsafe relative path");
            assert!(!error.to_string().contains(&workspace.display().to_string()));
        }
        assert_eq!(
            safe_relative_path("nested//safe.txt").expect("normalized path"),
            Path::new("nested/safe.txt")
        );
        assert_eq!(
            split_virtual_path("@root/2/nested/safe.txt").expect("virtual path"),
            (2, PathBuf::from("nested/safe.txt"))
        );
        for path in ["@root/0/file", "@root/1", "@root/1/../escape"] {
            assert!(split_virtual_path(path).is_err(), "{path}");
        }
        for path in ["binary.bin", "link.txt"] {
            let relative = safe_relative_path(path).expect("normalized path");
            let error = preview_file(&workspace, &relative, 1024).expect_err("unsafe preview");
            assert!(!error.to_string().contains(&workspace.display().to_string()));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_preview_rejects_before_opening_under_one_hundred_milliseconds() {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let fifo = workspace.join("blocked.fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo fixture")
                .success()
        );
        let started = Instant::now();
        preview_file(&workspace, Path::new("blocked.fifo"), 1024).expect_err("FIFO must fail");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_queries_do_not_escape_during_directory_swap_race() {
        use std::{
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
            thread,
        };

        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let outside = root.path().join("outside");
        let swap = workspace.join("swap");
        let held = workspace.join("held");
        fs::create_dir_all(&swap).expect("safe directory");
        fs::create_dir_all(&outside).expect("outside directory");
        fs::write(swap.join("target.txt"), "SAFE").expect("safe file");
        fs::write(outside.join("target.txt"), "OUTSIDE_CANARY").expect("outside file");
        fs::write(outside.join("OUTSIDE_CANARY.txt"), "outside").expect("outside marker");

        let running = Arc::new(AtomicBool::new(true));
        let attacker_running = Arc::clone(&running);
        let attacker_swap = swap.clone();
        let attacker_held = held.clone();
        let attacker_outside = outside.clone();
        let attacker = thread::spawn(move || {
            while attacker_running.load(Ordering::Relaxed) {
                if fs::rename(&attacker_swap, &attacker_held).is_ok() {
                    std::os::unix::fs::symlink(&attacker_outside, &attacker_swap)
                        .expect("race symlink");
                    fs::remove_file(&attacker_swap).expect("remove race symlink");
                    fs::rename(&attacker_held, &attacker_swap).expect("restore safe directory");
                }
                thread::yield_now();
            }
        });

        for _ in 0..250 {
            if let Ok(preview) = preview_file(&workspace, Path::new("swap/target.txt"), 1024) {
                assert_eq!(
                    preview.data,
                    AttachmentData::Text {
                        content: "SAFE".to_owned()
                    }
                );
            }
            if let Ok((matches, _)) = search_workspace(&workspace, "OUTSIDE_CANARY", 10) {
                assert!(matches.is_empty(), "search escaped through a raced symlink");
            }
        }
        running.store(false, Ordering::Relaxed);
        attacker.join().expect("attacker thread");

        let preview = preview_file(&workspace, Path::new("swap/target.txt"), 1024)
            .expect("safe directory restored");
        assert_eq!(
            preview.data,
            AttachmentData::Text {
                content: "SAFE".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn create_persists_remote_safe_descriptor_and_resume_recovers_exact_identity() {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        fs::write(workspace.join("needle.rs"), "fn needle() {}\n").expect("query fixture");
        let factory = factory(root.path(), &workspace);
        let session_id = SessionId("session-create-resume".to_owned());
        let created = factory
            .create(CreateSessionRequest {
                session_id: session_id.clone(),
                workspace: workspace.display().to_string(),
                model: None,
            })
            .await
            .expect("create");
        assert_eq!(created.descriptor().session_id, session_id);
        assert!(!created.descriptor().workspace_name.contains('/'));
        let (matches, truncated) = factory
            .search_workspace_files(&created.descriptor(), "needle", 10)
            .await
            .expect("search");
        assert!(!truncated);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "needle.rs");
        let preview = factory
            .preview_workspace_file(&created.descriptor(), "needle.rs", 1024)
            .await
            .expect("preview");
        assert_eq!(
            preview.data,
            AttachmentData::Text {
                content: "fn needle() {}\n".to_owned()
            }
        );
        fs::write(
            workspace.join("screen shot.png"),
            b"\x89PNG\r\n\x1a\nattachment bytes",
        )
        .expect("image fixture");
        let preview = factory
            .preview_workspace_file(&created.descriptor(), "screen shot.png", 1024)
            .await
            .expect("image preview");
        assert_eq!(preview.media_type, "image/png");
        assert!(matches!(preview.data, AttachmentData::InlineBase64 { .. }));
        drop(created);
        tokio::task::yield_now().await;
        let resumed = factory.resume(&session_id).await.expect("resume");
        assert_eq!(resumed.descriptor().session_id, session_id);
        assert_eq!(resumed.descriptor().workspace_name, "workspace");
    }

    #[tokio::test]
    async fn hosted_add_dir_enforces_allowed_roots_before_generation_or_tool_access() {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let allowed = root.path().join("allowed");
        let outside = root.path().join("outside");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&allowed).expect("allowed");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("OUTSIDE_CANARY.txt"), "outside").expect("outside canary");
        let workspace = fs::canonicalize(workspace).expect("canonical workspace");
        let allowed = fs::canonicalize(allowed).expect("canonical allowed");
        let outside = fs::canonicalize(outside).expect("canonical outside");
        let factory =
            factory_with_allowed_workspaces(root.path(), vec![workspace.clone(), allowed.clone()]);
        let session_id = SessionId("hosted-add-root-policy".to_owned());
        let hosted = factory
            .create(CreateSessionRequest {
                session_id: session_id.clone(),
                workspace: workspace.display().to_string(),
                model: None,
            })
            .await
            .expect("create hosted session");
        let handle = hosted.handle();

        let denied = handle
            .send_message(format!("/add-dir {}", outside.display()))
            .await
            .expect_err("outside root must be denied");
        assert!(denied.to_string().contains("authorization policy"));
        let unchanged = handle.snapshot().await.expect("unchanged snapshot");
        assert_eq!(unchanged.workspace_generation, 0);
        assert_eq!(unchanged.workspace_roots.len(), 1);
        assert_eq!(
            factory
                .workspace_roots_for_session(&hosted.descriptor())
                .expect("host roots after denial"),
            vec![workspace.clone()]
        );
        assert!(
            factory
                .preview_workspace_file(&hosted.descriptor(), "@root/1/OUTSIDE_CANARY.txt", 1024,)
                .await
                .is_err(),
            "denied root must not become queryable through hosted tool paths"
        );

        let allowed_session_id = SessionId("hosted-add-root-allowed".to_owned());
        let allowed_hosted = factory
            .create(CreateSessionRequest {
                session_id: allowed_session_id,
                workspace: workspace.display().to_string(),
                model: None,
            })
            .await
            .expect("create allowed-root session");
        let allowed_handle = allowed_hosted.handle();
        allowed_handle
            .send_message(format!("/add-dir {}", allowed.display()))
            .await
            .expect("configured allowed root");
        let changed = allowed_handle.snapshot().await.expect("changed snapshot");
        assert_eq!(changed.workspace_generation, 1);
        assert_eq!(changed.workspace_roots.len(), 2);
        assert_eq!(
            factory
                .workspace_roots_for_session(&allowed_hosted.descriptor())
                .expect("host roots after allowed add"),
            vec![workspace, allowed]
        );
        assert!(!outside.join("created-by-tool.txt").exists());
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn production_factory_fork_composes_and_resumes_child() {
        use rw_core::{
            ClientCommand, ClientId, ClientRole, CommandMeta, CommandOutcome, ForkOperationKey,
            PROTOCOL_VERSION, PreparedForkOperation, RequestId, TurnId,
        };
        use rw_providers::{FinishReason, ProviderEvent};

        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let storage_root = private_test_directory(&root.path().join("state"));
        let factory = RuntimeSessionFactory::new(RuntimeHostOptions {
            credentials_path: storage_root.join("credentials.json"),
            storage_root: storage_root.clone(),
            config: Config::default(),
            allowed_workspaces: vec![workspace.clone()],
            permission_mode: Some(PermissionMode::Strict),
            max_turns: 2,
            provider_mode: HostedProviderMode::DeterministicReplay {
                provider_name: "fork-production-offline".to_owned(),
                scripts: vec![vec![
                    ProviderEvent::TextDelta {
                        text: "completed parent turn".to_owned(),
                    },
                    ProviderEvent::Finished {
                        reason: FinishReason::Stop,
                    },
                ]],
                event_delay_ms: 0,
            },
            dangerously_trust: false,
            wait_for_execution_lease: false,
        })
        .expect("factory");
        let parent_id = SessionId("production-fork-parent".to_owned());
        let driver = ClientId("production-driver".to_owned());
        let parent = factory
            .create(CreateSessionRequest {
                session_id: parent_id.clone(),
                workspace: workspace.display().to_string(),
                model: None,
            })
            .await
            .expect("parent composes");
        let mut events = parent.handle().subscribe().expect("subscription");
        assert_eq!(
            parent
                .handle()
                .dispatch(ClientCommand::AttachSession {
                    meta: CommandMeta {
                        protocol_version: PROTOCOL_VERSION,
                        client_id: driver.clone(),
                        request_id: RequestId("production-attach".to_owned()),
                    },
                    session_id: parent_id.clone(),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                })
                .await
                .expect("attach dispatch"),
            CommandOutcome::Accepted
        );
        assert_eq!(
            parent
                .handle()
                .dispatch(ClientCommand::SendMessage {
                    meta: CommandMeta {
                        protocol_version: PROTOCOL_VERSION,
                        client_id: driver.clone(),
                        request_id: RequestId("production-message".to_owned()),
                    },
                    session_id: parent_id.clone(),
                    content: "complete one durable turn".to_owned(),
                    attachments: Vec::new(),
                })
                .await
                .expect("parent message"),
            CommandOutcome::Accepted
        );
        loop {
            if matches!(
                events.recv().await.expect("parent event"),
                rw_core::EngineEvent::TurnFinished { .. }
            ) {
                break;
            }
        }
        let switched_model = ModelAlias("historical-parent-later-model".to_owned());
        assert_eq!(
            parent
                .handle()
                .dispatch(ClientCommand::SwitchModel {
                    meta: CommandMeta {
                        protocol_version: PROTOCOL_VERSION,
                        client_id: driver.clone(),
                        request_id: RequestId("production-switch-after-boundary".to_owned()),
                    },
                    session_id: parent_id.clone(),
                    model: switched_model.clone(),
                    provider: None,
                })
                .await
                .expect("switch parent model after fork boundary"),
            CommandOutcome::Accepted
        );
        let model_question_id = loop {
            if let rw_core::EngineEvent::QuestionAsked {
                question_id,
                questions,
                ..
            } = events.recv().await.expect("parent model question")
                && questions.iter().any(|question| {
                    question
                        .model_switch
                        .as_ref()
                        .is_some_and(|target| target.model == switched_model)
                })
            {
                break question_id;
            }
        };
        assert_eq!(
            parent
                .handle()
                .dispatch(ClientCommand::AnswerQuestion {
                    meta: CommandMeta {
                        protocol_version: PROTOCOL_VERSION,
                        client_id: driver.clone(),
                        request_id: RequestId("production-switch-context".to_owned()),
                    },
                    session_id: parent_id.clone(),
                    question_id: model_question_id.clone(),
                    answers: vec![rw_core::Answer {
                        question_id: model_question_id,
                        values: vec!["pass_full_context".to_owned()],
                    }],
                })
                .await
                .expect("answer parent model context question"),
            CommandOutcome::Accepted
        );
        loop {
            if matches!(
                events.recv().await.expect("parent model event"),
                rw_core::EngineEvent::ModelChanged { ref model, .. } if *model == switched_model
            ) {
                break;
            }
        }
        let parent_path = storage_root
            .join("sessions")
            .join(&parent_id.0)
            .join("journal")
            .join("active.jsonl");
        let parent_before = fs::read(&parent_path).expect("parent bytes");
        let child_id = SessionId("production-fork-child".to_owned());
        let fork_turn = TurnId("1".to_owned());
        let fork_payload_hash = blake3::hash(
            &serde_json::to_vec(&(&parent_id, &Some(fork_turn.clone())))
                .expect("stable fork payload"),
        )
        .to_hex()
        .to_string();
        let operation_key = ForkOperationKey {
            operation_id: "production-fork-operation".to_owned(),
            client_id: driver.clone(),
            request_id: RequestId("production-fork".to_owned()),
            payload_hash: fork_payload_hash.clone(),
        };
        let fork_request = ForkSessionRequest {
            operation_key: operation_key.clone(),
            parent: SessionDescriptor {
                driver_client_id: Some(driver.clone()),
                model: switched_model,
                ..parent.descriptor()
            },
            child_session_id: child_id.clone(),
            at_turn: fork_turn.clone(),
            through_sequence: None,
            include_idle_tail: false,
            driver_client_id: driver.clone(),
        };
        factory
            .prepare_fork_operation(PreparedForkOperation {
                key: operation_key,
                request: fork_request.clone(),
            })
            .await
            .expect("prepare production fork");
        let child = factory
            .fork(fork_request)
            .await
            .expect("production fork composes");
        assert_eq!(child.descriptor().session_id, child_id);
        assert_eq!(child.descriptor().model, ModelAlias("fast".to_owned()));
        let snapshot = child.handle().snapshot().await.expect("child snapshot");
        assert_eq!(snapshot.completed_turns, 1);
        assert_eq!(snapshot.driver_client_id, Some(driver));
        assert_eq!(
            fs::read(parent_path).expect("parent after fork"),
            parent_before
        );
        assert!(
            rw_store::session::AccountingLedger::open(&storage_root)
                .expect("accounting ledger")
                .entries_for_session(&child.descriptor().session_id.0)
                .expect("child accounting")
                .is_empty()
        );
        assert!(
            storage_root
                .join("sessions")
                .join(&child_id.0)
                .join("metadata.json")
                .is_file()
        );

        let durable_key = ForkOperationKey {
            operation_id: "production-fork-operation".to_owned(),
            client_id: ClientId("production-driver".to_owned()),
            request_id: RequestId("production-fork".to_owned()),
            payload_hash: fork_payload_hash,
        };
        let mut journal = factory
            .load_fork_journal(&durable_key)
            .expect("load storage-committed journal")
            .expect("journal exists");
        assert!(matches!(journal.state, ForkJournalState::StorageCommitted));
        // Simulate a kill after metadata fsync but before the phase rewrite.
        journal.state = ForkJournalState::Prepared;
        factory
            .force_replace_fork_journal_for_test(&journal)
            .expect("simulate prepared crash state");
        let restart_options = (*factory.options).clone();
        drop(events);
        drop(child);
        drop(parent);
        drop(factory);
        tokio::task::yield_now().await;
        let restarted = Arc::new(
            RuntimeSessionFactory::new(restart_options.clone()).expect("restart recovery"),
        );
        let promoted = restarted
            .load_fork_journal(&durable_key)
            .expect("load promoted journal")
            .expect("promoted journal exists");
        assert!(matches!(promoted.state, ForkJournalState::StorageCommitted));
        assert_eq!(
            RuntimeSessionFactory::journal_operation(&promoted)
                .request
                .child_session_id,
            child_id
        );
        let restarted_client_key = ForkOperationKey {
            operation_id: durable_key.operation_id.clone(),
            client_id: ClientId("replacement-driver".to_owned()),
            request_id: RequestId("retry-after-process-restart".to_owned()),
            payload_hash: durable_key.payload_hash.clone(),
        };
        let host = rw_core::EngineHost::new(
            rw_core::EngineHostConfig::default(),
            restarted.clone(),
            restarted.clone(),
        )
        .expect("restart host");
        let replacement = rw_core::BoundClient {
            client_id: restarted_client_key.client_id.clone(),
        };
        let mut replacement_events = host
            .subscribe(replacement.clone(), None, None)
            .await
            .expect("replacement event stream");
        assert_eq!(
            host.dispatch(
                replacement,
                rw_core::ClientCommand::Fork {
                    meta: rw_core::CommandMeta {
                        protocol_version: rw_core::PROTOCOL_VERSION,
                        client_id: ClientId("wire-spoof-is-replaced".to_owned()),
                        request_id: restarted_client_key.request_id.clone(),
                    },
                    session_id: parent_id.clone(),
                    at_turn: Some(fork_turn.clone()),
                    operation_id: restarted_client_key.operation_id.clone(),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let replayed_child = loop {
            if let EngineEvent::SessionForked { child, meta, .. } = replacement_events
                .recv()
                .await
                .expect("replayed fork event")
                .expect("replayed fork result")
            {
                assert_eq!(meta.client_id, restarted_client_key.client_id);
                assert_eq!(meta.request_id, restarted_client_key.request_id);
                break child;
            }
        };
        assert_eq!(replayed_child.session_id, child_id);
        assert_eq!(
            replayed_child.driver_client_id,
            Some(restarted_client_key.client_id.clone())
        );
        let completion = match restarted
            .load_fork_operation(&restarted_client_key)
            .await
            .expect("load completion after stable retry")
        {
            ForkOperationState::Completed(completion) => completion,
            state => panic!("stable retry did not complete: {state:?}"),
        };
        assert_eq!(completion.child, replayed_child);
        let mut racing_completion = completion.clone();
        racing_completion.command_ack_emitted_at = "2026-07-11T00:00:02.000Z".to_owned();
        racing_completion.fork_event_emitted_at = "2026-07-11T00:00:03.000Z".to_owned();
        assert_eq!(
            restarted
                .complete_fork_operation(&restarted_client_key, &racing_completion)
                .await
                .expect("racing completion returns authoritative result"),
            completion
        );
        journal.state = ForkJournalState::StorageCommitted;
        let monotonic = restarted
            .transition_fork_journal_for_test(&journal)
            .expect("stale transition is read-modify-write guarded");
        assert!(matches!(
            monotonic.state,
            ForkJournalState::Completed { .. }
        ));
        drop(replacement_events);
        drop(host);
        drop(restarted);
        tokio::task::yield_now().await;
        let mut grown_child = SessionEventLog::open(&storage_root, &child_id.0)
            .expect("open completed child for post-fork growth");
        let target_events = crate::history::MAX_HISTORY_EVENTS + 1;
        while usize::try_from(grown_child.next_sequence()).expect("child sequence") < target_events
        {
            let start = grown_child.next_sequence();
            let remaining =
                target_events.saturating_sub(usize::try_from(start).expect("child sequence index"));
            let count = remaining.min(10_000);
            let batch = (0..count)
                .map(|offset| {
                    let sequence = start + u64::try_from(offset).expect("batch offset");
                    EngineEvent::ModeChanged {
                        meta: rw_core::EventMeta {
                            protocol_version: rw_core::PROTOCOL_VERSION,
                            session_id: child_id.clone(),
                            sequence_id: rw_core::SequenceId(sequence),
                            emitted_at: "2026-07-11T00:00:04Z".to_owned(),
                            caused_by: None,
                        },
                        mode: rw_core::ModeId("execute".to_owned()),
                        definition_fingerprint: "fixture".to_owned(),
                    }
                })
                .collect::<Vec<_>>();
            grown_child
                .append_batch(batch)
                .expect("append valid post-fork child events");
        }
        drop(grown_child);
        assert!(matches!(
            SessionEventLog::load_existing_bounded::<EngineEvent>(
                &storage_root,
                &child_id.0,
                crate::history::MAX_HISTORY_BYTES,
                crate::history::MAX_HISTORY_EVENTS,
            ),
            Err(rw_store::session::SessionStoreError::EventCountTooLarge { .. })
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(
                storage_root
                    .join("sessions")
                    .join(&child_id.0)
                    .join("journal")
                    .join("active.jsonl"),
                fs::Permissions::from_mode(0o000),
            )
            .expect("install completed-child no-read canary");
            fs::set_permissions(
                crate::session_runtime::checkpoint_root(&storage_root, &workspace, &child_id.0)
                    .join("workspace-roots.json"),
                fs::Permissions::from_mode(0o000),
            )
            .expect("install completed-child root-journal no-read canary");
        }
        let reloaded =
            Arc::new(RuntimeSessionFactory::new(restart_options).expect("completed restart"));
        assert_eq!(
            reloaded
                .load_fork_operation(&durable_key)
                .await
                .expect("reload completed result"),
            ForkOperationState::Completed(completion.clone())
        );
        let second_restart_key = ForkOperationKey {
            operation_id: durable_key.operation_id.clone(),
            client_id: ClientId("second-replacement-driver".to_owned()),
            request_id: RequestId("retry-after-second-restart".to_owned()),
            payload_hash: durable_key.payload_hash.clone(),
        };
        assert_eq!(
            reloaded
                .load_fork_operation(&second_restart_key)
                .await
                .expect("stable operation id survives client and request replacement"),
            ForkOperationState::Completed(completion.clone())
        );
        let host = rw_core::EngineHost::new(
            rw_core::EngineHostConfig::default(),
            reloaded.clone(),
            reloaded.clone(),
        )
        .expect("restart host");
        let replacement = rw_core::BoundClient {
            client_id: second_restart_key.client_id.clone(),
        };
        let mut replacement_events = host
            .subscribe(replacement.clone(), None, None)
            .await
            .expect("replacement event stream");
        assert_eq!(
            host.dispatch(
                replacement,
                rw_core::ClientCommand::Fork {
                    meta: rw_core::CommandMeta {
                        protocol_version: rw_core::PROTOCOL_VERSION,
                        client_id: ClientId("wire-spoof-is-replaced".to_owned()),
                        request_id: second_restart_key.request_id.clone(),
                    },
                    session_id: completion.parent_session_id.clone(),
                    at_turn: Some(completion.at_turn.clone()),
                    operation_id: second_restart_key.operation_id.clone(),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let replayed_child = loop {
            if let EngineEvent::SessionForked { child, meta, .. } = replacement_events
                .recv()
                .await
                .expect("replayed fork event")
                .expect("replayed fork result")
            {
                assert_eq!(meta.client_id, second_restart_key.client_id);
                assert_eq!(meta.request_id, second_restart_key.request_id);
                break child;
            }
        };
        assert_eq!(replayed_child.session_id, completion.child.session_id);
        let conflict = ForkOperationKey {
            payload_hash: "b".repeat(64),
            ..durable_key
        };
        assert_eq!(
            reloaded
                .load_fork_operation(&conflict)
                .await
                .expect_err("payload conflict"),
            HostError::RequestConflict
        );
    }

    #[tokio::test]
    async fn prepared_fork_recovery_cleans_partial_trees_and_keeps_child_identity() {
        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let factory = factory(root.path(), &workspace);
        let parent = factory
            .create(CreateSessionRequest {
                session_id: SessionId("journal-parent".to_owned()),
                workspace: workspace.display().to_string(),
                model: None,
            })
            .await
            .expect("parent");
        let key = ForkOperationKey {
            operation_id: "journal-operation".to_owned(),
            client_id: rw_core::ClientId("journal-client".to_owned()),
            request_id: rw_core::RequestId("journal-request".to_owned()),
            payload_hash: "c".repeat(64),
        };
        let child = SessionId("journal-child".to_owned());
        let operation = PreparedForkOperation {
            key: key.clone(),
            request: ForkSessionRequest {
                operation_key: key.clone(),
                parent: parent.descriptor(),
                child_session_id: child.clone(),
                at_turn: rw_core::TurnId("0".to_owned()),
                through_sequence: None,
                include_idle_tail: false,
                driver_client_id: key.client_id.clone(),
            },
        };
        factory
            .prepare_fork_operation(operation.clone())
            .await
            .expect("prepare");
        let session_tree = factory.options.storage_root.join("sessions").join(&child.0);
        fs::create_dir_all(session_tree.join("journal")).expect("partial session tree");
        fs::write(session_tree.join("journal/active.jsonl"), b"partial").expect("partial log");
        fs::write(session_tree.join(".metadata-crash.tmp"), br#"{"version":1"#)
            .expect("unpublished metadata temporary");
        let digest = blake3::hash(workspace.as_os_str().as_encoded_bytes())
            .to_hex()
            .to_string();
        let checkpoint_tree = factory
            .options
            .storage_root
            .join("workspaces")
            .join(digest)
            .join("sessions")
            .join(&child.0);
        fs::create_dir_all(&checkpoint_tree).expect("partial checkpoint tree");
        fs::write(checkpoint_tree.join("partial"), b"partial").expect("partial checkpoint");
        let restarted = RuntimeSessionFactory::new((*factory.options).clone()).expect("recover");
        assert!(!session_tree.exists());
        assert!(!checkpoint_tree.exists());
        assert_eq!(
            restarted
                .load_fork_operation(&key)
                .await
                .expect("load prepared"),
            ForkOperationState::Pending(operation)
        );
    }

    #[tokio::test]
    async fn session_capacity_rejection_abandons_prepared_fork_journal() {
        use rw_core::{
            BoundClient, ClientCommand, ClientId, ClientRole, CommandMeta, CommandOutcome,
            EngineHost, EngineHostConfig, PROTOCOL_VERSION, RequestId,
        };

        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let factory = Arc::new(factory(root.path(), &workspace));
        let host = EngineHost::new(
            EngineHostConfig {
                max_sessions: 1,
                max_deduplicated_requests: 64,
            },
            factory.clone(),
            factory.clone(),
        )
        .expect("host");
        let parent = SessionId("capacity-parent".to_owned());
        host.prepare_session(
            CreateSessionRequest {
                session_id: parent.clone(),
                workspace: workspace.display().to_string(),
                model: None,
            },
            false,
        )
        .await
        .expect("parent");
        let driver = BoundClient {
            client_id: ClientId("capacity-driver".to_owned()),
        };
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::AttachSession {
                    meta: CommandMeta {
                        protocol_version: PROTOCOL_VERSION,
                        client_id: driver.client_id.clone(),
                        request_id: RequestId("capacity-attach".to_owned()),
                    },
                    session_id: parent.clone(),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let outcome = host
            .dispatch(
                driver.clone(),
                ClientCommand::Fork {
                    meta: CommandMeta {
                        protocol_version: PROTOCOL_VERSION,
                        client_id: driver.client_id,
                        request_id: RequestId("capacity-fork".to_owned()),
                    },
                    session_id: parent,
                    at_turn: None,
                    operation_id: "capacity-fork-operation".to_owned(),
                },
            )
            .await;
        assert!(matches!(
            outcome,
            CommandOutcome::Rejected { error } if error.code == "session_capacity"
        ));
        assert!(
            fs::read_dir(factory.fork_journal_directory())
                .expect("journal directory")
                .all(|entry| entry
                    .expect("journal entry")
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    != Some("json"))
        );
    }

    #[test]
    #[cfg(unix)]
    fn fork_journal_cross_process_lock_helper() {
        let Ok(root) = std::env::var("RW_TEST_FORK_LOCK_ROOT") else {
            return;
        };
        let workspace =
            PathBuf::from(std::env::var("RW_TEST_FORK_LOCK_WORKSPACE").expect("helper workspace"));
        let ready = PathBuf::from(std::env::var("RW_TEST_FORK_LOCK_READY").expect("helper ready"));
        let release =
            PathBuf::from(std::env::var("RW_TEST_FORK_LOCK_RELEASE").expect("helper release"));
        let factory = factory(Path::new(&root), &workspace);
        let _lock = factory
            .acquire_fork_journal_lock()
            .expect("helper acquires lock");
        fs::write(ready, b"ready").expect("helper ready marker");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !release.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(release.exists(), "parent releases helper lock");
    }

    #[test]
    #[cfg(unix)]
    fn fork_recovery_waits_for_cross_process_journal_lock() {
        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let factory = factory(root.path(), &workspace);
        let options = (*factory.options).clone();
        let ready = root.path().join("lock-ready");
        let release = root.path().join("lock-release");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .arg("--exact")
            .arg("session_host::tests::fork_journal_cross_process_lock_helper")
            .arg("--nocapture")
            .env("RW_TEST_FORK_LOCK_ROOT", root.path())
            .env("RW_TEST_FORK_LOCK_WORKSPACE", &workspace)
            .env("RW_TEST_FORK_LOCK_READY", &ready)
            .env("RW_TEST_FORK_LOCK_RELEASE", &release)
            .spawn()
            .expect("lock helper");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "helper acquired cross-process lock");

        let (send, receive) = std::sync::mpsc::channel();
        let recovery = std::thread::spawn(move || {
            let result = RuntimeSessionFactory::new(options);
            send.send(result.is_ok()).expect("recovery result");
        });
        assert!(
            receive.recv_timeout(Duration::from_millis(100)).is_err(),
            "recovery must wait while another process owns the journal lock"
        );
        fs::write(&release, b"release").expect("release marker");
        assert!(child.wait().expect("helper exit").success());
        assert!(
            receive
                .recv_timeout(Duration::from_secs(5))
                .expect("recovery completes")
        );
        recovery.join().expect("recovery thread");
    }

    #[test]
    #[cfg(unix)]
    fn fork_journal_rejects_unexpected_symlink_hardlink_and_oversized_entries() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let factory = factory(root.path(), &workspace);
        let options = (*factory.options).clone();
        let directory = factory.fork_journal_directory();

        let unpublished = directory.join(".fork-crash.tmp");
        fs::write(&unpublished, br#"{"version":1"#).expect("unpublished journal temporary");
        fs::set_permissions(&unpublished, fs::Permissions::from_mode(0o600))
            .expect("private unpublished journal");
        RuntimeSessionFactory::new(options.clone()).expect("orphan temporary is recoverable");
        assert!(!unpublished.exists());

        fs::write(directory.join("unexpected"), b"x").expect("unexpected entry");
        assert!(RuntimeSessionFactory::new(options.clone()).is_err());
        fs::remove_file(directory.join("unexpected")).expect("remove unexpected");

        let outside = root.path().join("outside");
        fs::write(&outside, b"{}").expect("outside");
        symlink(&outside, directory.join(format!("{}.json", "a".repeat(64)))).expect("symlink");
        assert!(RuntimeSessionFactory::new(options.clone()).is_err());
        fs::remove_file(directory.join(format!("{}.json", "a".repeat(64))))
            .expect("remove symlink");

        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).expect("private source");
        fs::hard_link(&outside, directory.join(format!("{}.json", "b".repeat(64))))
            .expect("hardlink");
        assert!(RuntimeSessionFactory::new(options.clone()).is_err());
        fs::remove_file(directory.join(format!("{}.json", "b".repeat(64))))
            .expect("remove hardlink");

        let oversized = directory.join(format!("{}.json", "c".repeat(64)));
        fs::write(&oversized, vec![b'x'; MAX_FORK_JOURNAL_BYTES + 1]).expect("oversized");
        fs::set_permissions(&oversized, fs::Permissions::from_mode(0o600)).expect("private file");
        assert!(RuntimeSessionFactory::new(options).is_err());
    }

    #[test]
    fn thinking_setting_uses_configured_alias_after_concrete_model_selection() {
        let root = tempdir().expect("root");
        let user = root.path().join("user/config.toml");
        let project = root.path().join("repo/.rottweiler/config.toml");
        fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        fs::write(
            &user,
            "[models]\ndefault = \"fast\"\n[models.aliases]\nfast = [\"openai/gpt-5-mini\"]\n",
        )
        .expect("user config");
        let loaded = ConfigLoader::new(user, project)
            .load()
            .expect("loaded config");
        let session = SessionDescriptor {
            session_id: SessionId("concrete".to_owned()),
            title: "Concrete model".to_owned(),
            workspace_name: "repo".to_owned(),
            model: ModelAlias("openai/gpt-5-mini".to_owned()),
            driver_client_id: None,
            shell_active: false,
        };

        let settings =
            RuntimeSessionFactory::setting_descriptors(&loaded, &session, None, "standard", &[]);

        assert!(
            settings
                .iter()
                .any(|setting| setting.key == "models.thinking.fast")
        );
        assert!(
            settings
                .iter()
                .all(|setting| !setting.key.contains("openai/gpt-5-mini"))
        );
    }

    #[test]
    fn theme_setting_leaves_choices_to_the_tui_theme_catalog() {
        let root = tempdir().expect("root");
        let user = root.path().join("user/config.toml");
        let project = root.path().join("repo/.rottweiler/config.toml");
        fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        let loaded = ConfigLoader::new(user, project)
            .load()
            .expect("loaded config");
        let session = SessionDescriptor {
            session_id: SessionId("theme-settings".to_owned()),
            title: "Theme settings".to_owned(),
            workspace_name: "repo".to_owned(),
            model: ModelAlias("fast".to_owned()),
            driver_client_id: None,
            shell_active: false,
        };

        let settings =
            RuntimeSessionFactory::setting_descriptors(&loaded, &session, None, "standard", &[]);
        let theme = settings
            .iter()
            .find(|setting| setting.key == "ui.theme")
            .expect("theme setting");

        assert!(theme.choices.is_empty());
    }

    #[test]
    fn budget_setting_descriptors_format_human_values_without_choices() {
        let root = tempdir().expect("root");
        let user = root.path().join("user/config.toml");
        let project = root.path().join("repo/.rottweiler/config.toml");
        fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        let mut loaded = ConfigLoader::new(user, project)
            .load()
            .expect("loaded config");
        loaded.config.budget.session_cost_cap_micros_usd = Some(12_500_000);
        loaded.config.budget.daily_cost_cap_micros_usd = None;
        loaded.config.budget.warn_at_percent = 80;
        let session = SessionDescriptor {
            session_id: SessionId("budget-settings".to_owned()),
            title: "Budget settings".to_owned(),
            workspace_name: "repo".to_owned(),
            model: ModelAlias("fast".to_owned()),
            driver_client_id: None,
            shell_active: false,
        };

        let settings =
            RuntimeSessionFactory::setting_descriptors(&loaded, &session, None, "standard", &[]);
        let descriptor = |key: &str| {
            settings
                .iter()
                .find(|setting| setting.key == key)
                .unwrap_or_else(|| panic!("missing descriptor {key}"))
        };

        assert_eq!(
            descriptor("budget.session_cost_cap_micros_usd").value,
            "$12.50"
        );
        assert_eq!(
            descriptor("budget.daily_cost_cap_micros_usd").value,
            "Unlimited"
        );
        assert_eq!(descriptor("budget.warn_at_percent").value, "80%");
        for key in [
            "budget.session_cost_cap_micros_usd",
            "budget.daily_cost_cap_micros_usd",
            "budget.warn_at_percent",
        ] {
            assert!(descriptor(key).choices.is_empty());
            assert!(!descriptor(key).applies_immediately);
        }
    }

    #[test]
    fn project_model_preferences_are_isolated_by_the_session_workspace() {
        let root = tempdir().expect("root");
        let first = private_test_directory(&root.path().join("first"));
        let second = private_test_directory(&root.path().join("second"));
        let factory =
            factory_with_allowed_workspaces(root.path(), vec![first.clone(), second.clone()]);

        factory
            .settings_loader_for(&first)
            .persist_tui_project_model("openai/first")
            .expect("first preference");
        factory
            .settings_loader_for(&second)
            .persist_tui_project_model("openai/second")
            .expect("second preference");

        assert_eq!(
            factory
                .settings_loader_for(&first)
                .tui_project_model()
                .expect("first")
                .as_deref(),
            Some("openai/first")
        );
        assert_eq!(
            factory
                .settings_loader_for(&second)
                .tui_project_model()
                .expect("second")
                .as_deref(),
            Some("openai/second")
        );
    }

    #[test]
    fn fresh_factory_uses_the_persisted_project_model_without_catalog_interaction() {
        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let first = factory(root.path(), &workspace);
        first
            .settings_loader_for(&workspace)
            .persist_tui_project_model("openai_codex/gpt-5.6-sol")
            .expect("persist selected model");
        drop(first);

        let restarted = factory(root.path(), &workspace);
        assert_eq!(
            restarted
                .requested_model_for_compose(&workspace, None, false)
                .expect("load the restart selection")
                .as_deref(),
            Some("openai_codex/gpt-5.6-sol")
        );
    }

    #[test]
    fn resume_ignores_a_corrupt_project_model_preference() {
        let root = tempdir().expect("root");
        let workspace = private_test_directory(&root.path().join("workspace"));
        let factory = factory(root.path(), &workspace);
        let preference = factory
            .options
            .credentials_path
            .with_file_name("project-model-preferences.json");
        fs::write(&preference, "not-json").expect("corrupt preference");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&preference, fs::Permissions::from_mode(0o600))
                .expect("private corrupt preference");
        }

        assert_eq!(
            factory
                .requested_model_for_compose(&workspace, None, true)
                .expect("resume ignores preference"),
            None
        );
        assert!(
            factory
                .requested_model_for_compose(&workspace, None, false)
                .is_err()
        );
    }
}
