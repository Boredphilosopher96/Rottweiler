//! Layered TOML configuration loading with per-leaf provenance.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Condvar, Mutex, OnceLock};

use rw_types::config::Config;
use thiserror::Error;

use crate::trust::FolderTrustError;

const ENV_ENGINE_SESSIONS: &str = "RW_ENGINE_MAX_CONCURRENT_SESSIONS";
const ENV_SUBAGENT_DEPTH: &str = "RW_ENGINE_SUBAGENT_MAX_DEPTH";
const ENV_SUBAGENT_CONCURRENCY: &str = "RW_ENGINE_SUBAGENT_MAX_CONCURRENCY";
const ENV_MODEL_DEFAULT: &str = "RW_MODEL_DEFAULT";
const ENV_COMPACTION_AUTO: &str = "RW_COMPACTION_AUTO";
const ENV_COMPACTION_RESERVED: &str = "RW_COMPACTION_RESERVED";
const ENV_COMPACTION_MODEL_ALIAS: &str = "RW_COMPACTION_MODEL_ALIAS";
const ENV_BUDGET_SESSION_COST_CAP: &str = "RW_BUDGET_SESSION_COST_CAP_MICROS_USD";
const ENV_BUDGET_DAILY_COST_CAP: &str = "RW_BUDGET_DAILY_COST_CAP_MICROS_USD";
const ENV_BUDGET_SESSION_CREDIT_CAP: &str = "RW_BUDGET_SESSION_AI_CREDIT_CAP_MICROS";
const ENV_BUDGET_DAILY_CREDIT_CAP: &str = "RW_BUDGET_DAILY_AI_CREDIT_CAP_MICROS";
const ENV_BUDGET_SESSION_TOKEN_CAP: &str = "RW_BUDGET_SESSION_TOKEN_CAP";
const ENV_BUDGET_DAILY_TOKEN_CAP: &str = "RW_BUDGET_DAILY_TOKEN_CAP";
const ENV_BUDGET_SPEND_RATE: &str = "RW_BUDGET_SPEND_RATE_ALARM_MICROS_USD_PER_MINUTE";
const ENV_BUDGET_CREDIT_RATE: &str = "RW_BUDGET_AI_CREDIT_RATE_ALARM_MICROS_PER_MINUTE";
const ENV_BUDGET_TOKEN_RATE: &str = "RW_BUDGET_TOKEN_RATE_ALARM_PER_MINUTE";
const ENV_BUDGET_WARN_PERCENT: &str = "RW_BUDGET_WARN_AT_PERCENT";
const ENV_NETWORK_PROXY: &str = "RW_NETWORK_PROXY";
const ENV_NETWORK_PROXY_USERNAME: &str = "RW_NETWORK_PROXY_USERNAME";
const ENV_NETWORK_PROXY_PASSWORD_CREDENTIAL: &str = "RW_NETWORK_PROXY_PASSWORD_CREDENTIAL";
const ENV_PERMISSION_DEFAULT: &str = "RW_PERMISSION_DEFAULT";
const ENV_SANDBOX_SAFE_LIST: &str = "RW_SANDBOX_SAFE_LIST";
const ENV_TELEMETRY_ENABLED: &str = "RW_TELEMETRY_ENABLED";
const ENV_UPDATE_CHANNEL: &str = "RW_UPDATE_CHANNEL";
const MAX_TUI_AUX_CONFIG_BYTES: usize = 64 * 1024;
const MAX_MCP_ARG_BYTES: usize = 16 * 1024;
const MAX_MCP_ARGV_ENTRIES: usize = 256;
const MAX_MCP_ENVIRONMENT_ENTRIES: usize = 256;
const MAX_MCP_ENVIRONMENT_NAME_BYTES: usize = 128;
const MAX_MCP_ENVIRONMENT_VALUE_BYTES: usize = 16 * 1024;
static TUI_SETTING_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static TUI_SETTING_PROCESS_LOCKS: OnceLock<(Mutex<BTreeSet<PathBuf>>, Condvar)> = OnceLock::new();

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

mod values;
use values::{override_reason, quoted};

mod loading;

mod editing;
use editing::{configured_setting_value, read_tui_provenance};

mod layers;
use layers::{
    FileScope, apply_environment, apply_file, apply_override, defaults_with_provenance,
    nonempty_value, read_assessed_project_file, read_file, set_source,
    warn_ignored_project_sections,
};

mod validation;
use validation::{
    parse_tui_budget_cap, parse_tui_budget_warning, parse_tui_token_limit, paths_collide, validate,
    validate_tui_setting,
};

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests;
