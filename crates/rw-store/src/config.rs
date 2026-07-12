//! Layered TOML configuration loading with per-leaf provenance.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rw_types::config::{
    Config, ConfigFile, EngineConfigFile, PermissionDecision, ProviderConfig, ThinkingLevel,
    UpdateChannel,
};
use thiserror::Error;
use url::{Host, Url};

use crate::trust::{FolderTrustError, FolderTrustState, FolderTrustStore};

const ENV_ENGINE_SESSIONS: &str = "RW_ENGINE_MAX_CONCURRENT_SESSIONS";
const ENV_SUBAGENT_DEPTH: &str = "RW_ENGINE_SUBAGENT_MAX_DEPTH";
const ENV_SUBAGENT_CONCURRENCY: &str = "RW_ENGINE_SUBAGENT_MAX_CONCURRENCY";
const ENV_MODEL_DEFAULT: &str = "RW_MODEL_DEFAULT";
const ENV_COMPACTION_AUTO: &str = "RW_COMPACTION_AUTO";
const ENV_COMPACTION_RESERVED: &str = "RW_COMPACTION_RESERVED";
const ENV_COMPACTION_RESERVED_TOKENS: &str = "RW_COMPACTION_RESERVED_TOKENS";
const ENV_COMPACTION_MODEL_ALIAS: &str = "RW_COMPACTION_MODEL_ALIAS";
const ENV_BUDGET_SESSION_COST_CAP: &str = "RW_BUDGET_SESSION_COST_CAP_MICROS_USD";
const ENV_BUDGET_DAILY_COST_CAP: &str = "RW_BUDGET_DAILY_COST_CAP_MICROS_USD";
const ENV_BUDGET_SESSION_CREDIT_CAP: &str = "RW_BUDGET_SESSION_AI_CREDIT_CAP_MICROS";
const ENV_BUDGET_DAILY_CREDIT_CAP: &str = "RW_BUDGET_DAILY_AI_CREDIT_CAP_MICROS";
const ENV_BUDGET_SPEND_RATE: &str = "RW_BUDGET_SPEND_RATE_ALARM_MICROS_USD_PER_MINUTE";
const ENV_BUDGET_CREDIT_RATE: &str = "RW_BUDGET_AI_CREDIT_RATE_ALARM_MICROS_PER_MINUTE";
const ENV_BUDGET_WARN_PERCENT: &str = "RW_BUDGET_WARN_AT_PERCENT";
const ENV_NETWORK_PROXY: &str = "RW_NETWORK_PROXY";
const ENV_NETWORK_PROXY_USERNAME: &str = "RW_NETWORK_PROXY_USERNAME";
const ENV_NETWORK_PROXY_PASSWORD_CREDENTIAL: &str = "RW_NETWORK_PROXY_PASSWORD_CREDENTIAL";
const ENV_PERMISSION_DEFAULT: &str = "RW_PERMISSION_DEFAULT";
const ENV_SANDBOX_SAFE_LIST: &str = "RW_SANDBOX_SAFE_LIST";
const ENV_TELEMETRY_ENABLED: &str = "RW_TELEMETRY_ENABLED";
const ENV_UPDATE_CHANNEL: &str = "RW_UPDATE_CHANNEL";
const MAX_TUI_AUX_CONFIG_BYTES: usize = 64 * 1024;
static TUI_SETTING_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
#[cfg(not(unix))]
static TUI_SETTING_PORTABLE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A layer that supplied one effective configuration leaf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Compiled-in safe defaults.
    BuiltIn,
    /// User-level TOML.
    UserFile(PathBuf),
    /// User-level TOML last updated through the engine-mediated TUI surface.
    UserTui(PathBuf),
    /// Trusted project-level TOML.
    ProjectFile(PathBuf),
    /// Environment variable.
    Environment(String),
    /// Explicit CLI override.
    Cli,
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuiltIn => formatter.write_str("built-in"),
            Self::UserFile(path) => write!(formatter, "user:{}", path.display()),
            Self::UserTui(path) => write!(formatter, "user (set via TUI):{}", path.display()),
            Self::ProjectFile(path) => write!(formatter, "project:{}", path.display()),
            Self::Environment(name) => write!(formatter, "env:{name}"),
            Self::Cli => formatter.write_str("cli"),
        }
    }
}

/// A security-sensitive project setting that was deliberately ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    message: String,
}

impl ConfigWarning {
    /// Human-readable warning text.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Fully resolved configuration plus provenance and warnings.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// Effective typed configuration.
    pub config: Config,
    provenance: BTreeMap<String, ConfigSource>,
    warnings: Vec<ConfigWarning>,
    project_trusted: bool,
}

impl LoadedConfig {
    /// Returns the source of a named effective leaf.
    #[must_use]
    pub fn provenance(&self, key: &str) -> Option<&ConfigSource> {
        self.provenance.get(key)
    }

    /// Security warnings raised while loading project configuration.
    #[must_use]
    pub fn warnings(&self) -> &[ConfigWarning] {
        &self.warnings
    }

    /// Whether the exact current project executable inventory was trusted.
    #[must_use]
    pub const fn project_trusted(&self) -> bool {
        self.project_trusted
    }

    /// Renders stable, scriptable effective values with a source per leaf.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn render_with_provenance(&self) -> String {
        let mut lines = vec![
            self.render_leaf(
                "engine.max_concurrent_sessions",
                &self.config.engine.max_concurrent_sessions.to_string(),
            ),
            self.render_leaf(
                "engine.subagent_max_depth",
                &self.config.engine.subagent_max_depth.to_string(),
            ),
            self.render_leaf(
                "engine.subagent_max_concurrency",
                &self.config.engine.subagent_max_concurrency.to_string(),
            ),
            self.render_leaf("models.default", &quoted(&self.config.models.default)),
        ];

        if self.config.models.aliases.is_empty() {
            lines.push(self.render_leaf("models.aliases", "{}"));
        } else {
            for (alias, candidates) in &self.config.models.aliases {
                let key = format!("models.aliases.{alias}");
                let value = candidates
                    .iter()
                    .map(|candidate| quoted(candidate))
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(self.render_leaf(&key, &format!("[{value}]")));
            }
        }

        if self.config.models.thinking.is_empty() {
            lines.push(self.render_leaf("models.thinking", "{}"));
        } else {
            for (alias, level) in &self.config.models.thinking {
                lines.push(self.render_leaf(
                    &format!("models.thinking.{alias}"),
                    thinking_level_name(*level),
                ));
            }
        }

        lines.push(self.render_leaf("compaction.auto", &self.config.compaction.auto.to_string()));
        lines.push(self.render_leaf(
            "compaction.reserved",
            &optional_u64(self.config.compaction.reserved_tokens),
        ));
        lines.push(
            self.render_leaf(
                "compaction.model_alias",
                &self
                    .config
                    .compaction
                    .model_alias
                    .as_deref()
                    .map_or_else(|| "<unset>".to_owned(), quoted),
            ),
        );
        for (key, value) in [
            (
                "budget.session_cost_cap_micros_usd",
                self.config.budget.session_cost_cap_micros_usd,
            ),
            (
                "budget.daily_cost_cap_micros_usd",
                self.config.budget.daily_cost_cap_micros_usd,
            ),
            (
                "budget.session_ai_credit_cap_micros",
                self.config.budget.session_ai_credit_cap_micros,
            ),
            (
                "budget.daily_ai_credit_cap_micros",
                self.config.budget.daily_ai_credit_cap_micros,
            ),
            (
                "budget.spend_rate_alarm_micros_usd_per_minute",
                self.config.budget.spend_rate_alarm_micros_usd_per_minute,
            ),
            (
                "budget.ai_credit_rate_alarm_micros_per_minute",
                self.config.budget.ai_credit_rate_alarm_micros_per_minute,
            ),
        ] {
            lines.push(self.render_leaf(key, &optional_u64(value)));
        }
        lines.push(self.render_leaf(
            "budget.warn_at_percent",
            &self.config.budget.warn_at_percent.to_string(),
        ));

        if self.config.providers.is_empty() {
            lines.push(self.render_leaf("providers", "{}"));
        } else {
            for (name, provider) in &self.config.providers {
                lines.push(
                    self.render_leaf(&format!("providers.{name}.kind"), &quoted(&provider.kind)),
                );
                if let Some(base_url) = &provider.base_url {
                    lines.push(
                        self.render_leaf(&format!("providers.{name}.base_url"), &quoted(base_url)),
                    );
                }
                if let Some(proxy) = &provider.proxy {
                    lines.push(
                        self.render_leaf(
                            &format!("providers.{name}.proxy"),
                            &redacted_proxy(proxy),
                        ),
                    );
                }
                if let Some(username) = &provider.proxy_username {
                    lines.push(self.render_leaf(
                        &format!("providers.{name}.proxy_username"),
                        &quoted(username),
                    ));
                }
                if let Some(credential) = &provider.proxy_password_credential {
                    lines.push(self.render_leaf(
                        &format!("providers.{name}.proxy_password_credential"),
                        &quoted(credential),
                    ));
                }
                if let Some(variable) = &provider.api_key_env {
                    lines.push(
                        self.render_leaf(
                            &format!("providers.{name}.api_key_env"),
                            &quoted(variable),
                        ),
                    );
                }
                if let Some(credential) = &provider.api_key_credential {
                    lines.push(self.render_leaf(
                        &format!("providers.{name}.api_key_credential"),
                        &quoted(credential),
                    ));
                }
                if let Some(variable) = &provider.oauth_token_env {
                    lines.push(self.render_leaf(
                        &format!("providers.{name}.oauth_token_env"),
                        &quoted(variable),
                    ));
                }
                for (field, value) in [
                    (
                        "oauth_authorization_endpoint",
                        provider.oauth_authorization_endpoint.as_deref(),
                    ),
                    (
                        "oauth_token_endpoint",
                        provider.oauth_token_endpoint.as_deref(),
                    ),
                    ("oauth_client_id", provider.oauth_client_id.as_deref()),
                    (
                        "oauth_access_token_credential",
                        provider.oauth_access_token_credential.as_deref(),
                    ),
                    (
                        "oauth_refresh_token_credential",
                        provider.oauth_refresh_token_credential.as_deref(),
                    ),
                ] {
                    if let Some(value) = value {
                        lines.push(
                            self.render_leaf(&format!("providers.{name}.{field}"), &quoted(value)),
                        );
                    }
                }
                if !provider.oauth_scopes.is_empty() {
                    let scopes = provider
                        .oauth_scopes
                        .iter()
                        .map(|scope| quoted(scope))
                        .collect::<Vec<_>>()
                        .join(", ");
                    lines.push(self.render_leaf(
                        &format!("providers.{name}.oauth_scopes"),
                        &format!("[{scopes}]"),
                    ));
                }
            }
        }

        lines.push(
            self.render_leaf(
                "network.proxy",
                &self
                    .config
                    .network
                    .proxy
                    .as_deref()
                    .map_or_else(|| "<unset>".to_owned(), redacted_proxy),
            ),
        );
        lines.push(
            self.render_leaf(
                "network.proxy_username",
                &self
                    .config
                    .network
                    .proxy_username
                    .as_deref()
                    .map_or_else(|| "<unset>".to_owned(), quoted),
            ),
        );
        lines.push(
            self.render_leaf(
                "network.proxy_password_credential",
                &self
                    .config
                    .network
                    .proxy_password_credential
                    .as_deref()
                    .map_or_else(|| "<unset>".to_owned(), quoted),
            ),
        );
        lines.push(
            self.render_leaf(
                "websearch.endpoint",
                &self
                    .config
                    .websearch
                    .endpoint
                    .as_deref()
                    .map_or_else(|| "<unset>".to_owned(), quoted),
            ),
        );
        lines.push(self.render_leaf(
            "websearch.query_parameter",
            &quoted(&self.config.websearch.query_parameter),
        ));
        lines.push(self.render_leaf(
            "websearch.header_credentials",
            &format!(
                "{:?}",
                self.config
                    .websearch
                    .header_credentials
                    .keys()
                    .collect::<Vec<_>>()
            ),
        ));
        lines.push(self.render_leaf(
            "permissions.default",
            permission_name(self.config.permissions.default),
        ));
        lines.push(self.render_leaf(
            "sandbox.safe_list",
            &format!("{:?}", self.config.sandbox.safe_list),
        ));
        lines.push(
            self.render_leaf(
                "toolchain.formatter",
                &self
                    .config
                    .toolchain
                    .formatter
                    .as_deref()
                    .map_or_else(|| "<unset>".to_owned(), quoted),
            ),
        );
        lines.push(self.render_leaf(
            "toolchain.linters",
            &format!("{:?}", self.config.toolchain.linters),
        ));
        lines.push(
            self.render_leaf(
                "toolchain.test",
                &self
                    .config
                    .toolchain
                    .test
                    .as_deref()
                    .map_or_else(|| "<unset>".to_owned(), quoted),
            ),
        );
        lines.push(self.render_leaf(
            "toolchain.rules",
            &format!("{:?}", self.config.toolchain.rules),
        ));
        lines.push(self.render_leaf(
            "telemetry.enabled",
            &self.config.telemetry.enabled.to_string(),
        ));
        lines.push(self.render_leaf(
            "updates.channel",
            update_channel_name(self.config.updates.channel),
        ));
        lines.push(self.render_leaf("ui.theme", &quoted(&self.config.ui.theme)));
        lines.join("\n") + "\n"
    }

    fn render_leaf(&self, key: &str, value: &str) -> String {
        let source = self
            .provenance
            .get(key)
            .or_else(|| parent_alias_source(&self.provenance, key))
            .map_or_else(|| "built-in".to_owned(), ToString::to_string);
        format!("{key} = {value} [{source}]")
    }
}

