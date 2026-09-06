//! CLI composition for the headless multi-session engine host.

mod command_receipts;
mod factory;
mod fork_journal;
mod git;
mod queries;
mod workspace;
use git::{read_workspace_diff, read_workspace_status};
use workspace::{
    open_relative_regular_file, open_workspace_directory, preview_file, safe_relative_path,
    search_workspaces, split_virtual_path,
};

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
    merge_model_catalog_provider, retain_model_catalog_provider,
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
    receipt_io: Arc<tokio::sync::Mutex<()>>,
    plugin_runtime_budget: Arc<crate::extension_runtime::PluginRuntimeBudget>,
    wasm_workers: Arc<rw_ext::WasmWorkerPool>,
    index_pool: Arc<rw_tools::WorkspaceIndexPool>,
    journal_service: Arc<crate::journal_service::JournalService>,
    transcripts: Arc<crate::transcript_service::TranscriptReader>,
    provider_admission: Arc<crate::provider_admission::DurableProviderAdmission>,
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
    fn generation(&self) -> u64 {
        self.inner.generation()
    }

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
    pub async fn new(mut options: RuntimeHostOptions) -> Result<Self, HostError> {
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
        let journal_service = crate::journal_service::JournalService::new(&options.storage_root)
            .map_err(|error| HostError::Persistence(error.to_string()))?;
        let provider_admission =
            crate::provider_admission::DurableProviderAdmission::open(options.storage_root.clone())
                .await
                .map_err(|error| HostError::Persistence(error.to_string()))?;
        let factory = Self {
            receipt_io: Arc::default(),
            provider_admission: Arc::new(provider_admission),
            transcripts: crate::transcript_service::TranscriptReader::new(Arc::clone(
                &journal_service,
            )),
            plugin_runtime_budget: Arc::new(
                crate::extension_runtime::PluginRuntimeBudget::default(),
            ),
            wasm_workers: rw_ext::WasmWorkerPool::new(),
            index_pool: Arc::new(rw_tools::WorkspaceIndexPool::default()),
            journal_service,
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
            .journal_service
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
            &self.journal_service,
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
            plugin_runtime_budget: Arc::clone(&self.plugin_runtime_budget),
            wasm_workers: Arc::clone(&self.wasm_workers),
            index_pool: Arc::clone(&self.index_pool),
            journal_service: Arc::clone(&self.journal_service),
            transcripts: Arc::clone(&self.transcripts),
            provider_admission: Arc::clone(&self.provider_admission),
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

#[cfg(test)]
mod tests;
