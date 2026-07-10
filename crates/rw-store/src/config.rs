//! Layered TOML configuration loading with per-leaf provenance.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use rw_types::config::{Config, ConfigFile, PermissionDecision, UpdateChannel};
use thiserror::Error;
use url::Url;

const ENV_ENGINE_SESSIONS: &str = "RW_ENGINE_MAX_CONCURRENT_SESSIONS";
const ENV_MODEL_DEFAULT: &str = "RW_MODEL_DEFAULT";
const ENV_NETWORK_PROXY: &str = "RW_NETWORK_PROXY";
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
        "network.proxy",
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
    if let Some(network) = file.network
        && let Some(value) = network.proxy
    {
        loaded.config.network.proxy = Some(value);
        set_source(loaded, "network.proxy", source);
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
    if let Some(proxy) = &config.network.proxy {
        let parsed = Url::parse(proxy).map_err(|_| {
            ConfigError::Validation(
                "network.proxy must be an absolute HTTP(S) URL without inline credentials"
                    .to_owned(),
            )
        })?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !matches!(parsed.path(), "" | "/")
        {
            return Err(ConfigError::Validation(
                "network.proxy must be an HTTP(S) origin without inline credentials, path, query, or fragment"
                    .to_owned(),
            ));
        }
    }
    Ok(())
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
    key.starts_with("models.aliases.")
        .then(|| provenance.get("models.aliases"))
        .flatten()
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
[network]
proxy = "http://user-proxy"
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
            loaded.config.network.proxy.as_deref(),
            Some("http://user-proxy")
        );
        assert_eq!(loaded.config.permissions.default, PermissionDecision::Allow);
        assert_eq!(loaded.config.sandbox.safe_list, ["git status"]);
        assert!(!loaded.config.telemetry.enabled);
        assert_eq!(loaded.config.updates.channel, UpdateChannel::Beta);
        assert_eq!(loaded.warnings().len(), 5);
        assert_eq!(
            loaded.provenance("engine.max_concurrent_sessions"),
            Some(&ConfigSource::Cli)
        );
        assert_eq!(
            loaded.provenance("models.aliases.plan"),
            Some(&ConfigSource::ProjectFile(project))
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