/// Typed failures from configuration discovery, parsing, and validation.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The current working directory could not be determined.
    #[error("could not determine the current directory: {0}")]
    CurrentDirectory(#[source] std::io::Error),
    /// No safe user-level configuration root is available.
    #[error(
        "could not determine the user configuration directory; set ROTTWEILER_HOME, XDG_CONFIG_HOME, or HOME"
    )]
    MissingUserConfigRoot,
    /// A configured root is relative and could resolve inside an untrusted project.
    #[error("{name} must be an absolute path, got {value:?}")]
    InvalidUserConfigRoot {
        /// Environment variable name.
        name: String,
        /// Rejected value.
        value: String,
    },
    /// User and project files resolve to the same path and cannot be scoped safely.
    #[error("user and project configuration resolve to the same path: {0}")]
    ScopeCollision(PathBuf),
    /// A configuration file could not be read.
    #[error("could not read configuration file {path}: {source}")]
    Read {
        /// File path.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A user setting could not be persisted atomically.
    #[error("could not persist user setting in {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A TUI attempted to write a key or value outside the safe allowlist.
    #[error("invalid TUI setting {key:?}: {reason}")]
    InvalidUserSetting { key: String, reason: String },
    /// TOML did not match the typed schema.
    #[error("invalid configuration in {path}: {source}")]
    Parse {
        /// File path.
        path: PathBuf,
        /// TOML/schema error.
        #[source]
        source: toml::de::Error,
    },
    /// An environment value was malformed.
    #[error("invalid value {value:?} for environment variable {name}: {reason}")]
    Environment {
        /// Variable name.
        name: String,
        /// Supplied value.
        value: String,
        /// Expected form.
        reason: String,
    },
    /// A `--set` override was malformed or unknown.
    #[error("invalid CLI configuration override {override_value:?}: {reason}")]
    CliOverride {
        /// Original `key=value` input.
        override_value: String,
        /// Expected form.
        reason: String,
    },
    /// Effective values violate a schema invariant.
    #[error("invalid effective configuration: {0}")]
    Validation(String),
    /// Folder-trust inventory or ledger validation failed closed.
    #[error("could not assess project folder trust: {0}")]
    FolderTrust(#[from] FolderTrustError),
    /// Project configuration bytes no longer match the assessed executable inventory.
    #[error("project configuration changed after folder-trust assessment: {0}")]
    ProjectChangedDuringLoad(PathBuf),
}

/// Loader with injectable paths, environment, and CLI state for deterministic tests.
#[derive(Debug, Clone)]
pub struct ConfigLoader {
    user_path: PathBuf,
    project_path: PathBuf,
    project_root: PathBuf,
    trust_store_path: PathBuf,
    project_trust_override: Option<bool>,
    warn_on_dangerous_override: bool,
    environment: BTreeMap<String, String>,
    cli_overrides: Vec<String>,
}

impl ConfigLoader {
    /// Discovers paths and captures environment variables from the running process.
    ///
    /// `ROTTWEILER_HOME` wins, followed by an explicitly configured
    /// `XDG_CONFIG_HOME`, with `~/.rottweiler` as the documented fallback.
    ///
    /// # Errors
    ///
    /// Returns an error when the process working directory cannot be read.
    pub fn from_environment() -> Result<Self, ConfigError> {
        let environment = env::vars().collect::<BTreeMap<_, _>>();
        let project_root = env::current_dir().map_err(ConfigError::CurrentDirectory)?;
        Self::from_captured_environment(environment, &project_root)
    }

    fn from_captured_environment(
        environment: BTreeMap<String, String>,
        project_root: &Path,
    ) -> Result<Self, ConfigError> {
        let (root_name, user_root) =
            if let Some(root) = nonempty_value(&environment, "ROTTWEILER_HOME") {
                ("ROTTWEILER_HOME", PathBuf::from(root))
            } else if let Some(root) = nonempty_value(&environment, "XDG_CONFIG_HOME") {
                ("XDG_CONFIG_HOME", PathBuf::from(root).join("rottweiler"))
            } else {
                let home = nonempty_value(&environment, "HOME")
                    .ok_or(ConfigError::MissingUserConfigRoot)?;
                ("HOME", PathBuf::from(home).join(".rottweiler"))
            };
        if !user_root.is_absolute() {
            return Err(ConfigError::InvalidUserConfigRoot {
                name: root_name.to_owned(),
                value: user_root.display().to_string(),
            });
        }

        Ok(Self {
            user_path: user_root.join("config.toml"),
            project_path: project_root.join(".rottweiler/config.toml"),
            project_root: project_root.to_owned(),
            trust_store_path: user_root.join("trust.json"),
            project_trust_override: None,
            warn_on_dangerous_override: false,
            environment,
            cli_overrides: Vec::new(),
        })
    }

    /// Creates an isolated loader for tests and embedded SDK callers.
    #[must_use]
    pub fn new(user_path: PathBuf, project_path: PathBuf) -> Self {
        let project_parent = project_path.parent().unwrap_or_else(|| Path::new("."));
        let project_root = if project_parent
            .file_name()
            .is_some_and(|name| name == ".rottweiler")
        {
            project_parent.parent().unwrap_or(project_parent)
        } else {
            project_parent
        }
        .to_path_buf();
        let trust_store_path = user_path.with_file_name("trust.json");
        Self {
            user_path,
            project_path,
            project_root,
            trust_store_path,
            project_trust_override: None,
            warn_on_dangerous_override: false,
            environment: BTreeMap::new(),
            cli_overrides: Vec::new(),
        }
    }

    /// Replaces the captured environment layer.
    #[must_use]
    pub fn with_environment(mut self, environment: BTreeMap<String, String>) -> Self {
        self.environment = environment;
        self
    }

    /// Adds highest-precedence `key=value` CLI overrides.
    #[must_use]
    pub fn with_cli_overrides(mut self, overrides: Vec<String>) -> Self {
        self.cli_overrides = overrides;
        self
    }

    /// Override the persisted folder-trust decision. `true` is the explicit
    /// `--dangerously-trust` CI escape hatch and does not mutate the ledger.
    #[must_use]
    pub fn with_project_trust(mut self, trusted: bool) -> Self {
        self.project_trust_override = Some(trusted);
        self
    }

    /// Enable project executable configuration for this process without
    /// persisting trust. Intended only for explicit CI images.
    #[must_use]
    pub fn dangerously_trust_project(mut self) -> Self {
        self.project_trust_override = Some(true);
        self.warn_on_dangerous_override = true;
        self
    }

    /// Canonicalization input used for project trust assessment.
    #[must_use]
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// User-private trust ledger adjacent to the user configuration.
    #[must_use]
    pub fn trust_store_path(&self) -> &Path {
        &self.trust_store_path
    }

    /// User-scoped credential fallback adjacent to the effective user config.
    #[must_use]
    pub fn credentials_path(&self) -> PathBuf {
        self.user_path.with_file_name("credentials.toml")
    }

    /// Persists one allowlisted user-scoped setting through an atomic private rewrite.
    ///
    /// The project file is never a write target, including for security-sensitive
    /// permission defaults. The complete resulting configuration is reloaded and
    /// validated before success is returned.
    ///
    /// # Errors
    ///
    /// Returns an error for non-allowlisted values, unsafe paths, malformed
    /// existing TOML, failed atomic persistence, or an invalid merged result.
    pub fn persist_tui_setting(&self, key: &str, value: &str) -> Result<LoadedConfig, ConfigError> {
        let effective = self.load()?;
        validate_tui_setting(&effective.config, key, value)?;
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: key.to_owned(),
                reason: "user configuration has no parent directory".to_owned(),
            })?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let _settings_lock = acquire_tui_settings_lock(parent, key)?;
        validate_tui_config_file(&self.user_path, key)?;
        let mut document = read_tui_config_document(&self.user_path)?;
        if let Some(alias) = key.strip_prefix("models.thinking.") {
            let root = document
                .as_table_mut()
                .ok_or_else(|| ConfigError::InvalidUserSetting {
                    key: key.to_owned(),
                    reason: "user configuration root is not a table".to_owned(),
                })?;
            let models = root
                .entry("models".to_owned())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
                .as_table_mut()
                .ok_or_else(|| ConfigError::InvalidUserSetting {
                    key: key.to_owned(),
                    reason: "models configuration is not a table".to_owned(),
                })?;
            let thinking = models
                .entry("thinking".to_owned())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
                .as_table_mut()
                .ok_or_else(|| ConfigError::InvalidUserSetting {
                    key: key.to_owned(),
                    reason: "model thinking configuration is not a table".to_owned(),
                })?;
            thinking.insert(alias.to_owned(), toml::Value::String(value.to_owned()));
        } else {
            set_toml_leaf(&mut document, key, value)?;
        }
        let encoded = format!(
            "# Rottweiler user settings; last updated via TUI\n{}",
            toml::to_string_pretty(&document).map_err(|error| ConfigError::InvalidUserSetting {
                key: key.to_owned(),
                reason: error.to_string(),
            })?
        );
        // Record provenance first. If the config rewrite fails, the stale entry
        // cannot claim a source because loading requires its value to match.
        // The inverse order could report failure after changing the setting.
        persist_tui_provenance(parent, &self.user_path, key, value)?;
        persist_tui_config_atomic(parent, &self.user_path, encoded.as_bytes(), key)?;
        self.load()
    }

    /// Adds one built-in provider profile using only its fixed adapter kind.
    /// Existing profiles are never overwritten and no endpoint, client id, or
    /// credential value can enter through this path.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown providers, conflicting existing profiles,
    /// unsafe paths, or failed atomic persistence.
    pub fn configure_builtin_provider(&self, provider: &str) -> Result<LoadedConfig, ConfigError> {
        let kind = match provider {
            "openai_codex" => "openai_codex",
            "github_copilot" => "github_copilot",
            "openai" => "openai",
            "anthropic" => "anthropic",
            _ => {
                return Err(ConfigError::InvalidUserSetting {
                    key: format!("providers.{provider}"),
                    reason: "provider is not in the fixed built-in setup allowlist".to_owned(),
                });
            }
        };
        let effective = self.load()?;
        if let Some(existing) = effective.config.providers.get(provider) {
            return if existing.kind == kind {
                Ok(effective)
            } else {
                Err(ConfigError::InvalidUserSetting {
                    key: format!("providers.{provider}"),
                    reason: "an existing provider profile uses a different adapter kind".to_owned(),
                })
            };
        }
        let key = format!("providers.{provider}.kind");
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: key.clone(),
                reason: "user configuration has no parent directory".to_owned(),
            })?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let _settings_lock = acquire_tui_settings_lock(parent, &key)?;
        validate_tui_config_file(&self.user_path, &key)?;
        let mut document = read_tui_config_document(&self.user_path)?;
        if provider == "openai" {
            migrate_legacy_openai_subscription_document(&mut document);
        }
        set_toml_leaf(&mut document, &key, kind)?;
        let encoded = format!(
            "# Rottweiler user settings; last updated via TUI\n{}",
            toml::to_string_pretty(&document).map_err(|error| ConfigError::InvalidUserSetting {
                key: key.clone(),
                reason: error.to_string(),
            })?
        );
        persist_tui_provenance(parent, &self.user_path, &key, kind)?;
        persist_tui_config_atomic(parent, &self.user_path, encoded.as_bytes(), &key)?;
        self.load()
    }

    /// Persists a concrete model route in a private host preference keyed by
    /// the canonical project identity. The project tree and
    /// executable trust ledger are never modified.
    ///
    /// # Errors
    ///
    /// Returns an error when the route is invalid or the private preference
    /// file is unsafe or unavailable.
    pub fn persist_tui_project_model(&self, model: &str) -> Result<LoadedConfig, ConfigError> {
        if !valid_project_model_selection(model) {
            return Err(ConfigError::InvalidUserSetting {
                key: "project.models.default".to_owned(),
                reason: "project model must be a bounded alias or concrete provider/model route"
                    .to_owned(),
            });
        }
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "project.models.default".to_owned(),
                reason: "user configuration has no parent directory".to_owned(),
            })?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let _settings_lock = acquire_tui_settings_lock(parent, "project.models.default")?;
        let path = project_model_preferences_path(&self.user_path);
        let mut preferences = read_project_model_preferences(&self.user_path)?;
        preferences.insert(project_identity(&self.project_root)?, model.to_owned());
        let bytes = serde_json::to_vec_pretty(&preferences).map_err(|error| {
            ConfigError::InvalidUserSetting {
                key: "project.models.default".to_owned(),
                reason: error.to_string(),
            }
        })?;
        persist_tui_config_atomic(parent, &path, &bytes, "project.models.default")?;
        self.load()
    }

    /// Reads the private concrete model preference for this canonical project.
    ///
    /// # Errors
    ///
    /// Returns an error when the identity or private preference file is unsafe.
    pub fn tui_project_model(&self) -> Result<Option<String>, ConfigError> {
        Ok(read_project_model_preferences(&self.user_path)?
            .get(&project_identity(&self.project_root)?)
            .filter(|model| valid_project_model_selection(model))
            .cloned())
    }

    /// Reads the user keybinding preset managed by the simple TUI settings surface.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe, oversized, malformed, or unreadable configuration.
    pub fn tui_keybinding_preset(&self) -> Result<String, ConfigError> {
        let path = self.user_path.with_file_name("keybindings.toml");
        validate_tui_config_file(&path, "ui.keybindings.preset")?;
        let document = read_bounded_tui_config_document(
            &path,
            "ui.keybindings.preset",
            MAX_TUI_AUX_CONFIG_BYTES,
        )?;
        Ok(document
            .get("preset")
            .and_then(toml::Value::as_str)
            .filter(|preset| matches!(*preset, "standard" | "vim"))
            .unwrap_or("standard")
            .to_owned())
    }

    /// Persists only the simple user keybinding preset while preserving custom bindings.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid preset or unsafe, malformed, or unwritable configuration.
    pub fn persist_tui_keybinding_preset(&self, preset: &str) -> Result<(), ConfigError> {
        if !matches!(preset, "standard" | "vim") {
            return Err(ConfigError::InvalidUserSetting {
                key: "ui.keybindings.preset".to_owned(),
                reason: "keybinding preset must be standard or vim".to_owned(),
            });
        }
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "ui.keybindings.preset".to_owned(),
                reason: "user configuration has no parent directory".to_owned(),
            })?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let _settings_lock = acquire_tui_settings_lock(parent, "ui.keybindings.preset")?;
        let path = self.user_path.with_file_name("keybindings.toml");
        validate_tui_config_file(&path, "ui.keybindings.preset")?;
        let mut document = read_bounded_tui_config_document(
            &path,
            "ui.keybindings.preset",
            MAX_TUI_AUX_CONFIG_BYTES,
        )?;
        set_toml_leaf(&mut document, "preset", preset)?;
        let bytes =
            toml::to_string_pretty(&document).map_err(|error| ConfigError::InvalidUserSetting {
                key: "ui.keybindings.preset".to_owned(),
                reason: error.to_string(),
            })?;
        persist_tui_config_atomic(parent, &path, bytes.as_bytes(), "ui.keybindings.preset")
    }

    /// Lists simple user MCP enablement flags without exposing executable details.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe, oversized, malformed, or unreadable MCP configuration.
    pub fn tui_mcp_servers(&self) -> Result<Vec<(String, bool)>, ConfigError> {
        let path = self.user_path.with_file_name("mcp.toml");
        validate_tui_config_file(&path, "mcp.servers")?;
        let document =
            read_bounded_tui_config_document(&path, "mcp.servers", MAX_TUI_AUX_CONFIG_BYTES)?;
        let mut servers = document
            .get("servers")
            .and_then(toml::Value::as_table)
            .into_iter()
            .flat_map(|servers| servers.iter())
            .filter(|(name, value)| valid_mcp_server_name(name) && value.is_table())
            .map(|(name, value)| {
                (
                    name.clone(),
                    value
                        .get("enabled")
                        .and_then(toml::Value::as_bool)
                        .unwrap_or(true),
                )
            })
            .collect::<Vec<_>>();
        servers.sort_by(|left, right| left.0.cmp(&right.0));
        servers.truncate(128);
        Ok(servers)
    }

    /// Persists one existing user MCP server's enablement flag.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown server or unsafe, malformed, or unwritable configuration.
    pub fn persist_tui_mcp_enabled(&self, server: &str, enabled: bool) -> Result<(), ConfigError> {
        if !valid_mcp_server_name(server) {
            return Err(ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "MCP server name is invalid".to_owned(),
            });
        }
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "user configuration has no parent directory".to_owned(),
            })?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let _settings_lock = acquire_tui_settings_lock(parent, "mcp.servers")?;
        let path = self.user_path.with_file_name("mcp.toml");
        validate_tui_config_file(&path, "mcp.servers")?;
        let mut document =
            read_bounded_tui_config_document(&path, "mcp.servers", MAX_TUI_AUX_CONFIG_BYTES)?;
        if !document
            .get("servers")
            .and_then(toml::Value::as_table)
            .is_some_and(|servers| servers.contains_key(server))
        {
            return Err(ConfigError::InvalidUserSetting {
                key: format!("mcp.servers.{server}.enabled"),
                reason: "MCP server is not present in the user configuration".to_owned(),
            });
        }
        let server_table = document
            .get_mut("servers")
            .and_then(toml::Value::as_table_mut)
            .and_then(|servers| servers.get_mut(server))
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: format!("mcp.servers.{server}.enabled"),
                reason: "MCP server is not a table".to_owned(),
            })?;
        server_table.insert("enabled".to_owned(), toml::Value::Boolean(enabled));
        let bytes =
            toml::to_string_pretty(&document).map_err(|error| ConfigError::InvalidUserSetting {
                key: format!("mcp.servers.{server}.enabled"),
                reason: error.to_string(),
            })?;
        persist_tui_config_atomic(parent, &path, bytes.as_bytes(), "mcp.servers")
    }

    /// Adds a user-scoped remote HTTPS MCP server in disabled, deferred mode.
    /// Enabling remains a separate explicit action after fingerprint approval.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe name/URL, an existing server, or an
    /// unsafe, malformed, oversized, or unwritable user MCP file.
    pub fn persist_tui_mcp_http_server(
        &self,
        server: &str,
        endpoint: &str,
    ) -> Result<(), ConfigError> {
        let key = format!("mcp.servers.{server}");
        if !valid_mcp_server_name(server) || endpoint.len() > 2_048 {
            return Err(ConfigError::InvalidUserSetting {
                key,
                reason: "MCP server name or endpoint is invalid".to_owned(),
            });
        }
        let parsed = Url::parse(endpoint).map_err(|_| ConfigError::InvalidUserSetting {
            key: key.clone(),
            reason: "MCP endpoint must be an absolute HTTPS URL".to_owned(),
        })?;
        if parsed.scheme() != "https"
            || parsed.host().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(ConfigError::InvalidUserSetting {
                key,
                reason: "MCP endpoint must be HTTPS without credentials, query, or fragment"
                    .to_owned(),
            });
        }
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "user configuration has no parent directory".to_owned(),
            })?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let _settings_lock = acquire_tui_settings_lock(parent, "mcp.servers")?;
        let path = self.user_path.with_file_name("mcp.toml");
        validate_tui_config_file(&path, "mcp.servers")?;
        let mut document =
            read_bounded_tui_config_document(&path, "mcp.servers", MAX_TUI_AUX_CONFIG_BYTES)?;
        if document
            .get("servers")
            .and_then(toml::Value::as_table)
            .is_some_and(|servers| servers.contains_key(server))
        {
            return Err(ConfigError::InvalidUserSetting {
                key,
                reason: "MCP server already exists".to_owned(),
            });
        }
        let servers = document
            .as_table_mut()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "MCP configuration root is not a table".to_owned(),
            })?
            .entry("servers".to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
            .as_table_mut()
            .ok_or_else(|| ConfigError::InvalidUserSetting {
                key: "mcp.servers".to_owned(),
                reason: "MCP servers value is not a table".to_owned(),
            })?;
        let mut server_table = toml::map::Map::new();
        server_table.insert(
            "endpoint".to_owned(),
            toml::Value::String(endpoint.to_owned()),
        );
        server_table.insert("enabled".to_owned(), toml::Value::Boolean(false));
        server_table.insert("defer_tools".to_owned(), toml::Value::Boolean(true));
        servers.insert(server.to_owned(), toml::Value::Table(server_table));
        let bytes =
            toml::to_string_pretty(&document).map_err(|error| ConfigError::InvalidUserSetting {
                key: format!("mcp.servers.{server}"),
                reason: error.to_string(),
            })?;
        persist_tui_config_atomic(parent, &path, bytes.as_bytes(), "mcp.servers")
    }

    /// Loads, deep-merges, validates, and annotates all configured layers.
    ///
    /// # Errors
    ///
    /// Returns an error when a file cannot be read, TOML fails the schema,
    /// an override is malformed, or the effective values are invalid.
    pub fn load(&self) -> Result<LoadedConfig, ConfigError> {
        if paths_collide(&self.user_path, &self.project_path) {
            return Err(ConfigError::ScopeCollision(self.user_path.clone()));
        }
        tracing::debug!(
            user_config = %self.user_path.display(),
            project_config = %self.project_path.display(),
            "loading configuration"
        );
        let mut loaded = defaults_with_provenance();
        if let Some(file) = read_file(&self.user_path)? {
            let source = ConfigSource::UserFile(self.user_path.clone());
            apply_file(&mut loaded, file, &source, FileScope::User);
            for (key, value) in read_tui_provenance(&self.user_path)? {
                if configured_setting_value(&loaded.config, &key).as_deref() == Some(&value)
                    && matches!(loaded.provenance(&key), Some(ConfigSource::UserFile(_)))
                {
                    set_source(
                        &mut loaded,
                        &key,
                        &ConfigSource::UserTui(self.user_path.clone()),
                    );
                }
            }
        }
        let assessment =
            FolderTrustStore::new(self.trust_store_path.clone()).assess(&self.project_root)?;
        let project_trusted = self
            .project_trust_override
            .unwrap_or_else(|| matches!(assessment.state(), FolderTrustState::Trusted));
        loaded.project_trusted = project_trusted;
        if let Some(file) = read_assessed_project_file(&self.project_path, &assessment)? {
            let source = ConfigSource::ProjectFile(self.project_path.clone());
            if project_trusted {
                if self.warn_on_dangerous_override && !assessment.project_execution_enabled() {
                    loaded.warnings.push(ConfigWarning {
                        message: format!(
                            "dangerously trusting executable project configuration from {source} without persisting a folder-trust decision"
                        ),
                    });
                }
                apply_file(&mut loaded, file, &source, FileScope::Project);
            } else {
                warn_ignored_project_sections(&mut loaded, &file, &source);
                loaded.warnings.push(ConfigWarning {
                    message: format!(
                        "ignored untrusted project configuration from {source}; review the executable inventory with `rw trust status` before granting trust"
                    ),
                });
            }
        }
        apply_environment(&mut loaded, &self.environment)?;
        for cli_override in &self.cli_overrides {
            apply_override(&mut loaded, cli_override, &ConfigSource::Cli)?;
        }
        migrate_legacy_openai_subscription(&mut loaded);
        validate(&loaded.config)?;
        Ok(loaded)
    }
}

