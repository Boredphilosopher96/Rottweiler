//! Layered TOML configuration loading with per-leaf provenance.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use rw_types::config::{
    Config, ConfigFile, PermissionDecision, ProviderConfig, ThinkingLevel, UpdateChannel,
};
use thiserror::Error;
use url::{Host, Url};

const ENV_ENGINE_SESSIONS: &str = "RW_ENGINE_MAX_CONCURRENT_SESSIONS";
const ENV_MODEL_DEFAULT: &str = "RW_MODEL_DEFAULT";
const ENV_NETWORK_PROXY: &str = "RW_NETWORK_PROXY";
const ENV_NETWORK_PROXY_USERNAME: &str = "RW_NETWORK_PROXY_USERNAME";
const ENV_NETWORK_PROXY_PASSWORD_CREDENTIAL: &str = "RW_NETWORK_PROXY_PASSWORD_CREDENTIAL";
const ENV_PERMISSION_DEFAULT: &str = "RW_PERMISSION_DEFAULT";
const ENV_SANDBOX_SAFE_LIST: &str = "RW_SANDBOX_SAFE_LIST";
const ENV_TELEMETRY_ENABLED: &str = "RW_TELEMETRY_ENABLED";
const ENV_UPDATE_CHANNEL: &str = "RW_UPDATE_CHANNEL";

/// A layer that supplied one effective configuration leaf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Compiled-in safe defaults.
    BuiltIn,
    /// User-level TOML.
    UserFile(PathBuf),
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

    /// Renders stable, scriptable effective values with a source per leaf.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn render_with_provenance(&self) -> String {
        let mut lines = Vec::new();
        lines.push(self.render_leaf(
            "engine.max_concurrent_sessions",
            &self.config.engine.max_concurrent_sessions.to_string(),
        ));
        lines.push(self.render_leaf("models.default", &quoted(&self.config.models.default)));

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
        lines.push(self.render_leaf(
            "permissions.default",
            permission_name(self.config.permissions.default),
        ));
        lines.push(self.render_leaf(
            "sandbox.safe_list",
            &format!("{:?}", self.config.sandbox.safe_list),
        ));
        lines.push(self.render_leaf(
            "telemetry.enabled",
            &self.config.telemetry.enabled.to_string(),
        ));
        lines.push(self.render_leaf(
            "updates.channel",
            update_channel_name(self.config.updates.channel),
        ));
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
}

/// Loader with injectable paths, environment, and CLI state for deterministic tests.
#[derive(Debug, Clone)]
pub struct ConfigLoader {
    user_path: PathBuf,
    project_path: PathBuf,
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
            environment,
            cli_overrides: Vec::new(),
        })
    }

    /// Creates an isolated loader for tests and embedded SDK callers.
    #[must_use]
    pub fn new(user_path: PathBuf, project_path: PathBuf) -> Self {
        Self {
            user_path,
            project_path,
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

    /// User-scoped credential fallback adjacent to the effective user config.
    #[must_use]
    pub fn credentials_path(&self) -> PathBuf {
        self.user_path.with_file_name("credentials.toml")
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
        }
        if let Some(file) = read_file(&self.project_path)? {
            let source = ConfigSource::ProjectFile(self.project_path.clone());
            apply_file(&mut loaded, file, &source, FileScope::Project);
        }
        apply_environment(&mut loaded, &self.environment)?;
        for cli_override in &self.cli_overrides {
            apply_override(&mut loaded, cli_override, &ConfigSource::Cli)?;
        }
        validate(&loaded.config)?;
        Ok(loaded)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileScope {
    User,
    Project,
}

fn defaults_with_provenance() -> LoadedConfig {
    let provenance = [
        "engine.max_concurrent_sessions",
        "models.default",
        "models.aliases",
        "models.thinking",
        "providers",
        "network.proxy",
        "network.proxy_username",
        "network.proxy_password_credential",
        "permissions.default",
        "sandbox.safe_list",
        "telemetry.enabled",
        "updates.channel",
    ]
    .into_iter()
    .map(|key| (key.to_owned(), ConfigSource::BuiltIn))
    .collect();
    LoadedConfig {
        config: Config::default(),
        provenance,
        warnings: Vec::new(),
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

fn apply_file(
    loaded: &mut LoadedConfig,
    mut file: ConfigFile,
    source: &ConfigSource,
    scope: FileScope,
) {
    if let Some(engine) = file.engine.take()
        && let Some(value) = engine.max_concurrent_sessions
    {
        loaded.config.engine.max_concurrent_sessions = value;
        set_source(loaded, "engine.max_concurrent_sessions", source);
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

fn apply_security_file_sections(
    loaded: &mut LoadedConfig,
    file: ConfigFile,
    source: &ConfigSource,
) {
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
    if let Some(permissions) = file.permissions
        && let Some(value) = permissions.default
    {
        loaded.config.permissions.default = value;
        set_source(loaded, "permissions.default", source);
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
        (ENV_MODEL_DEFAULT, "models.default"),
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
    match key {
        "engine.max_concurrent_sessions" => {
            loaded.config.engine.max_concurrent_sessions =
                value.parse().map_err(|_| ConfigError::CliOverride {
                    override_value: raw.to_owned(),
                    reason: "expected a positive integer".to_owned(),
                })?;
        }
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

    use super::{ConfigError, ConfigLoader, ConfigSource};

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
        ]);

        let loaded = ConfigLoader::new(user.clone(), project.clone())
            .with_environment(environment)
            .with_cli_overrides(vec!["engine.max_concurrent_sessions=11".to_owned()])
            .load()
            .expect("layered config should load");

        assert_eq!(loaded.config.engine.max_concurrent_sessions, 11);
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
}