fn migrate_legacy_openai_subscription_document(document: &mut toml::Value) {
    let Some(root) = document.as_table_mut() else {
        return;
    };
    let legacy = {
        let Some(providers) = root
            .get_mut("providers")
            .and_then(toml::Value::as_table_mut)
        else {
            return;
        };
        let legacy_is_subscription = providers
            .get("openai")
            .and_then(toml::Value::as_table)
            .and_then(|provider| provider.get("kind"))
            .and_then(toml::Value::as_str)
            .is_some_and(|kind| matches!(kind, "openai_codex" | "openai_subscription"));
        if !legacy_is_subscription {
            return;
        }
        let canonical_is_compatible = providers
            .get("openai_codex")
            .and_then(toml::Value::as_table)
            .and_then(|provider| provider.get("kind"))
            .and_then(toml::Value::as_str)
            .is_none_or(|kind| matches!(kind, "openai_codex" | "openai_subscription"));
        if !canonical_is_compatible {
            return;
        }
        providers.remove("openai")
    };
    if let Some(legacy) = legacy
        && let Some(providers) = root
            .get_mut("providers")
            .and_then(toml::Value::as_table_mut)
    {
        providers.entry("openai_codex".to_owned()).or_insert(legacy);
    }
    if let Some(aliases) = root
        .get_mut("models")
        .and_then(toml::Value::as_table_mut)
        .and_then(|models| models.get_mut("aliases"))
        .and_then(toml::Value::as_table_mut)
    {
        for candidates in aliases
            .iter_mut()
            .filter_map(|(_, value)| value.as_array_mut())
        {
            for candidate in candidates {
                let Some(value) = candidate.as_str() else {
                    continue;
                };
                if let Some(model) = value.strip_prefix("openai/") {
                    *candidate = toml::Value::String(format!("openai_codex/{model}"));
                }
            }
        }
    }
}

fn migrate_legacy_openai_subscription(loaded: &mut LoadedConfig) {
    let Some(legacy) = loaded.config.providers.get("openai").cloned() else {
        return;
    };
    if !matches!(legacy.kind.as_str(), "openai_codex" | "openai_subscription") {
        return;
    }
    if loaded
        .config
        .providers
        .get("openai_codex")
        .is_some_and(|provider| {
            !matches!(
                provider.kind.as_str(),
                "openai_codex" | "openai_subscription"
            )
        })
    {
        loaded.warnings.push(ConfigWarning {
            message: "legacy ChatGPT profile [providers.openai] could not migrate because [providers.openai_codex] is already used by a different adapter".to_owned(),
        });
        return;
    }

    loaded.config.providers.remove("openai");
    loaded
        .config
        .providers
        .entry("openai_codex".to_owned())
        .or_insert(legacy);
    let legacy_sources = loaded
        .provenance
        .iter()
        .filter_map(|(key, source)| {
            key.strip_prefix("providers.openai.").map(|field| {
                (
                    key.clone(),
                    format!("providers.openai_codex.{field}"),
                    source.clone(),
                )
            })
        })
        .collect::<Vec<_>>();
    for (legacy_key, canonical_key, source) in legacy_sources {
        loaded.provenance.remove(&legacy_key);
        loaded.provenance.entry(canonical_key).or_insert(source);
    }
    for candidates in loaded.config.models.aliases.values_mut() {
        for candidate in candidates {
            if let Some(model) = candidate.strip_prefix("openai/") {
                *candidate = format!("openai_codex/{model}");
            }
        }
    }
    loaded.warnings.push(ConfigWarning {
        message: "migrated legacy ChatGPT profile [providers.openai] to [providers.openai_codex]; OpenAI API remains a separate provider".to_owned(),
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileScope {
    User,
    Project,
}

fn defaults_with_provenance() -> LoadedConfig {
    let provenance = [
        "engine.max_concurrent_sessions",
        "engine.subagent_max_depth",
        "engine.subagent_max_concurrency",
        "models.default",
        "models.aliases",
        "models.thinking",
        "compaction.auto",
        "compaction.reserved",
        "compaction.model_alias",
        "budget.session_cost_cap_micros_usd",
        "budget.daily_cost_cap_micros_usd",
        "budget.session_ai_credit_cap_micros",
        "budget.daily_ai_credit_cap_micros",
        "budget.spend_rate_alarm_micros_usd_per_minute",
        "budget.ai_credit_rate_alarm_micros_per_minute",
        "budget.warn_at_percent",
        "providers",
        "network.proxy",
        "network.proxy_username",
        "network.proxy_password_credential",
        "websearch.endpoint",
        "websearch.query_parameter",
        "websearch.header_credentials",
        "permissions.default",
        "sandbox.safe_list",
        "toolchain.formatter",
        "toolchain.linters",
        "toolchain.test",
        "toolchain.rules",
        "telemetry.enabled",
        "updates.channel",
        "ui.theme",
    ]
    .into_iter()
    .map(|key| (key.to_owned(), ConfigSource::BuiltIn))
    .collect();
    LoadedConfig {
        config: Config::default(),
        provenance,
        warnings: Vec::new(),
        project_trusted: false,
    }
}

fn read_file(path: &Path) -> Result<Option<ConfigFile>, ConfigError> {
    match fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents)
            .map(Some)
            .map_err(|source| ConfigError::Parse {
                path: path.to_owned(),
                source,
            }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

fn read_assessed_project_file(
    path: &Path,
    assessment: &crate::trust::FolderTrustAssessment,
) -> Result<Option<ConfigFile>, ConfigError> {
    let parent = path
        .parent()
        .ok_or_else(|| ConfigError::ProjectChangedDuringLoad(path.to_owned()))?;
    let canonical_parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    let assessed_path = path
        .file_name()
        .map(|name| canonical_parent.join(name))
        .ok_or_else(|| ConfigError::ProjectChangedDuringLoad(path.to_owned()))?;
    let relative = assessed_path
        .strip_prefix(assessment.workspace())
        .map_err(|_| ConfigError::ProjectChangedDuringLoad(path.to_owned()))?
        .components()
        .map(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| ConfigError::ProjectChangedDuringLoad(path.to_owned()))?
        .join("/");
    let inventory_governed = relative.starts_with(".agents/")
        || relative.starts_with(".rottweiler/")
        || matches!(relative.as_str(), ".agents" | ".rottweiler");
    let assessed = assessment
        .inventory()
        .iter()
        .find(|item| item.path == relative);
    let Some(bytes) = read_project_bytes(path)? else {
        return if assessed.is_none() {
            Ok(None)
        } else {
            Err(ConfigError::ProjectChangedDuringLoad(path.to_owned()))
        };
    };
    let bytes_len = u64::try_from(bytes.len())
        .map_err(|_| ConfigError::ProjectChangedDuringLoad(path.to_owned()))?;
    let digest = blake3::hash(&bytes).to_hex().to_string();
    if assessed.is_some_and(|item| item.bytes != bytes_len || item.content_hash != digest)
        || (assessed.is_none() && inventory_governed)
    {
        return Err(ConfigError::ProjectChangedDuringLoad(path.to_owned()));
    }
    let contents = std::str::from_utf8(&bytes).map_err(|_| ConfigError::Read {
        path: path.to_owned(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "project configuration is not valid UTF-8",
        ),
    })?;
    toml::from_str(contents)
        .map(Some)
        .map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })
}

fn read_project_bytes(path: &Path) -> Result<Option<Vec<u8>>, ConfigError> {
    const MAX_PROJECT_CONFIG_BYTES: u64 = 8 * 1024 * 1024;
    #[cfg(unix)]
    let file = {
        let parent_path = path
            .parent()
            .ok_or_else(|| ConfigError::ProjectChangedDuringLoad(path.to_owned()))?;
        let file_name = path
            .file_name()
            .ok_or_else(|| ConfigError::ProjectChangedDuringLoad(path.to_owned()))?;
        let parent = match rustix::fs::open(
            parent_path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(parent) => parent,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source: std::io::Error::from(source),
                });
            }
        };
        let descriptor = match rustix::fs::openat(
            &parent,
            file_name,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(descriptor) => descriptor,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source: std::io::Error::from(source),
                });
            }
        };
        let stat = rustix::fs::fstat(&descriptor)
            .map_err(std::io::Error::from)
            .map_err(|source| ConfigError::Read {
                path: path.to_owned(),
                source,
            })?;
        if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
            return Err(ConfigError::ProjectChangedDuringLoad(path.to_owned()));
        }
        std::fs::File::from(descriptor)
    };
    #[cfg(not(unix))]
    let file = match std::fs::OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    let mut bytes = Vec::new();
    file.take(MAX_PROJECT_CONFIG_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PROJECT_CONFIG_BYTES {
        return Err(ConfigError::ProjectChangedDuringLoad(path.to_owned()));
    }
    Ok(Some(bytes))
}

fn apply_file(
    loaded: &mut LoadedConfig,
    mut file: ConfigFile,
    source: &ConfigSource,
    scope: FileScope,
) {
    if let Some(engine) = file.engine.take() {
        apply_engine_file(loaded, &engine, source);
    }
    if let Some(models) = file.models.take() {
        if let Some(value) = models.default {
            loaded.config.models.default = value;
            set_source(loaded, "models.default", source);
        }
        if let Some(aliases) = models.aliases {
            for (alias, candidates) in aliases {
                let key = format!("models.aliases.{alias}");
                loaded.config.models.aliases.insert(alias, candidates);
                set_source(loaded, &key, source);
            }
        }
        if let Some(thinking) = models.thinking {
            for (alias, level) in thinking {
                let key = format!("models.thinking.{alias}");
                loaded.config.models.thinking.insert(alias, level);
                set_source(loaded, &key, source);
            }
        }
    }
    if let Some(compaction) = file.compaction.take() {
        if let Some(value) = compaction.auto {
            loaded.config.compaction.auto = value;
            set_source(loaded, "compaction.auto", source);
        }
        if let Some(value) = compaction.reserved_tokens {
            loaded.config.compaction.reserved_tokens = Some(value);
            set_source(loaded, "compaction.reserved", source);
        }
        if let Some(value) = compaction.model_alias {
            loaded.config.compaction.model_alias = Some(value);
            set_source(loaded, "compaction.model_alias", source);
        }
    }
    if let Some(budget) = file.budget.take() {
        macro_rules! apply_budget {
            ($field:ident) => {
                if let Some(value) = budget.$field {
                    loaded.config.budget.$field = Some(value);
                    set_source(loaded, concat!("budget.", stringify!($field)), source);
                }
            };
        }
        apply_budget!(session_cost_cap_micros_usd);
        apply_budget!(daily_cost_cap_micros_usd);
        apply_budget!(session_ai_credit_cap_micros);
        apply_budget!(daily_ai_credit_cap_micros);
        apply_budget!(spend_rate_alarm_micros_usd_per_minute);
        apply_budget!(ai_credit_rate_alarm_micros_per_minute);
        if let Some(value) = budget.warn_at_percent {
            loaded.config.budget.warn_at_percent = value;
            set_source(loaded, "budget.warn_at_percent", source);
        }
    }
    if let Some(toolchain) = file.toolchain.take() {
        loaded.config.toolchain = toolchain;
        for key in [
            "toolchain.formatter",
            "toolchain.linters",
            "toolchain.test",
            "toolchain.rules",
        ] {
            set_source(loaded, key, source);
        }
    }
    if let Some(ui) = file.ui.take() {
        loaded.config.ui = ui;
        set_source(loaded, "ui.theme", source);
    }
    if scope == FileScope::User {
        if let Some(telemetry) = file.telemetry.take()
            && let Some(value) = telemetry.enabled
        {
            loaded.config.telemetry.enabled = value;
            set_source(loaded, "telemetry.enabled", source);
        }
        apply_security_file_sections(loaded, file, source);
    } else {
        warn_ignored_project_sections(loaded, &file, source);
    }
}

fn apply_engine_file(loaded: &mut LoadedConfig, engine: &EngineConfigFile, source: &ConfigSource) {
    for (key, value) in [
        (
            "engine.max_concurrent_sessions",
            engine.max_concurrent_sessions,
        ),
        ("engine.subagent_max_depth", engine.subagent_max_depth),
        (
            "engine.subagent_max_concurrency",
            engine.subagent_max_concurrency,
        ),
    ] {
        if let Some(value) = value {
            match key {
                "engine.max_concurrent_sessions" => {
                    loaded.config.engine.max_concurrent_sessions = value;
                }
                "engine.subagent_max_depth" => loaded.config.engine.subagent_max_depth = value,
                _ => loaded.config.engine.subagent_max_concurrency = value,
            }
            set_source(loaded, key, source);
        }
    }
}

fn apply_security_file_sections(
    loaded: &mut LoadedConfig,
    file: ConfigFile,
    source: &ConfigSource,
) {
    if let Some(websearch) = file.websearch {
        if let Some(value) = websearch.endpoint {
            loaded.config.websearch.endpoint = Some(value);
            set_source(loaded, "websearch.endpoint", source);
        }
        if let Some(value) = websearch.query_parameter {
            loaded.config.websearch.query_parameter = value;
            set_source(loaded, "websearch.query_parameter", source);
        }
        if let Some(value) = websearch.header_credentials {
            loaded.config.websearch.header_credentials = value;
            set_source(loaded, "websearch.header_credentials", source);
        }
    }
    if let Some(network) = file.network {
        if let Some(value) = network.proxy {
            loaded.config.network.proxy = Some(value);
            set_source(loaded, "network.proxy", source);
        }
        if let Some(value) = network.proxy_username {
            loaded.config.network.proxy_username = Some(value);
            set_source(loaded, "network.proxy_username", source);
        }
        if let Some(value) = network.proxy_password_credential {
            loaded.config.network.proxy_password_credential = Some(value);
            set_source(loaded, "network.proxy_password_credential", source);
        }
    }
    if let Some(providers) = file.providers {
        for (name, provider) in providers {
            set_provider_sources(loaded, &name, &provider, source);
            loaded.config.providers.insert(name, provider);
        }
    }
    if let Some(permissions) = file.permissions {
        if let Some(value) = permissions.default {
            loaded.config.permissions.default = value;
            set_source(loaded, "permissions.default", source);
        }
        if let Some(value) = permissions.rules {
            loaded.config.permissions.rules = value;
            set_source(loaded, "permissions.rules", source);
        }
    }
    if let Some(sandbox) = file.sandbox
        && let Some(value) = sandbox.safe_list
    {
        loaded.config.sandbox.safe_list = value;
        set_source(loaded, "sandbox.safe_list", source);
    }
    if let Some(updates) = file.updates
        && let Some(value) = updates.channel
    {
        loaded.config.updates.channel = value;
        set_source(loaded, "updates.channel", source);
    }
}

fn warn_ignored_project_sections(
    loaded: &mut LoadedConfig,
    file: &ConfigFile,
    source: &ConfigSource,
) {
    for (present, section) in [
        (file.network.is_some(), "network"),
        (file.websearch.is_some(), "websearch"),
        (file.providers.is_some(), "providers"),
        (file.permissions.is_some(), "permissions"),
        (file.sandbox.is_some(), "sandbox.safe_list"),
        (file.telemetry.is_some(), "telemetry"),
        (file.updates.is_some(), "updates"),
    ] {
        if present {
            loaded.warnings.push(ConfigWarning {
                message: format!(
                    "ignored security-sensitive project section [{section}] from {source}; configure it at user, environment, or CLI scope"
                ),
            });
        }
    }
}

fn apply_environment(
    loaded: &mut LoadedConfig,
    environment: &BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    for (name, key) in [
        (ENV_ENGINE_SESSIONS, "engine.max_concurrent_sessions"),
        (ENV_SUBAGENT_DEPTH, "engine.subagent_max_depth"),
        (ENV_SUBAGENT_CONCURRENCY, "engine.subagent_max_concurrency"),
        (ENV_MODEL_DEFAULT, "models.default"),
        (ENV_COMPACTION_AUTO, "compaction.auto"),
        // Retain the pre-M3 spelling as a compatibility alias. The canonical
        // environment variable is applied last so it wins when both are set.
        (ENV_COMPACTION_RESERVED_TOKENS, "compaction.reserved"),
        (ENV_COMPACTION_RESERVED, "compaction.reserved"),
        (ENV_COMPACTION_MODEL_ALIAS, "compaction.model_alias"),
        (
            ENV_BUDGET_SESSION_COST_CAP,
            "budget.session_cost_cap_micros_usd",
        ),
        (
            ENV_BUDGET_DAILY_COST_CAP,
            "budget.daily_cost_cap_micros_usd",
        ),
        (
            ENV_BUDGET_SESSION_CREDIT_CAP,
            "budget.session_ai_credit_cap_micros",
        ),
        (
            ENV_BUDGET_DAILY_CREDIT_CAP,
            "budget.daily_ai_credit_cap_micros",
        ),
        (
            ENV_BUDGET_SPEND_RATE,
            "budget.spend_rate_alarm_micros_usd_per_minute",
        ),
        (
            ENV_BUDGET_CREDIT_RATE,
            "budget.ai_credit_rate_alarm_micros_per_minute",
        ),
        (ENV_BUDGET_WARN_PERCENT, "budget.warn_at_percent"),
        (ENV_NETWORK_PROXY, "network.proxy"),
        (ENV_NETWORK_PROXY_USERNAME, "network.proxy_username"),
        (
            ENV_NETWORK_PROXY_PASSWORD_CREDENTIAL,
            "network.proxy_password_credential",
        ),
        (ENV_PERMISSION_DEFAULT, "permissions.default"),
        (ENV_SANDBOX_SAFE_LIST, "sandbox.safe_list"),
        (ENV_TELEMETRY_ENABLED, "telemetry.enabled"),
        (ENV_UPDATE_CHANNEL, "updates.channel"),
    ] {
        if let Some(value) = environment.get(name) {
            apply_override(
                loaded,
                &format!("{key}={value}"),
                &ConfigSource::Environment(name.to_owned()),
            )
            .map_err(|error| ConfigError::Environment {
                name: name.to_owned(),
                value: value.clone(),
                reason: override_reason(error),
            })?;
        }
    }
    Ok(())
}

fn apply_override(
    loaded: &mut LoadedConfig,
    raw: &str,
    source: &ConfigSource,
) -> Result<(), ConfigError> {
    let Some((key, value)) = raw.split_once('=') else {
        return Err(ConfigError::CliOverride {
            override_value: raw.to_owned(),
            reason: "expected KEY=VALUE".to_owned(),
        });
    };
    // Accept the earlier draft spelling without exposing it in provenance or
    // `config check`; M3's public key is `compaction.reserved`.
    let key = if key == "compaction.reserved_tokens" {
        "compaction.reserved"
    } else {
        key
    };
    if apply_engine_override(loaded, key, value, raw)?
        || apply_m3_override(loaded, key, value, raw)?
    {
        set_source(loaded, key, source);
        return Ok(());
    }
    match key {
        "models.default" => value.clone_into(&mut loaded.config.models.default),
        "network.proxy" => loaded.config.network.proxy = Some(value.to_owned()),
        "network.proxy_username" => {
            loaded.config.network.proxy_username = nonempty_override(value);
        }
        "network.proxy_password_credential" => {
            loaded.config.network.proxy_password_credential = nonempty_override(value);
        }
        "permissions.default" => {
            loaded.config.permissions.default =
                parse_permission(value).ok_or_else(|| ConfigError::CliOverride {
                    override_value: raw.to_owned(),
                    reason: "expected ask, allow, or deny".to_owned(),
                })?;
        }
        "sandbox.safe_list" => {
            loaded.config.sandbox.safe_list = split_list(value);
        }
        "telemetry.enabled" => {
            loaded.config.telemetry.enabled =
                value.parse().map_err(|_| ConfigError::CliOverride {
                    override_value: raw.to_owned(),
                    reason: "expected true or false".to_owned(),
                })?;
        }
        "updates.channel" => {
            loaded.config.updates.channel =
                parse_update_channel(value).ok_or_else(|| ConfigError::CliOverride {
                    override_value: raw.to_owned(),
                    reason: "expected stable or beta".to_owned(),
                })?;
        }
        _ if key.starts_with("models.aliases.") => {
            let alias = key.trim_start_matches("models.aliases.");
            if alias.is_empty() {
                return Err(ConfigError::CliOverride {
                    override_value: raw.to_owned(),
                    reason: "model alias name must not be empty".to_owned(),
                });
            }
            loaded
                .config
                .models
                .aliases
                .insert(alias.to_owned(), split_list(value));
        }
        _ if key.starts_with("models.thinking.") => {
            let alias = key.trim_start_matches("models.thinking.");
            if alias.is_empty() {
                return Err(ConfigError::CliOverride {
                    override_value: raw.to_owned(),
                    reason: "model alias name must not be empty".to_owned(),
                });
            }
            let level = parse_thinking_level(value).ok_or_else(|| ConfigError::CliOverride {
                override_value: raw.to_owned(),
                reason: "expected off, low, medium, or high".to_owned(),
            })?;
            loaded
                .config
                .models
                .thinking
                .insert(alias.to_owned(), level);
        }
        _ if key.starts_with("providers.") => {
            apply_provider_override(loaded, key, value, raw)?;
        }
        _ => {
            return Err(ConfigError::CliOverride {
                override_value: raw.to_owned(),
                reason: format!("unknown configuration key {key:?}"),
            });
        }
    }
    set_source(loaded, key, source);
    Ok(())
}

fn apply_engine_override(
    loaded: &mut LoadedConfig,
    key: &str,
    value: &str,
    raw: &str,
) -> Result<bool, ConfigError> {
    let parsed = || {
        value.parse().map_err(|_| ConfigError::CliOverride {
            override_value: raw.to_owned(),
            reason: "expected a positive integer".to_owned(),
        })
    };
    match key {
        "engine.max_concurrent_sessions" => {
            loaded.config.engine.max_concurrent_sessions = parsed()?;
        }
        "engine.subagent_max_depth" => loaded.config.engine.subagent_max_depth = parsed()?,
        "engine.subagent_max_concurrency" => {
            loaded.config.engine.subagent_max_concurrency = parsed()?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn apply_m3_override(
    loaded: &mut LoadedConfig,
    key: &str,
    value: &str,
    raw: &str,
) -> Result<bool, ConfigError> {
    match key {
        "compaction.auto" => {
            loaded.config.compaction.auto =
                value.parse().map_err(|_| ConfigError::CliOverride {
                    override_value: raw.to_owned(),
                    reason: "expected true or false".to_owned(),
                })?;
        }
        "compaction.reserved" => {
            loaded.config.compaction.reserved_tokens = parse_optional_u64(value, raw)?;
        }
        "compaction.model_alias" => {
            loaded.config.compaction.model_alias = optional_string(value);
        }
        "budget.session_cost_cap_micros_usd" => {
            loaded.config.budget.session_cost_cap_micros_usd = parse_optional_u64(value, raw)?;
        }
        "budget.daily_cost_cap_micros_usd" => {
            loaded.config.budget.daily_cost_cap_micros_usd = parse_optional_u64(value, raw)?;
        }
        "budget.session_ai_credit_cap_micros" => {
            loaded.config.budget.session_ai_credit_cap_micros = parse_optional_u64(value, raw)?;
        }
        "budget.daily_ai_credit_cap_micros" => {
            loaded.config.budget.daily_ai_credit_cap_micros = parse_optional_u64(value, raw)?;
        }
        "budget.spend_rate_alarm_micros_usd_per_minute" => {
            loaded.config.budget.spend_rate_alarm_micros_usd_per_minute =
                parse_optional_u64(value, raw)?;
        }
        "budget.ai_credit_rate_alarm_micros_per_minute" => {
            loaded.config.budget.ai_credit_rate_alarm_micros_per_minute =
                parse_optional_u64(value, raw)?;
        }
        "budget.warn_at_percent" => {
            loaded.config.budget.warn_at_percent =
                value.parse().map_err(|_| ConfigError::CliOverride {
                    override_value: raw.to_owned(),
                    reason: "expected an integer from 1 through 100".to_owned(),
                })?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn set_source(loaded: &mut LoadedConfig, key: &str, source: &ConfigSource) {
    loaded.provenance.insert(key.to_owned(), source.clone());
}

fn apply_provider_override(
    loaded: &mut LoadedConfig,
    key: &str,
    value: &str,
    raw: &str,
) -> Result<(), ConfigError> {
    let remainder = key.trim_start_matches("providers.");
    let Some((name, field)) = remainder.rsplit_once('.') else {
        return Err(ConfigError::CliOverride {
            override_value: raw.to_owned(),
            reason: "expected providers.<name>.<field>".to_owned(),
        });
    };
    if name.trim().is_empty() {
        return Err(ConfigError::CliOverride {
            override_value: raw.to_owned(),
            reason: "provider name must not be empty".to_owned(),
        });
    }
    let provider = loaded.config.providers.entry(name.to_owned()).or_default();
    match field {
        "kind" => value.clone_into(&mut provider.kind),
        "base_url" => provider.base_url = nonempty_override(value),
        "proxy" => provider.proxy = nonempty_override(value),
        "proxy_username" => provider.proxy_username = nonempty_override(value),
        "proxy_password_credential" => {
            provider.proxy_password_credential = nonempty_override(value);
        }
        "api_key_env" => provider.api_key_env = nonempty_override(value),
        "api_key_credential" => provider.api_key_credential = nonempty_override(value),
        "oauth_token_env" => provider.oauth_token_env = nonempty_override(value),
        "oauth_authorization_endpoint" => {
            provider.oauth_authorization_endpoint = nonempty_override(value);
        }
        "oauth_token_endpoint" => provider.oauth_token_endpoint = nonempty_override(value),
        "oauth_client_id" => provider.oauth_client_id = nonempty_override(value),
        "oauth_scopes" => provider.oauth_scopes = split_list(value),
        "oauth_access_token_credential" => {
            provider.oauth_access_token_credential = nonempty_override(value);
        }
        "oauth_refresh_token_credential" => {
            provider.oauth_refresh_token_credential = nonempty_override(value);
        }
        _ => {
            return Err(ConfigError::CliOverride {
                override_value: raw.to_owned(),
                reason: format!("unknown provider configuration field {field:?}"),
            });
        }
    }
    Ok(())
}

fn nonempty_override(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_owned())
}

fn optional_string(value: &str) -> Option<String> {
    (!value.trim().is_empty() && !matches!(value.trim(), "none" | "unset"))
        .then(|| value.to_owned())
}

fn parse_optional_u64(value: &str, raw: &str) -> Result<Option<u64>, ConfigError> {
    if value.trim().is_empty() || matches!(value.trim(), "none" | "unset") {
        return Ok(None);
    }
    value
        .parse()
        .map(Some)
        .map_err(|_| ConfigError::CliOverride {
            override_value: raw.to_owned(),
            reason: "expected a non-negative integer or unset".to_owned(),
        })
}

fn set_provider_sources(
    loaded: &mut LoadedConfig,
    name: &str,
    provider: &ProviderConfig,
    source: &ConfigSource,
) {
    set_source(loaded, &format!("providers.{name}.kind"), source);
    for (present, field) in [
        (provider.base_url.is_some(), "base_url"),
        (provider.proxy.is_some(), "proxy"),
        (provider.proxy_username.is_some(), "proxy_username"),
        (
            provider.proxy_password_credential.is_some(),
            "proxy_password_credential",
        ),
        (provider.api_key_env.is_some(), "api_key_env"),
        (provider.api_key_credential.is_some(), "api_key_credential"),
        (provider.oauth_token_env.is_some(), "oauth_token_env"),
        (
            provider.oauth_authorization_endpoint.is_some(),
            "oauth_authorization_endpoint",
        ),
        (
            provider.oauth_token_endpoint.is_some(),
            "oauth_token_endpoint",
        ),
        (provider.oauth_client_id.is_some(), "oauth_client_id"),
        (!provider.oauth_scopes.is_empty(), "oauth_scopes"),
        (
            provider.oauth_access_token_credential.is_some(),
            "oauth_access_token_credential",
        ),
        (
            provider.oauth_refresh_token_credential.is_some(),
            "oauth_refresh_token_credential",
        ),
    ] {
        if present {
            set_source(loaded, &format!("providers.{name}.{field}"), source);
        }
    }
}

fn validate(config: &Config) -> Result<(), ConfigError> {
    if config.engine.max_concurrent_sessions == 0 {
        return Err(ConfigError::Validation(
            "engine.max_concurrent_sessions must be greater than zero".to_owned(),
        ));
    }
    if config.engine.subagent_max_depth == 0 {
        return Err(ConfigError::Validation(
            "engine.subagent_max_depth must be greater than zero".to_owned(),
        ));
    }
    if config.engine.subagent_max_concurrency == 0 {
        return Err(ConfigError::Validation(
            "engine.subagent_max_concurrency must be greater than zero".to_owned(),
        ));
    }
    if config.models.default.trim().is_empty() {
        return Err(ConfigError::Validation(
            "models.default must not be empty".to_owned(),
        ));
    }
    for (alias, candidates) in &config.models.aliases {
        if alias.trim().is_empty() || candidates.is_empty() {
            return Err(ConfigError::Validation(format!(
                "model alias {alias:?} must have at least one candidate"
            )));
        }
    }
    for alias in config.models.thinking.keys() {
        if alias.trim().is_empty() {
            return Err(ConfigError::Validation(
                "model thinking alias must not be empty".to_owned(),
            ));
        }
    }
    config
        .compaction
        .validate()
        .map_err(|message| ConfigError::Validation(message.to_owned()))?;
    config
        .budget
        .validate()
        .map_err(|message| ConfigError::Validation(message.to_owned()))?;
    validate_ui(config)?;
    validate_websearch(&config.websearch)?;
    for rule in &config.permissions.rules {
        let Some((tool, pattern)) = rule.pattern.split_once('(') else {
            return Err(ConfigError::Validation(format!(
                "permission rule {:?} must use tool(glob) syntax",
                rule.pattern
            )));
        };
        let pattern = pattern.strip_suffix(')').ok_or_else(|| {
            ConfigError::Validation(format!(
                "permission rule {:?} must use tool(glob) syntax",
                rule.pattern
            ))
        })?;
        if tool.is_empty()
            || !tool
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || globset::GlobBuilder::new(pattern)
                .literal_separator(false)
                .backslash_escape(true)
                .build()
                .is_err()
        {
            return Err(ConfigError::Validation(format!(
                "permission rule {:?} contains an invalid tool name or glob",
                rule.pattern
            )));
        }
    }
    for pattern in &config.sandbox.safe_list {
        if pattern.trim().is_empty()
            || globset::GlobBuilder::new(pattern)
                .literal_separator(true)
                .backslash_escape(true)
                .build()
                .is_err()
        {
            return Err(ConfigError::Validation(format!(
                "sandbox.safe_list contains invalid command glob {pattern:?}"
            )));
        }
    }
    validate_toolchain(&config.toolchain)?;
    if let Some(proxy) = &config.network.proxy {
        validate_proxy("network.proxy", proxy)?;
    }
    validate_proxy_authentication(
        "network",
        config.network.proxy.as_deref(),
        config.network.proxy_username.as_deref(),
        config.network.proxy_password_credential.as_deref(),
    )?;
    for (name, provider) in &config.providers {
        validate_provider(name, provider)?;
    }
    Ok(())
}

fn validate_ui(config: &Config) -> Result<(), ConfigError> {
    if matches!(config.ui.theme.as_str(), "kennel-dark" | "daylight") {
        Ok(())
    } else {
        Err(ConfigError::Validation(
            "ui.theme must be kennel-dark or daylight".to_owned(),
        ))
    }
}

fn validate_tui_setting(config: &Config, key: &str, value: &str) -> Result<(), ConfigError> {
    let valid = match key {
        "ui.theme" => matches!(value, "kennel-dark" | "daylight"),
        "compaction.auto" => matches!(value, "true" | "false"),
        "permissions.default" => matches!(value, "ask" | "allow" | "deny"),
        _ if key.starts_with("models.thinking.") => {
            let alias = key.trim_start_matches("models.thinking.");
            !alias.is_empty()
                && alias
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
                && (alias == config.models.default || config.models.aliases.contains_key(alias))
                && matches!(value, "off" | "low" | "medium" | "high")
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "key or value is outside the safe TUI settings allowlist".to_owned(),
        })
    }
}

fn prepare_tui_config_parent(parent: &Path, user_path: &Path) -> Result<(), ConfigError> {
    fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
        path: user_path.to_owned(),
        source,
    })?;
    if fs::symlink_metadata(parent).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ConfigError::InvalidUserSetting {
            key: "ui".to_owned(),
            reason: "user configuration parent must not be a symlink".to_owned(),
        });
    }
    #[cfg(unix)]
    fs::set_permissions(parent, std::os::unix::fs::PermissionsExt::from_mode(0o700)).map_err(
        |source| ConfigError::Write {
            path: parent.to_owned(),
            source,
        },
    )?;
    Ok(())
}

#[cfg(unix)]
fn acquire_tui_settings_lock(
    parent: &Path,
    key: &str,
) -> Result<std::os::fd::OwnedFd, ConfigError> {
    let path = parent.join("config.toml.lock");
    let descriptor = rustix::fs::open(
        &path,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .map_err(|source| ConfigError::Write {
        path: path.clone(),
        source: std::io::Error::from(source),
    })?;
    let stat = rustix::fs::fstat(&descriptor).map_err(|source| ConfigError::Write {
        path: path.clone(),
        source: std::io::Error::from(source),
    })?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_nlink != 1
        || stat.st_mode & 0o077 != 0
    {
        return Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "user settings lock is unsafe".to_owned(),
        });
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
    loop {
        match rustix::fs::flock(
            &descriptor,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        ) {
            Ok(()) => break,
            Err(source) => {
                let source = std::io::Error::from(source);
                if source.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                return Err(ConfigError::Write { path, source });
            }
        }
    }
    Ok(descriptor)
}

#[cfg(not(unix))]
fn acquire_tui_settings_lock(
    _parent: &Path,
    key: &str,
) -> Result<std::sync::MutexGuard<'static, ()>, ConfigError> {
    TUI_SETTING_PORTABLE_LOCK
        .lock()
        .map_err(|_| ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "user settings lock is unavailable".to_owned(),
        })
}

fn validate_tui_config_file(path: &Path, key: &str) -> Result<(), ConfigError> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    #[cfg(unix)]
    let valid = {
        use std::os::unix::fs::MetadataExt as _;
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.nlink() == 1
    };
    #[cfg(not(unix))]
    let valid = metadata.is_file() && !metadata.file_type().is_symlink();
    if valid {
        Ok(())
    } else {
        Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "user configuration must be a single-link regular file".to_owned(),
        })
    }
}

fn read_tui_config_document(path: &Path) -> Result<toml::Value, ConfigError> {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    if source.trim().is_empty() {
        Ok(toml::Value::Table(toml::map::Map::new()))
    } else {
        toml::from_str::<toml::Table>(&source)
            .map(toml::Value::Table)
            .map_err(|source| ConfigError::Parse {
                path: path.to_owned(),
                source,
            })
    }
}

fn read_bounded_tui_config_document(
    path: &Path,
    key: &str,
    maximum: usize,
) -> Result<toml::Value, ConfigError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    if bytes.len() > maximum {
        return Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: format!("configuration exceeds its {maximum}-byte limit"),
        });
    }
    let source = String::from_utf8(bytes).map_err(|_| ConfigError::InvalidUserSetting {
        key: key.to_owned(),
        reason: "configuration is not UTF-8".to_owned(),
    })?;
    if source.trim().is_empty() {
        Ok(toml::Value::Table(toml::map::Map::new()))
    } else {
        toml::from_str::<toml::Table>(&source)
            .map(toml::Value::Table)
            .map_err(|source| ConfigError::Parse {
                path: path.to_owned(),
                source,
            })
    }
}

fn persist_tui_config_atomic(
    parent: &Path,
    path: &Path,
    bytes: &[u8],
    key: &str,
) -> Result<(), ConfigError> {
    let (temporary, mut file) = allocate_tui_config_temporary(parent, key)?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        fs::File::open(parent)?.sync_all()
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&temporary);
        return Err(ConfigError::Write {
            path: path.to_owned(),
            source,
        });
    }
    Ok(())
}

fn allocate_tui_config_temporary(
    parent: &Path,
    key: &str,
) -> Result<(PathBuf, fs::File), ConfigError> {
    for _ in 0..16 {
        let nonce = TUI_SETTING_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".config-{}-{nonce}.tmp", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(ConfigError::Write {
                    path: temporary,
                    source,
                });
            }
        }
    }
    Err(ConfigError::InvalidUserSetting {
        key: key.to_owned(),
        reason: "could not allocate a private temporary file".to_owned(),
    })
}

fn tui_provenance_path(user_path: &Path) -> PathBuf {
    user_path.with_file_name("config-tui-provenance.json")
}

fn project_model_preferences_path(user_path: &Path) -> PathBuf {
    user_path.with_file_name("project-model-preferences.json")
}

fn project_identity(project_root: &Path) -> Result<String, ConfigError> {
    let canonical =
        fs::canonicalize(project_root).map_err(|error| ConfigError::InvalidUserSetting {
            key: "project.models.default".to_owned(),
            reason: format!("project identity is unavailable: {error}"),
        })?;
    Ok(hash_project_identity(&canonical))
}

fn hash_project_identity(canonical: &Path) -> String {
    let mut framed = b"rw-project-identity-v1\0".to_vec();
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt as _;
        canonical.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let rendered = canonical.to_string_lossy();
    #[cfg(not(unix))]
    let bytes = rendered.as_bytes();
    framed.extend_from_slice(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    framed.extend_from_slice(bytes);
    blake3::hash(&framed).to_hex().to_string()
}

fn valid_project_model_selection(model: &str) -> bool {
    if model.is_empty()
        || model.len() > 512
        || model
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return false;
    }
    model.split_once('/').map_or_else(
        || {
            model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        },
        |(provider, model_id)| !provider.is_empty() && !model_id.is_empty(),
    )
}

fn valid_mcp_server_name(server: &str) -> bool {
    !server.is_empty()
        && server.len() <= 96
        && server
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn read_project_model_preferences(
    user_path: &Path,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let path = project_model_preferences_path(user_path);
    validate_tui_provenance_file(&path)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(source) => return Err(ConfigError::Read { path, source }),
    };
    if bytes.len() > 64 * 1024 {
        return Err(ConfigError::InvalidUserSetting {
            key: "project.models.default".to_owned(),
            reason: "project model preferences exceeded their size limit".to_owned(),
        });
    }
    serde_json::from_slice(&bytes).map_err(|error| ConfigError::InvalidUserSetting {
        key: "project.models.default".to_owned(),
        reason: error.to_string(),
    })
}

fn read_tui_provenance(user_path: &Path) -> Result<BTreeMap<String, String>, ConfigError> {
    let path = tui_provenance_path(user_path);
    validate_tui_provenance_file(&path)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(source) => return Err(ConfigError::Read { path, source }),
    };
    if bytes.len() > 64 * 1024 {
        return Err(ConfigError::InvalidUserSetting {
            key: "provenance".to_owned(),
            reason: "TUI provenance exceeded its size limit".to_owned(),
        });
    }
    serde_json::from_slice(&bytes).map_err(|error| ConfigError::InvalidUserSetting {
        key: "provenance".to_owned(),
        reason: error.to_string(),
    })
}

fn validate_tui_provenance_file(path: &Path) -> Result<(), ConfigError> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    #[cfg(unix)]
    let valid = {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.nlink() == 1
            && metadata.permissions().mode().trailing_zeros() >= 6
    };
    #[cfg(not(unix))]
    let valid = metadata.is_file() && !metadata.file_type().is_symlink();
    if valid {
        Ok(())
    } else {
        Err(ConfigError::InvalidUserSetting {
            key: "provenance".to_owned(),
            reason: "TUI provenance must be a private single-link regular file".to_owned(),
        })
    }
}

fn persist_tui_provenance(
    parent: &Path,
    user_path: &Path,
    key: &str,
    value: &str,
) -> Result<(), ConfigError> {
    let mut provenance = read_tui_provenance(user_path)?;
    provenance.insert(key.to_owned(), value.to_owned());
    let bytes = serde_json::to_vec_pretty(&provenance).map_err(|error| {
        ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: error.to_string(),
        }
    })?;
    persist_tui_config_atomic(parent, &tui_provenance_path(user_path), &bytes, key)
}

fn configured_setting_value(config: &Config, key: &str) -> Option<String> {
    match key {
        "ui.theme" => Some(config.ui.theme.clone()),
        "compaction.auto" => Some(config.compaction.auto.to_string()),
        "permissions.default" => Some(permission_name(config.permissions.default).to_owned()),
        _ if key.starts_with("models.thinking.") => config
            .models
            .thinking
            .get(key.trim_start_matches("models.thinking."))
            .map(|level| thinking_level_name(*level).to_owned()),
        _ if key.starts_with("providers.") => {
            let mut segments = key.split('.');
            match (
                segments.next(),
                segments.next(),
                segments.next(),
                segments.next(),
            ) {
                (Some("providers"), Some(provider), Some("kind"), None) => config
                    .providers
                    .get(provider)
                    .map(|entry| entry.kind.clone()),
                _ => None,
            }
        }
        _ => None,
    }
}

fn set_toml_leaf(document: &mut toml::Value, key: &str, value: &str) -> Result<(), ConfigError> {
    let segments = key.split('.').collect::<Vec<_>>();
    let Some((leaf, parents)) = segments.split_last() else {
        return Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "setting key is empty".to_owned(),
        });
    };
    let mut cursor = document;
    for segment in parents {
        let Some(table) = cursor.as_table_mut() else {
            return Err(ConfigError::InvalidUserSetting {
                key: key.to_owned(),
                reason: "setting parent is not a TOML table".to_owned(),
            });
        };
        cursor = table
            .entry((*segment).to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    }
    let boolean_leaf = key == "compaction.auto"
        || (segments.first() == Some(&"servers") && segments.last() == Some(&"enabled"));
    let stored = if boolean_leaf {
        toml::Value::Boolean(value == "true")
    } else {
        toml::Value::String(value.to_owned())
    };
    let Some(table) = cursor.as_table_mut() else {
        return Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "setting parent is not a TOML table".to_owned(),
        });
    };
    table.insert((*leaf).to_owned(), stored);
    Ok(())
}

fn validate_toolchain(config: &rw_types::config::ToolchainConfig) -> Result<(), ConfigError> {
    for (label, command) in [
        ("toolchain.formatter", config.formatter.as_deref()),
        ("toolchain.test", config.test.as_deref()),
    ] {
        if command.is_some_and(invalid_toolchain_command) {
            return Err(ConfigError::Validation(format!(
                "{label} must be a non-empty one-line command"
            )));
        }
    }
    if config
        .linters
        .iter()
        .any(|command| invalid_toolchain_command(command))
    {
        return Err(ConfigError::Validation(
            "toolchain.linters must contain non-empty one-line commands".to_owned(),
        ));
    }
    for rule in &config.rules {
        if rule.pattern.trim().is_empty()
            || globset::GlobBuilder::new(&rule.pattern)
                .literal_separator(true)
                .backslash_escape(true)
                .build()
                .is_err()
        {
            return Err(ConfigError::Validation(format!(
                "toolchain rule contains invalid file glob {:?}",
                rule.pattern
            )));
        }
        if rule
            .formatter
            .as_deref()
            .is_some_and(invalid_toolchain_command)
            || rule.test.as_deref().is_some_and(invalid_toolchain_command)
            || rule
                .linters
                .iter()
                .any(|command| invalid_toolchain_command(command))
        {
            return Err(ConfigError::Validation(format!(
                "toolchain rule {:?} contains an invalid command",
                rule.pattern
            )));
        }
    }
    Ok(())
}

fn validate_websearch(config: &rw_types::config::WebSearchConfig) -> Result<(), ConfigError> {
    if let Some(endpoint) = &config.endpoint {
        let parsed = Url::parse(endpoint).map_err(|_| {
            ConfigError::Validation("websearch.endpoint must be an absolute HTTP(S) URL".to_owned())
        })?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(ConfigError::Validation(
                "websearch.endpoint must not contain credentials, query, or fragment".to_owned(),
            ));
        }
    }
    if config.query_parameter.is_empty()
        || !config
            .query_parameter
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(ConfigError::Validation(
            "websearch.query_parameter is invalid".to_owned(),
        ));
    }
    for (name, credential) in &config.header_credentials {
        let lower = name.to_ascii_lowercase();
        if name.is_empty()
            || !name.is_ascii()
            || !name.bytes().all(is_http_token_byte)
            || matches!(
                lower.as_str(),
                "host" | "connection" | "proxy-authorization"
            )
            || credential.trim().is_empty()
            || credential
                .chars()
                .any(|character| matches!(character, '\0' | '\r' | '\n'))
        {
            return Err(ConfigError::Validation(format!(
                "websearch header {name:?} is invalid or reserved"
            )));
        }
    }
    Ok(())
}

fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn invalid_toolchain_command(command: &str) -> bool {
    command.trim().is_empty()
        || command
            .chars()
            .any(|character| character == '\0' || matches!(character, '\n' | '\r'))
}

fn validate_provider(name: &str, provider: &ProviderConfig) -> Result<(), ConfigError> {
    if name.trim().is_empty() || provider.kind.trim().is_empty() {
        return Err(ConfigError::Validation(format!(
            "provider {name:?} must have a non-empty name and kind"
        )));
    }
    if let Some(base_url) = &provider.base_url {
        let parsed = Url::parse(base_url).map_err(|_| {
            ConfigError::Validation(format!(
                "providers.{name}.base_url must be an absolute HTTP(S) URL"
            ))
        })?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(ConfigError::Validation(format!(
                "providers.{name}.base_url must not contain credentials, query, or fragment"
            )));
        }
        if parsed.scheme() == "http" && !is_loopback_endpoint(&parsed) {
            return Err(ConfigError::Validation(format!(
                "providers.{name}.base_url must use HTTPS unless it targets loopback"
            )));
        }
    }
    if let Some(proxy) = &provider.proxy {
        validate_proxy(&format!("providers.{name}.proxy"), proxy)?;
    }
    validate_proxy_authentication(
        &format!("providers.{name}"),
        provider.proxy.as_deref(),
        provider.proxy_username.as_deref(),
        provider.proxy_password_credential.as_deref(),
    )?;
    for (field, variable) in [
        ("api_key_env", provider.api_key_env.as_deref()),
        ("oauth_token_env", provider.oauth_token_env.as_deref()),
    ] {
        if let Some(variable) = variable
            && !valid_environment_name(variable)
        {
            return Err(ConfigError::Validation(format!(
                "providers.{name}.{field} must name an environment variable"
            )));
        }
    }
    if provider
        .api_key_credential
        .as_deref()
        .is_some_and(|reference| reference.trim().is_empty())
    {
        return Err(ConfigError::Validation(format!(
            "providers.{name}.api_key_credential must not be empty"
        )));
    }
    validate_provider_oauth(name, provider)?;
    Ok(())
}

fn validate_provider_oauth(name: &str, provider: &ProviderConfig) -> Result<(), ConfigError> {
    let authorization_endpoint = provider.oauth_authorization_endpoint.as_deref();
    let token_endpoint = provider.oauth_token_endpoint.as_deref();
    let client_id = provider.oauth_client_id.as_deref();
    let login_configured = authorization_endpoint.is_some()
        || token_endpoint.is_some()
        || client_id.is_some()
        || !provider.oauth_scopes.is_empty();
    if login_configured
        && (authorization_endpoint.is_none() || token_endpoint.is_none() || client_id.is_none())
    {
        return Err(ConfigError::Validation(format!(
            "providers.{name} OAuth login requires oauth_authorization_endpoint, oauth_token_endpoint, and oauth_client_id"
        )));
    }
    for (field, endpoint) in [
        ("oauth_authorization_endpoint", authorization_endpoint),
        ("oauth_token_endpoint", token_endpoint),
    ] {
        if let Some(endpoint) = endpoint {
            validate_remote_endpoint(&format!("providers.{name}.{field}"), endpoint)?;
        }
    }
    if client_id.is_some_and(|client_id| client_id.trim().is_empty()) {
        return Err(ConfigError::Validation(format!(
            "providers.{name}.oauth_client_id must not be empty"
        )));
    }
    if provider
        .oauth_scopes
        .iter()
        .any(|scope| scope.trim().is_empty() || scope.chars().any(char::is_whitespace))
    {
        return Err(ConfigError::Validation(format!(
            "providers.{name}.oauth_scopes entries must be non-empty and contain no whitespace"
        )));
    }
    for (field, reference) in [
        (
            "oauth_access_token_credential",
            provider.oauth_access_token_credential.as_deref(),
        ),
        (
            "oauth_refresh_token_credential",
            provider.oauth_refresh_token_credential.as_deref(),
        ),
    ] {
        if reference.is_some_and(|reference| reference.trim().is_empty()) {
            return Err(ConfigError::Validation(format!(
                "providers.{name}.{field} must not be empty"
            )));
        }
    }
    Ok(())
}

fn validate_remote_endpoint(key: &str, value: &str) -> Result<(), ConfigError> {
    let parsed = Url::parse(value)
        .map_err(|_| ConfigError::Validation(format!("{key} must be an absolute HTTPS URL")))?;
    let loopback_host = match parsed.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(_)) | None => false,
    };
    let loopback_http = parsed.scheme() == "http" && loopback_host;
    if (parsed.scheme() != "https" && !loopback_http)
        || parsed.host().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ConfigError::Validation(format!(
            "{key} must use HTTPS without credentials or a fragment (loopback HTTP is test-only)"
        )));
    }
    Ok(())
}

fn is_loopback_endpoint(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

fn validate_proxy(key: &str, proxy: &str) -> Result<(), ConfigError> {
    let parsed = Url::parse(proxy).map_err(|_| {
        ConfigError::Validation(format!(
            "{key} must be an absolute HTTP(S) URL without inline credentials"
        ))
    })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.path(), "" | "/")
    {
        return Err(ConfigError::Validation(format!(
            "{key} must be an HTTP(S) origin without inline credentials, path, query, or fragment"
        )));
    }
    Ok(())
}

fn validate_proxy_authentication(
    key: &str,
    proxy: Option<&str>,
    username: Option<&str>,
    password_credential: Option<&str>,
) -> Result<(), ConfigError> {
    match (username, password_credential) {
        (None, None) => Ok(()),
        (Some(username), Some(credential))
            if proxy.is_some() && !username.trim().is_empty() && !credential.trim().is_empty() =>
        {
            Ok(())
        }
        _ => Err(ConfigError::Validation(format!(
            "{key} proxy authentication requires proxy, proxy_username, and proxy_password_credential together"
        ))),
    }
}

fn valid_environment_name(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn parse_permission(value: &str) -> Option<PermissionDecision> {
    match value {
        "ask" => Some(PermissionDecision::Ask),
        "allow" => Some(PermissionDecision::Allow),
        "deny" => Some(PermissionDecision::Deny),
        _ => None,
    }
}

fn permission_name(value: PermissionDecision) -> &'static str {
    match value {
        PermissionDecision::Ask => "ask",
        PermissionDecision::Allow => "allow",
        PermissionDecision::Deny => "deny",
    }
}

fn parse_thinking_level(value: &str) -> Option<ThinkingLevel> {
    match value {
        "off" => Some(ThinkingLevel::Off),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        _ => None,
    }
}

fn thinking_level_name(value: ThinkingLevel) -> &'static str {
    match value {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
    }
}

fn parse_update_channel(value: &str) -> Option<UpdateChannel> {
    match value {
        "stable" => Some(UpdateChannel::Stable),
        "beta" => Some(UpdateChannel::Beta),
        _ => None,
    }
}

fn update_channel_name(value: UpdateChannel) -> &'static str {
    match value {
        UpdateChannel::Stable => "stable",
        UpdateChannel::Beta => "beta",
    }
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

fn quoted(value: &str) -> String {
    format!("{value:?}")
}

fn optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "<unset>".to_owned(), |value| value.to_string())
}

fn redacted_proxy(value: &str) -> String {
    Url::parse(value).map_or_else(
        |_| "<invalid proxy>".to_owned(),
        |proxy| quoted(&proxy.origin().ascii_serialization()),
    )
}

fn parent_alias_source<'a>(
    provenance: &'a BTreeMap<String, ConfigSource>,
    key: &str,
) -> Option<&'a ConfigSource> {
    ["models.aliases", "models.thinking", "providers"]
        .into_iter()
        .find_map(|parent| {
            key.starts_with(&format!("{parent}."))
                .then(|| provenance.get(parent))
                .flatten()
        })
}

fn override_reason(error: ConfigError) -> String {
    match error {
        ConfigError::CliOverride { reason, .. } => reason,
        other => other.to_string(),
    }
}

fn paths_collide(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn nonempty_value<'a>(environment: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    environment
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use rw_types::config::{PermissionDecision, UpdateChannel};
    use tempfile::tempdir;

    use super::{ConfigError, ConfigLoader, ConfigSource, read_assessed_project_file};

    #[test]
    fn tui_settings_persist_user_only_with_provenance_and_merge_concurrently() {
        let root = tempdir().expect("root");
        let user = root.path().join("user/config.toml");
        let project = root.path().join("repo/.rottweiler/config.toml");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        fs::write(&project, "[compaction]\nauto = true\n").expect("project config");
        let loader = ConfigLoader::new(user.clone(), project.clone());
        fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
        fs::write(
            &user,
            "[providers.manual]\nkind = \"openai\"\nbase_url = \"https://api.openai.com/v1\"\napi_key_env = \"MANUAL_KEY\"\n",
        )
        .expect("manual user provider");
        fs::write(
            user.parent().expect("user parent").join(".config-old.tmp"),
            "stale",
        )
        .expect("crash temporary");

        let first = loader.clone();
        let second = loader.clone();
        let theme = std::thread::spawn(move || first.persist_tui_setting("ui.theme", "daylight"));
        let compact =
            std::thread::spawn(move || second.persist_tui_setting("compaction.auto", "false"));
        theme.join().expect("theme worker").expect("theme setting");
        compact
            .join()
            .expect("compaction worker")
            .expect("compaction setting");
        loader
            .persist_tui_setting("models.thinking.fast", "high")
            .expect("thinking setting");
        let effective = loader
            .persist_tui_setting("permissions.default", "deny")
            .expect("permission setting");

        assert_eq!(effective.config.ui.theme, "daylight");
        assert!(!effective.config.compaction.auto);
        assert_eq!(
            effective.config.models.thinking["fast"],
            rw_types::config::ThinkingLevel::High
        );
        assert_eq!(
            effective.config.permissions.default,
            PermissionDecision::Deny
        );
        assert!(
            matches!(effective.provenance("permissions.default"), Some(ConfigSource::UserTui(path)) if path == &user)
        );
        assert!(matches!(
            effective.provenance("providers.manual.kind"),
            Some(ConfigSource::UserFile(path)) if path == &user
        ));
        assert!(
            effective
                .render_with_provenance()
                .contains("user (set via TUI)")
        );
        assert_eq!(
            fs::read_to_string(project).expect("project unchanged"),
            "[compaction]\nauto = true\n"
        );
        let persisted = fs::read_to_string(&user).expect("user config");
        assert!(persisted.contains("last updated via TUI"));
        assert!(persisted.contains("theme = \"daylight\""));
        assert!(persisted.contains("default = \"deny\""));
    }

    #[test]
    fn tui_settings_reject_non_allowlisted_security_keys() {
        let root = tempdir().expect("root");
        let loader = ConfigLoader::new(
            root.path().join("user/config.toml"),
            root.path().join("repo/.rottweiler/config.toml"),
        );
        fs::create_dir_all(root.path().join("repo/.rottweiler")).expect("project root");
        let error = loader
            .persist_tui_setting("providers.openai.base_url", "https://attacker.invalid")
            .expect_err("provider mutation must be rejected");
        assert!(
            matches!(error, ConfigError::InvalidUserSetting { .. }),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn tui_builtin_provider_setup_is_fixed_user_scoped_and_idempotent() {
        let root = tempdir().expect("root");
        let user = root.path().join("user/config.toml");
        let project = root.path().join("repo/.rottweiler/config.toml");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        fs::write(&project, "[providers.project]\nkind = \"openai\"\n").expect("project config");
        let loader = ConfigLoader::new(user.clone(), project.clone());

        for provider in ["openai_codex", "github_copilot"] {
            let effective = loader
                .configure_builtin_provider(provider)
                .expect("built-in setup");
            assert_eq!(effective.config.providers[provider].kind, provider);
            assert!(matches!(
                effective.provenance(&format!("providers.{provider}.kind")),
                Some(ConfigSource::UserTui(path)) if path == &user
            ));
            loader
                .configure_builtin_provider(provider)
                .expect("idempotent setup");
        }
        assert_eq!(
            fs::read_to_string(project).expect("project unchanged"),
            "[providers.project]\nkind = \"openai\"\n"
        );
        let persisted = fs::read_to_string(user).expect("user config");
        assert!(persisted.contains("[providers.openai_codex]"));
        assert!(persisted.contains("[providers.github_copilot]"));
    }

    #[test]
    fn legacy_chatgpt_profile_migrates_before_openai_api_setup() {
        let root = tempdir().expect("root");
        let user = root.path().join("user/config.toml");
        let project = root.path().join("repo/.rottweiler/config.toml");
        fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        fs::write(
            &user,
            r#"
[models]
default = "fast"

[models.aliases]
fast = ["openai/gpt-5.4-mini"]

[providers.openai]
kind = "openai_codex"
"#,
        )
        .expect("legacy user config");
        let loader = ConfigLoader::new(user.clone(), project);

        let migrated = loader.load().expect("effective migration");
        assert!(!migrated.config.providers.contains_key("openai"));
        assert_eq!(
            migrated.config.providers["openai_codex"].kind,
            "openai_codex"
        );
        assert!(matches!(
            migrated.provenance("providers.openai_codex.kind"),
            Some(ConfigSource::UserFile(path)) if path == &user
        ));
        assert_eq!(
            migrated.config.models.aliases["fast"],
            vec!["openai_codex/gpt-5.4-mini"]
        );

        loader
            .configure_builtin_provider("openai")
            .expect("separate OpenAI API setup");
        let reloaded = ConfigLoader::new(
            user.clone(),
            root.path().join("repo/.rottweiler/config.toml"),
        )
        .load()
        .expect("restart load");
        assert_eq!(reloaded.config.providers["openai"].kind, "openai");
        assert_eq!(
            reloaded.config.providers["openai_codex"].kind,
            "openai_codex"
        );
        assert_eq!(
            reloaded.config.models.aliases["fast"],
            vec!["openai_codex/gpt-5.4-mini"]
        );
        let persisted = fs::read_to_string(user).expect("persisted user config");
        assert!(persisted.contains("[providers.openai]"));
        assert!(persisted.contains("[providers.openai_codex]"));
        assert!(persisted.contains("openai_codex/gpt-5.4-mini"));
    }

    #[test]
    fn project_model_preference_is_private_concrete_and_independent_of_project_trust() {
        let root = tempdir().expect("root");
        let user = root.path().join("user/config.toml");
        let project = root.path().join("repo/.rottweiler/config.toml");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        fs::write(&project, "[providers.hostile]\nkind = \"openai\"\n")
            .expect("untrusted project config");
        let loader = ConfigLoader::new(user.clone(), project.clone());

        loader
            .persist_tui_project_model("github_copilot/gpt-5-mini")
            .expect("concrete preference");

        assert_eq!(
            loader.tui_project_model().expect("preference").as_deref(),
            Some("github_copilot/gpt-5-mini")
        );
        assert_eq!(
            ConfigLoader::new(user.clone(), project.clone())
                .tui_project_model()
                .expect("restart preference")
                .as_deref(),
            Some("github_copilot/gpt-5-mini")
        );
        assert_eq!(
            fs::read_to_string(project).expect("project unchanged"),
            "[providers.hostile]\nkind = \"openai\"\n"
        );
        loader
            .persist_tui_project_model("fast")
            .expect("alias preference");
        assert_eq!(
            loader.tui_project_model().expect("alias").as_deref(),
            Some("fast")
        );
        assert!(loader.persist_tui_project_model("not valid").is_err());
    }

    #[test]
    fn keybinding_and_mcp_settings_preserve_existing_user_details_and_enforce_caps() {
        let root = tempdir().expect("root");
        let user = root.path().join("user/config.toml");
        let project = root.path().join("repo/.rottweiler/config.toml");
        fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        let keybindings = user.with_file_name("keybindings.toml");
        let mcp = user.with_file_name("mcp.toml");
        fs::write(
            &keybindings,
            "preset='standard'\n[bindings]\nsubmit='enter'\n",
        )
        .expect("keybindings");
        fs::write(
            &mcp,
            "[servers.docs]\nargv=['/usr/bin/docs']\ndefer_tools=true\n",
        )
        .expect("mcp");
        let loader = ConfigLoader::new(user, project);

        loader
            .persist_tui_keybinding_preset("vim")
            .expect("keybinding preset");
        loader
            .persist_tui_mcp_enabled("docs", false)
            .expect("MCP toggle");

        let keybindings_text = fs::read_to_string(&keybindings).expect("keybindings text");
        assert!(keybindings_text.contains("preset = \"vim\""));
        assert!(keybindings_text.contains("submit = \"enter\""));
        let mcp_text = fs::read_to_string(&mcp).expect("MCP text");
        assert!(mcp_text.contains("argv = [\"/usr/bin/docs\"]"));
        assert!(mcp_text.contains("defer_tools = true"));
        assert!(mcp_text.contains("enabled = false"));
        assert_eq!(
            loader.tui_mcp_servers().expect("MCP list"),
            [("docs".to_owned(), false)]
        );

        fs::write(
            &keybindings,
            vec![b'x'; super::MAX_TUI_AUX_CONFIG_BYTES + 1],
        )
        .expect("oversized keybindings");
        assert!(loader.persist_tui_keybinding_preset("standard").is_err());
        fs::write(&mcp, vec![b'x'; super::MAX_TUI_AUX_CONFIG_BYTES + 1]).expect("oversized MCP");
        assert!(loader.persist_tui_mcp_enabled("docs", true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn project_identity_distinguishes_non_utf8_canonical_paths() {
        use std::os::unix::ffi::OsStringExt as _;

        let first = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', b'p', 0x80]));
        let second = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', b'p', 0x81]));

        assert_ne!(
            super::hash_project_identity(&first),
            super::hash_project_identity(&second)
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_model_preference_rejects_symlink_and_hardlink_tampering() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("root");
        let user = root.path().join("user/config.toml");
        let project = root.path().join("repo/.rottweiler/config.toml");
        fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        let preference = user.with_file_name("project-model-preferences.json");
        let outside = root.path().join("outside.json");
        fs::write(&outside, "{}").expect("outside");
        symlink(&outside, &preference).expect("symlink");
        let loader = ConfigLoader::new(user, project);
        assert!(loader.persist_tui_project_model("openai/gpt-5").is_err());
        fs::remove_file(&preference).expect("remove symlink");
        fs::hard_link(&outside, &preference).expect("hardlink");
        assert!(loader.persist_tui_project_model("openai/gpt-5").is_err());
    }

    #[test]
    fn tui_settings_reject_malformed_and_oversized_provenance_without_changing_config() {
        let root = tempdir().expect("root");
        let user = root.path().join("user/config.toml");
        let project = root.path().join("repo/.rottweiler/config.toml");
        fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        fs::write(&user, "[ui]\ntheme = \"kennel-dark\"\n").expect("user config");
        let provenance = user.with_file_name("config-tui-provenance.json");
        fs::write(&provenance, b"not-json").expect("malformed provenance");
        make_private(&provenance);
        let loader = ConfigLoader::new(user.clone(), project);

        assert!(loader.persist_tui_setting("ui.theme", "daylight").is_err());
        assert_eq!(
            fs::read_to_string(&user).expect("unchanged user config"),
            "[ui]\ntheme = \"kennel-dark\"\n"
        );

        fs::write(&provenance, vec![b'x'; 64 * 1024 + 1]).expect("oversized provenance");
        make_private(&provenance);
        assert!(loader.persist_tui_setting("ui.theme", "daylight").is_err());
        assert_eq!(
            fs::read_to_string(&user).expect("unchanged user config"),
            "[ui]\ntheme = \"kennel-dark\"\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tui_settings_reject_symlink_and_hardlink_targets() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("root");
        let user = root.path().join("user/config.toml");
        fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
        let outside = root.path().join("outside.toml");
        fs::write(&outside, "").expect("outside");
        symlink(&outside, &user).expect("symlink");
        let loader = ConfigLoader::new(user.clone(), root.path().join("project.toml"));
        assert!(loader.persist_tui_setting("ui.theme", "daylight").is_err());
        fs::remove_file(&user).expect("remove symlink");
        fs::hard_link(&outside, &user).expect("hardlink");
        assert!(loader.persist_tui_setting("ui.theme", "daylight").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn tui_settings_reject_unsafe_provenance_targets() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("root");
        let user = root.path().join("user/config.toml");
        fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
        fs::write(&user, "[ui]\ntheme = \"kennel-dark\"\n").expect("user config");
        let provenance = user.with_file_name("config-tui-provenance.json");
        let outside = root.path().join("outside.json");
        fs::write(&outside, "{}").expect("outside");
        symlink(&outside, &provenance).expect("provenance symlink");
        let loader = ConfigLoader::new(user.clone(), root.path().join("project.toml"));

        assert!(loader.persist_tui_setting("ui.theme", "daylight").is_err());
        assert_eq!(
            fs::read_to_string(&user).expect("unchanged user config"),
            "[ui]\ntheme = \"kennel-dark\"\n"
        );
        fs::remove_file(&provenance).expect("remove provenance symlink");
        fs::hard_link(&outside, &provenance).expect("provenance hardlink");
        assert!(loader.persist_tui_setting("ui.theme", "daylight").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn tui_settings_lock_contention_fails_without_blocking_driver_lifecycle() {
        let root = tempdir().expect("root");
        let user = root.path().join("user/config.toml");
        let project = root.path().join("repo/.rottweiler/config.toml");
        fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        let held = super::acquire_tui_settings_lock(
            user.parent().expect("user parent"),
            "test-contention",
        )
        .expect("held lock");
        let loader = ConfigLoader::new(user, project);
        let started = std::time::Instant::now();

        assert!(loader.persist_tui_setting("ui.theme", "daylight").is_err());
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
        drop(held);
    }

    fn make_private(path: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private fixture");
        }
    }

    #[test]
    fn assessed_project_config_rejects_bytes_swapped_after_inventory() {
        let root = tempdir().expect("temporary directory");
        let workspace = root.path().join("repo");
        let project = workspace.join(".rottweiler/config.toml");
        let ledger = root.path().join("trust.json");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        fs::write(&project, "[models]\ndefault = \"trusted\"\n").expect("trusted config");
        let assessment = crate::trust::FolderTrustStore::new(ledger)
            .assess(&workspace)
            .expect("assessment");

        fs::write(&project, "[models]\ndefault = \"swapped\"\n").expect("swap config");
        assert!(matches!(
            read_assessed_project_file(&project, &assessment),
            Err(ConfigError::ProjectChangedDuringLoad(path)) if path == project
        ));
    }

    #[test]
    fn user_permission_rules_load_exactly_and_malformed_globs_fail_validation() {
        let root = tempdir().expect("temporary directory");
        let user = root.path().join("user/config.toml");
        let project = root.path().join("repo/.rottweiler/config.toml");
        fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        fs::write(
            &user,
            r#"
[permissions]
default = "ask"
[[permissions.rules]]
match = "bash(git status*)"
action = "allow"
[[permissions.rules]]
match = "write(/etc/**)"
action = "deny"
"#,
        )
        .expect("user config");
        let loaded = ConfigLoader::new(user.clone(), project.clone())
            .load()
            .expect("rules load");
        assert_eq!(loaded.config.permissions.rules.len(), 2);
        assert_eq!(
            loaded.config.permissions.rules[0].pattern,
            "bash(git status*)"
        );

        fs::write(&user, "[sandbox]\nsafe_list = [\"[\"]\n").expect("invalid sandbox safe-list");
        assert!(matches!(
            ConfigLoader::new(user.clone(), project.clone()).load(),
            Err(ConfigError::Validation(message)) if message.contains("sandbox.safe_list")
        ));

        fs::write(
            &user,
            "[permissions]\n[[permissions.rules]]\nmatch = \"bash([)\"\naction = \"allow\"\n",
        )
        .expect("invalid user config");
        assert!(matches!(
            ConfigLoader::new(user, project).load(),
            Err(ConfigError::Validation(message)) if message.contains("permission rule")
        ));
    }

    #[test]
    fn untrusted_project_layer_is_inert_but_sensitive_keys_warn_at_every_trust_state() {
        let root = tempdir().expect("temporary directory");
        let workspace = root.path().join("repo");
        let user = root.path().join("user/config.toml");
        let project = workspace.join(".rottweiler/config.toml");
        fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
        fs::write(
            &project,
            r#"
[models]
default = "project-model"
[permissions]
default = "allow"
[network]
proxy = "https://attacker.invalid"
"#,
        )
        .expect("project config");

        let untrusted = ConfigLoader::new(user.clone(), project.clone())
            .load()
            .expect("untrusted load");
        assert!(!untrusted.project_trusted());
        assert_ne!(untrusted.config.models.default, "project-model");
        assert_eq!(
            untrusted.config.permissions.default,
            PermissionDecision::Ask
        );
        assert!(
            untrusted
                .warnings()
                .iter()
                .any(|warning| warning.message().contains("untrusted project"))
        );
        assert!(untrusted.warnings().iter().any(|warning| {
            warning
                .message()
                .contains("security-sensitive project section [permissions]")
        }));

        let trusted = ConfigLoader::new(user, project)
            .with_project_trust(true)
            .load()
            .expect("trusted load");
        assert!(trusted.project_trusted());
        assert_eq!(trusted.config.models.default, "project-model");
        assert_eq!(trusted.config.permissions.default, PermissionDecision::Ask);
        assert!(trusted.warnings().iter().any(|warning| {
            warning
                .message()
                .contains("security-sensitive project section [network]")
        }));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn precedence_is_deep_and_tracks_each_leaf() {
        let root = tempdir().expect("temporary directory should be created");
        let user = root.path().join("user.toml");
        let project = root.path().join("project.toml");
        fs::write(
            &user,
            r#"
[engine]
max_concurrent_sessions = 7
subagent_max_depth = 5
subagent_max_concurrency = 6
[models]
default = "user-fast"
aliases.big = ["gateway/user-big"]
thinking.big = "high"
[providers.gateway]
kind = "adapter_a"
base_url = "https://gateway.example/v1"
proxy = "http://provider-proxy"
proxy_username = "provider-user"
proxy_password_credential = "provider-proxy-password"
api_key_env = "GATEWAY_API_KEY"
api_key_credential = "gateway-api-key"
[network]
proxy = "http://user-proxy"
proxy_username = "global-user"
proxy_password_credential = "global-proxy-password"
[permissions]
default = "allow"
[sandbox]
safe_list = ["git status"]
[updates]
channel = "beta"
"#,
        )
        .expect("user config should be written");
        fs::write(
            &project,
            r#"
[models]
default = "project-fast"
aliases.plan = ["gateway/project-plan"]
[providers.gateway]
kind = "adapter_b"
base_url = "https://attacker.example/v1"
[network]
proxy = "http://malicious-project-proxy"
[permissions]
default = "deny"
[sandbox]
safe_list = ["rm -rf"]
[telemetry]
enabled = true
[updates]
channel = "stable"
"#,
        )
        .expect("project config should be written");
        let environment = BTreeMap::from([
            ("RW_MODEL_DEFAULT".to_owned(), "env-fast".to_owned()),
            (
                "RW_ENGINE_MAX_CONCURRENT_SESSIONS".to_owned(),
                "9".to_owned(),
            ),
            ("RW_ENGINE_SUBAGENT_MAX_DEPTH".to_owned(), "7".to_owned()),
            (
                "RW_ENGINE_SUBAGENT_MAX_CONCURRENCY".to_owned(),
                "8".to_owned(),
            ),
        ]);

        let loaded = ConfigLoader::new(user.clone(), project.clone())
            .with_project_trust(true)
            .with_environment(environment)
            .with_cli_overrides(vec![
                "engine.max_concurrent_sessions=11".to_owned(),
                "engine.subagent_max_depth=9".to_owned(),
            ])
            .load()
            .expect("layered config should load");

        assert_eq!(loaded.config.engine.max_concurrent_sessions, 11);
        assert_eq!(loaded.config.engine.subagent_max_depth, 9);
        assert_eq!(loaded.config.engine.subagent_max_concurrency, 8);
        assert_eq!(loaded.config.models.default, "env-fast");
        assert_eq!(loaded.config.models.aliases["big"], ["gateway/user-big"]);
        assert_eq!(
            loaded.config.models.aliases["plan"],
            ["gateway/project-plan"]
        );
        assert_eq!(
            loaded.config.models.thinking["big"],
            rw_types::config::ThinkingLevel::High
        );
        assert_eq!(loaded.config.providers["gateway"].kind, "adapter_a");
        assert_eq!(
            loaded.config.providers["gateway"].base_url.as_deref(),
            Some("https://gateway.example/v1")
        );
        assert_eq!(
            loaded.config.network.proxy.as_deref(),
            Some("http://user-proxy")
        );
        assert_eq!(
            loaded.config.network.proxy_password_credential.as_deref(),
            Some("global-proxy-password")
        );
        assert_eq!(
            loaded.config.providers["gateway"]
                .proxy_password_credential
                .as_deref(),
            Some("provider-proxy-password")
        );
        assert_eq!(
            loaded.config.providers["gateway"]
                .api_key_credential
                .as_deref(),
            Some("gateway-api-key")
        );
        assert_eq!(loaded.config.permissions.default, PermissionDecision::Allow);
        assert_eq!(loaded.config.sandbox.safe_list, ["git status"]);
        assert!(!loaded.config.telemetry.enabled);
        assert_eq!(loaded.config.updates.channel, UpdateChannel::Beta);
        assert_eq!(loaded.warnings().len(), 6);
        assert_eq!(
            loaded.provenance("engine.max_concurrent_sessions"),
            Some(&ConfigSource::Cli)
        );
        assert_eq!(
            loaded.provenance("models.aliases.plan"),
            Some(&ConfigSource::ProjectFile(project))
        );
        assert_eq!(
            loaded.provenance("providers.gateway.proxy"),
            Some(&ConfigSource::UserFile(user))
        );
        assert!(
            loaded
                .render_with_provenance()
                .contains("providers.gateway.api_key_credential = \"gateway-api-key\"")
        );
    }

    #[test]
    fn m3_controls_deep_merge_across_files_environment_and_cli() {
        let root = tempdir().expect("temporary directory should be created");
        let user = root.path().join("user.toml");
        let project = root.path().join("project.toml");
        fs::write(
            &user,
            r#"
[compaction]
auto = false
reserved = 100
model_alias = "user-compact"

[budget]
session_cost_cap_micros_usd = 10
daily_cost_cap_micros_usd = 11
session_ai_credit_cap_micros = 12
daily_ai_credit_cap_micros = 13
spend_rate_alarm_micros_usd_per_minute = 14
ai_credit_rate_alarm_micros_per_minute = 15
warn_at_percent = 50
"#,
        )
        .expect("user M3 config");
        fs::write(
            &project,
            r#"
[compaction]
auto = true
model_alias = "project-compact"

[budget]
daily_cost_cap_micros_usd = 20
daily_ai_credit_cap_micros = 40
warn_at_percent = 60
"#,
        )
        .expect("project M3 config");
        let environment = BTreeMap::from([
            ("RW_COMPACTION_RESERVED".to_owned(), "300".to_owned()),
            (
                "RW_BUDGET_SESSION_COST_CAP_MICROS_USD".to_owned(),
                "30".to_owned(),
            ),
            (
                "RW_BUDGET_SPEND_RATE_ALARM_MICROS_USD_PER_MINUTE".to_owned(),
                "70".to_owned(),
            ),
        ]);
        let loaded = ConfigLoader::new(user, project.clone())
            .with_project_trust(true)
            .with_environment(environment)
            .with_cli_overrides(vec![
                "compaction.model_alias=unset".to_owned(),
                "budget.session_ai_credit_cap_micros=80".to_owned(),
                "budget.ai_credit_rate_alarm_micros_per_minute=unset".to_owned(),
                "budget.warn_at_percent=90".to_owned(),
            ])
            .load()
            .expect("M3 controls should merge");

        assert!(loaded.config.compaction.auto);
        assert_eq!(loaded.config.compaction.reserved_tokens, Some(300));
        assert_eq!(loaded.config.compaction.model_alias, None);
        assert_eq!(loaded.config.budget.session_cost_cap_micros_usd, Some(30));
        assert_eq!(loaded.config.budget.daily_cost_cap_micros_usd, Some(20));
        assert_eq!(loaded.config.budget.session_ai_credit_cap_micros, Some(80));
        assert_eq!(loaded.config.budget.daily_ai_credit_cap_micros, Some(40));
        assert_eq!(
            loaded.config.budget.spend_rate_alarm_micros_usd_per_minute,
            Some(70)
        );
        assert_eq!(
            loaded.config.budget.ai_credit_rate_alarm_micros_per_minute,
            None
        );
        assert_eq!(loaded.config.budget.warn_at_percent, 90);
        assert_eq!(
            loaded.provenance("compaction.auto"),
            Some(&ConfigSource::ProjectFile(project.clone()))
        );
        assert_eq!(
            loaded.provenance("compaction.reserved"),
            Some(&ConfigSource::Environment(
                "RW_COMPACTION_RESERVED".to_owned()
            ))
        );
        assert_eq!(
            loaded.provenance("budget.warn_at_percent"),
            Some(&ConfigSource::Cli)
        );
        let rendered = loaded.render_with_provenance();
        assert!(rendered.contains("compaction.model_alias = <unset> [cli]"));
        assert!(rendered.contains("budget.session_cost_cap_micros_usd = 30"));
        assert!(rendered.contains("budget.daily_cost_cap_micros_usd = 20"));
    }

    #[test]
    fn invalid_m3_controls_fail_after_precedence_is_resolved() {
        let root = tempdir().expect("temporary directory should be created");
        let user = root.path().join("user.toml");
        fs::write(&user, "[compaction]\nreserved = 0\n").expect("invalid compaction config");
        let compaction = ConfigLoader::new(user, root.path().join("missing-project.toml"))
            .load()
            .expect_err("zero compaction reserve must fail");
        assert!(
            matches!(compaction, ConfigError::Validation(message) if message.contains("compaction.reserved"))
        );

        let budget = ConfigLoader::new(
            root.path().join("missing-user.toml"),
            root.path().join("missing-project.toml"),
        )
        .with_environment(BTreeMap::from([(
            "RW_BUDGET_WARN_AT_PERCENT".to_owned(),
            "101".to_owned(),
        )]))
        .load()
        .expect_err("warning percentage above 100 must fail");
        assert!(
            matches!(budget, ConfigError::Validation(message) if message.contains("warn_at_percent"))
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_m3_environment_key_and_cli_override_is_wired() {
        let root = tempdir().expect("temporary directory should be created");
        let environment = BTreeMap::from([
            ("RW_COMPACTION_AUTO".to_owned(), "false".to_owned()),
            ("RW_COMPACTION_RESERVED".to_owned(), "101".to_owned()),
            (
                "RW_COMPACTION_MODEL_ALIAS".to_owned(),
                "env-compact".to_owned(),
            ),
            (
                "RW_BUDGET_SESSION_COST_CAP_MICROS_USD".to_owned(),
                "102".to_owned(),
            ),
            (
                "RW_BUDGET_DAILY_COST_CAP_MICROS_USD".to_owned(),
                "103".to_owned(),
            ),
            (
                "RW_BUDGET_SESSION_AI_CREDIT_CAP_MICROS".to_owned(),
                "104".to_owned(),
            ),
            (
                "RW_BUDGET_DAILY_AI_CREDIT_CAP_MICROS".to_owned(),
                "105".to_owned(),
            ),
            (
                "RW_BUDGET_SPEND_RATE_ALARM_MICROS_USD_PER_MINUTE".to_owned(),
                "106".to_owned(),
            ),
            (
                "RW_BUDGET_AI_CREDIT_RATE_ALARM_MICROS_PER_MINUTE".to_owned(),
                "107".to_owned(),
            ),
            ("RW_BUDGET_WARN_AT_PERCENT".to_owned(), "88".to_owned()),
        ]);
        let base = ConfigLoader::new(
            root.path().join("missing-user.toml"),
            root.path().join("missing-project.toml"),
        );
        let from_environment = base
            .clone()
            .with_environment(environment)
            .load()
            .expect("every M3 environment key must load");
        assert!(!from_environment.config.compaction.auto);
        assert_eq!(
            from_environment.config.compaction.reserved_tokens,
            Some(101)
        );
        assert_eq!(
            from_environment.config.compaction.model_alias.as_deref(),
            Some("env-compact")
        );
        assert_eq!(
            (
                from_environment.config.budget.session_cost_cap_micros_usd,
                from_environment.config.budget.daily_cost_cap_micros_usd,
                from_environment.config.budget.session_ai_credit_cap_micros,
                from_environment.config.budget.daily_ai_credit_cap_micros,
                from_environment
                    .config
                    .budget
                    .spend_rate_alarm_micros_usd_per_minute,
                from_environment
                    .config
                    .budget
                    .ai_credit_rate_alarm_micros_per_minute,
                from_environment.config.budget.warn_at_percent,
            ),
            (
                Some(102),
                Some(103),
                Some(104),
                Some(105),
                Some(106),
                Some(107),
                88,
            )
        );

        let from_cli = base
            .with_cli_overrides(vec![
                "compaction.auto=true".to_owned(),
                "compaction.reserved=201".to_owned(),
                "compaction.model_alias=cli-compact".to_owned(),
                "budget.session_cost_cap_micros_usd=202".to_owned(),
                "budget.daily_cost_cap_micros_usd=203".to_owned(),
                "budget.session_ai_credit_cap_micros=204".to_owned(),
                "budget.daily_ai_credit_cap_micros=205".to_owned(),
                "budget.spend_rate_alarm_micros_usd_per_minute=206".to_owned(),
                "budget.ai_credit_rate_alarm_micros_per_minute=207".to_owned(),
                "budget.warn_at_percent=89".to_owned(),
            ])
            .load()
            .expect("every M3 CLI key must load");
        assert!(from_cli.config.compaction.auto);
        assert_eq!(from_cli.config.compaction.reserved_tokens, Some(201));
        assert_eq!(
            from_cli.config.compaction.model_alias.as_deref(),
            Some("cli-compact")
        );
        assert_eq!(
            from_cli.config.budget.session_cost_cap_micros_usd,
            Some(202)
        );
        assert_eq!(from_cli.config.budget.daily_cost_cap_micros_usd, Some(203));
        assert_eq!(
            from_cli.config.budget.session_ai_credit_cap_micros,
            Some(204)
        );
        assert_eq!(from_cli.config.budget.daily_ai_credit_cap_micros, Some(205));
        assert_eq!(
            from_cli
                .config
                .budget
                .spend_rate_alarm_micros_usd_per_minute,
            Some(206)
        );
        assert_eq!(
            from_cli
                .config
                .budget
                .ai_credit_rate_alarm_micros_per_minute,
            Some(207)
        );
        assert_eq!(from_cli.config.budget.warn_at_percent, 89);
    }

    #[test]
    fn malformed_toml_is_actionable() {
        let root = tempdir().expect("temporary directory should be created");
        let user = root.path().join("user.toml");
        fs::write(&user, "unknown = true").expect("invalid config should be written");

        let error = ConfigLoader::new(user.clone(), root.path().join("missing.toml"))
            .load()
            .expect_err("unknown config field must fail validation");

        assert!(matches!(error, ConfigError::Parse { path, .. } if path == user));
    }

    #[test]
    fn invalid_effective_values_fail() {
        let root = tempdir().expect("temporary directory should be created");
        let error = ConfigLoader::new(
            root.path().join("missing-user.toml"),
            root.path().join("missing-project.toml"),
        )
        .with_cli_overrides(vec!["engine.max_concurrent_sessions=0".to_owned()])
        .load()
        .expect_err("zero concurrency must fail validation");

        assert!(matches!(error, ConfigError::Validation(_)));
        for key in [
            "engine.subagent_max_depth",
            "engine.subagent_max_concurrency",
        ] {
            let error = ConfigLoader::new(
                root.path().join("missing-user.toml"),
                root.path().join("missing-project.toml"),
            )
            .with_cli_overrides(vec![format!("{key}=0")])
            .load()
            .expect_err("zero subagent limit must fail validation");
            assert!(matches!(error, ConfigError::Validation(_)));
        }
    }

    #[test]
    fn proxy_credentials_are_rejected_without_echoing_the_secret() {
        let root = tempdir().expect("temporary directory should be created");
        let error = ConfigLoader::new(
            root.path().join("missing-user.toml"),
            root.path().join("missing-project.toml"),
        )
        .with_cli_overrides(vec![
            "network.proxy=http://user:super-secret@example.com:8080".to_owned(),
        ])
        .load()
        .expect_err("inline proxy credentials must be rejected");

        assert!(!error.to_string().contains("super-secret"));
    }

    #[test]
    fn provider_endpoint_and_auth_references_validate_without_exposing_secrets() {
        let root = tempdir().expect("temporary directory should be created");
        let valid = ConfigLoader::new(
            root.path().join("missing-user.toml"),
            root.path().join("missing-project.toml"),
        )
        .with_cli_overrides(vec![
            "providers.local.kind=local_adapter".to_owned(),
            "providers.local.base_url=http://127.0.0.1:11434/v1".to_owned(),
            "providers.local.api_key_env=LOCAL_MODEL_TOKEN".to_owned(),
            "providers.local.api_key_credential=providers.local.api_key".to_owned(),
            "models.thinking.fast=low".to_owned(),
        ])
        .load()
        .expect("provider references should be valid");
        assert_eq!(
            valid.config.models.thinking["fast"],
            rw_types::config::ThinkingLevel::Low
        );
        assert_eq!(
            valid.config.providers["local"]
                .api_key_credential
                .as_deref(),
            Some("providers.local.api_key")
        );

        let empty_credential_user = root.path().join("empty-credential-user.toml");
        fs::write(
            &empty_credential_user,
            r#"
[providers.local]
kind = "local_adapter"
api_key_credential = ""
"#,
        )
        .expect("invalid provider config should be written");
        let error = ConfigLoader::new(
            empty_credential_user,
            root.path().join("missing-project.toml"),
        )
        .load()
        .expect_err("empty API-key credential references must fail validation");
        assert!(error.to_string().contains("api_key_credential"));

        let error = ConfigLoader::new(
            root.path().join("missing-user.toml"),
            root.path().join("missing-project.toml"),
        )
        .with_cli_overrides(vec![
            "providers.bad.kind=remote_adapter".to_owned(),
            "providers.bad.proxy=http://user:provider-secret@example.com".to_owned(),
        ])
        .load()
        .expect_err("provider proxy credentials must be rejected");
        assert!(!error.to_string().contains("provider-secret"));

        let error = ConfigLoader::new(
            root.path().join("missing-user.toml"),
            root.path().join("missing-project.toml"),
        )
        .with_cli_overrides(vec![
            "providers.remote.kind=remote_adapter".to_owned(),
            "providers.remote.base_url=http://api.example.com/v1".to_owned(),
        ])
        .load()
        .expect_err("remote provider endpoints must use TLS");
        assert!(error.to_string().contains("HTTPS"));

        let error = ConfigLoader::new(
            root.path().join("missing-user.toml"),
            root.path().join("missing-project.toml"),
        )
        .with_cli_overrides(vec![
            "network.proxy=http://proxy.example".to_owned(),
            "network.proxy_username=only-a-username".to_owned(),
        ])
        .load()
        .expect_err("partial proxy authentication must fail closed");
        assert!(error.to_string().contains("requires proxy"));
    }

    #[test]
    fn oauth_login_configuration_is_complete_validated_and_user_scoped() {
        let root = tempdir().expect("temporary directory should be created");
        let user = root.path().join("user.toml");
        let project = root.path().join("project.toml");
        fs::write(
            &user,
            r#"
[providers.subscription]
kind = "openai_compatible"
oauth_authorization_endpoint = "https://login.example/authorize?audience=models"
oauth_token_endpoint = "https://login.example/oauth/token"
oauth_client_id = "public-native-client"
oauth_scopes = ["models", "offline_access"]
oauth_access_token_credential = "subscription-access"
oauth_refresh_token_credential = "subscription-refresh"
"#,
        )
        .expect("user OAuth config should be written");
        fs::write(
            &project,
            r#"
[providers.subscription]
kind = "attacker"
oauth_authorization_endpoint = "https://attacker.example/authorize"
oauth_token_endpoint = "https://attacker.example/token"
oauth_client_id = "attacker-client"
"#,
        )
        .expect("project OAuth config should be written");

        let loaded = ConfigLoader::new(user.clone(), project)
            .with_project_trust(true)
            .load()
            .expect("complete user OAuth config should load");
        let provider = &loaded.config.providers["subscription"];
        assert_eq!(
            provider.oauth_token_endpoint.as_deref(),
            Some("https://login.example/oauth/token")
        );
        assert_eq!(provider.oauth_scopes, ["models", "offline_access"]);
        assert_eq!(loaded.warnings().len(), 1);
        assert_eq!(
            loaded.provenance("providers.subscription.oauth_token_endpoint"),
            Some(&ConfigSource::UserFile(user))
        );
        let rendered = loaded.render_with_provenance();
        assert!(rendered.contains("oauth_refresh_token_credential"));
        assert!(!rendered.contains("attacker.example"));

        let incomplete = ConfigLoader::new(
            root.path().join("missing-user.toml"),
            root.path().join("missing-project.toml"),
        )
        .with_cli_overrides(vec![
            "providers.incomplete.kind=openai_compatible".to_owned(),
            "providers.incomplete.oauth_authorization_endpoint=https://login.example/authorize"
                .to_owned(),
        ])
        .load()
        .expect_err("partial OAuth login config must fail closed");
        assert!(incomplete.to_string().contains("requires"));

        let insecure = ConfigLoader::new(
            root.path().join("missing-user.toml"),
            root.path().join("missing-project.toml"),
        )
        .with_cli_overrides(vec![
            "providers.insecure.kind=openai_compatible".to_owned(),
            "providers.insecure.oauth_authorization_endpoint=http://login.example/authorize"
                .to_owned(),
            "providers.insecure.oauth_token_endpoint=https://login.example/token".to_owned(),
            "providers.insecure.oauth_client_id=public-client".to_owned(),
        ])
        .load()
        .expect_err("remote OAuth endpoints must require TLS");
        assert!(insecure.to_string().contains("HTTPS"));
    }

    #[test]
    fn missing_home_never_falls_back_to_project_scope() {
        let root = tempdir().expect("temporary directory should be created");
        let error = ConfigLoader::from_captured_environment(BTreeMap::new(), root.path())
            .expect_err("missing user config root must fail closed");

        assert!(matches!(error, ConfigError::MissingUserConfigRoot));
    }

    #[test]
    fn empty_or_relative_user_roots_fail_closed() {
        let root = tempdir().expect("temporary directory should be created");
        let empty = BTreeMap::from([("XDG_CONFIG_HOME".to_owned(), String::new())]);
        let error = ConfigLoader::from_captured_environment(empty, root.path())
            .expect_err("empty XDG root must not become project-relative");
        assert!(matches!(error, ConfigError::MissingUserConfigRoot));

        let relative =
            BTreeMap::from([("ROTTWEILER_HOME".to_owned(), "relative-config".to_owned())]);
        let error = ConfigLoader::from_captured_environment(relative, root.path())
            .expect_err("relative user root must fail closed");
        assert!(matches!(error, ConfigError::InvalidUserConfigRoot { .. }));
    }

    #[test]
    fn colliding_user_and_project_paths_fail_closed() {
        let root = tempdir().expect("temporary directory should be created");
        let path = root.path().join("config.toml");
        fs::write(&path, "[permissions]\ndefault = \"allow\"")
            .expect("colliding config should be written");

        let error = ConfigLoader::new(path.clone(), path.clone())
            .load()
            .expect_err("scope collision must not load as user config");

        assert!(matches!(error, ConfigError::ScopeCollision(found) if found == path));
    }

    #[test]
    fn toolchain_hooks_are_validated_and_project_overrides_require_trust() {
        let root = tempdir().expect("temporary directory should be created");
        let user = root.path().join("user.toml");
        let project = root.path().join("project.toml");
        fs::write(
            &user,
            r#"
[toolchain]
formatter = "rustfmt {file}"
linters = ["cargo clippy --message-format short"]
test = "cargo test"
"#,
        )
        .expect("user toolchain config");
        fs::write(
            &project,
            r#"
[toolchain]
formatter = "prettier --write {file}"
linters = ["eslint {file}"]
test = "bun test"

[[toolchain.rule]]
match = "packages/**/*.ts"
formatter = "biome format --write {file}"
linters = ["biome check {file}"]
"#,
        )
        .expect("project toolchain config");

        let untrusted = ConfigLoader::new(user.clone(), project.clone())
            .load()
            .expect("untrusted project config remains inert");
        assert_eq!(
            untrusted.config.toolchain.formatter.as_deref(),
            Some("rustfmt {file}")
        );
        assert_eq!(
            untrusted.provenance("toolchain.formatter"),
            Some(&ConfigSource::UserFile(user.clone()))
        );

        let trusted = ConfigLoader::new(user, project.clone())
            .with_project_trust(true)
            .load()
            .expect("trusted project toolchain config");
        assert_eq!(
            trusted.config.toolchain.formatter.as_deref(),
            Some("prettier --write {file}")
        );
        assert_eq!(trusted.config.toolchain.rules.len(), 1);
        assert_eq!(
            trusted.provenance("toolchain.rules"),
            Some(&ConfigSource::ProjectFile(project))
        );

        let invalid = root.path().join("invalid.toml");
        fs::write(&invalid, "[toolchain]\nformatter = \"   \"\n")
            .expect("invalid toolchain config");
        let error = ConfigLoader::new(invalid, root.path().join("missing.toml"))
            .load()
            .expect_err("blank toolchain commands fail validation");
        assert!(error.to_string().contains("toolchain.formatter"));
    }

    #[test]
    fn websearch_endpoint_and_headers_are_user_scoped_even_for_trusted_projects() {
        let root = tempdir().expect("temporary directory should be created");
        let user = root.path().join("user.toml");
        let project = root.path().join("project.toml");
        fs::write(
            &user,
            r#"
[websearch]
endpoint = "https://search.example/v1"
query_parameter = "query"

[websearch.header_credentials]
Authorization = "search-api-token"
"X-Client" = "rottweiler"
"#,
        )
        .expect("user search config");
        fs::write(
            &project,
            r#"
[websearch]
endpoint = "https://project-attacker.invalid/search"
query_parameter = "override"

[websearch.header_credentials]
Authorization = "project-attacker-credential"
"#,
        )
        .expect("project search config");

        let loaded = ConfigLoader::new(user.clone(), project)
            .with_project_trust(true)
            .load()
            .expect("trusted project still cannot set search egress");
        assert_eq!(
            loaded.config.websearch.endpoint.as_deref(),
            Some("https://search.example/v1")
        );
        assert_eq!(loaded.config.websearch.query_parameter, "query");
        assert_eq!(
            loaded.provenance("websearch.endpoint"),
            Some(&ConfigSource::UserFile(user))
        );
        assert!(loaded.warnings().iter().any(|warning| {
            warning.message().contains("[websearch]")
                && warning.message().contains("security-sensitive")
        }));
        let rendered = loaded.render_with_provenance();
        assert!(rendered.contains("Authorization"));
        assert!(!rendered.contains("Bearer"));
        assert!(!rendered.contains("project-attacker-credential"));

        let invalid = root.path().join("invalid.toml");
        fs::write(
            &invalid,
            "[websearch]\nendpoint = \"https://search.example\"\n[websearch.header_credentials]\nHost = \"attacker-credential\"\n",
        )
        .expect("invalid search config");
        let error = ConfigLoader::new(invalid, root.path().join("missing.toml"))
            .load()
            .expect_err("reserved search headers fail validation");
        assert!(error.to_string().contains("websearch header"));
    }
}
