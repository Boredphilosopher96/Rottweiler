use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::path::Path;

use rw_types::config::{Config, ConfigFile, EngineConfigFile, ProviderConfig};

use super::{
    ConfigError, ConfigSource, ConfigWarning, ENV_BUDGET_CREDIT_RATE, ENV_BUDGET_DAILY_COST_CAP,
    ENV_BUDGET_DAILY_CREDIT_CAP, ENV_BUDGET_DAILY_TOKEN_CAP, ENV_BUDGET_SESSION_COST_CAP,
    ENV_BUDGET_SESSION_CREDIT_CAP, ENV_BUDGET_SESSION_TOKEN_CAP, ENV_BUDGET_SPEND_RATE,
    ENV_BUDGET_TOKEN_RATE, ENV_BUDGET_WARN_PERCENT, ENV_COMPACTION_AUTO,
    ENV_COMPACTION_MODEL_ALIAS, ENV_COMPACTION_RESERVED, ENV_ENGINE_SESSIONS, ENV_MODEL_DEFAULT,
    ENV_NETWORK_PROXY, ENV_NETWORK_PROXY_PASSWORD_CREDENTIAL, ENV_NETWORK_PROXY_USERNAME,
    ENV_PERMISSION_DEFAULT, ENV_SANDBOX_SAFE_LIST, ENV_SUBAGENT_CONCURRENCY, ENV_SUBAGENT_DEPTH,
    ENV_TELEMETRY_ENABLED, ENV_UPDATE_CHANNEL, LoadedConfig, override_reason, quoted,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileScope {
    User,
    Project,
}

pub(super) fn defaults_with_provenance() -> LoadedConfig {
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
        "budget.session_token_cap",
        "budget.daily_token_cap",
        "budget.spend_rate_alarm_micros_usd_per_minute",
        "budget.ai_credit_rate_alarm_micros_per_minute",
        "budget.token_rate_alarm_per_minute",
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
        "toolchain.runtime_read_roots",
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

pub(super) fn read_file(path: &Path) -> Result<Option<ConfigFile>, ConfigError> {
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

pub(super) fn read_assessed_project_file(
    path: &Path,
    assessment: &crate::trust::FolderTrustAssessment,
) -> Result<Option<ConfigFile>, ConfigError> {
    let parent = path
        .parent()
        .ok_or_else(|| ConfigError::ProjectChangedDuringLoad(path.to_owned()))?;
    let canonical_parent = canonical_config_parent(parent)
        .map_err(|_| ConfigError::ProjectChangedDuringLoad(path.to_owned()))?;
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

pub(super) fn read_project_bytes(path: &Path) -> Result<Option<Vec<u8>>, ConfigError> {
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

pub(super) fn apply_file(
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
        apply_budget!(session_token_cap);
        apply_budget!(daily_token_cap);
        apply_budget!(spend_rate_alarm_micros_usd_per_minute);
        apply_budget!(ai_credit_rate_alarm_micros_per_minute);
        apply_budget!(token_rate_alarm_per_minute);
        if let Some(value) = budget.warn_at_percent {
            loaded.config.budget.warn_at_percent = value;
            set_source(loaded, "budget.warn_at_percent", source);
        }
    }
    if let Some(toolchain) = file.toolchain.take() {
        loaded.config.toolchain = toolchain;
        for key in [
            "toolchain.runtime_read_roots",
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

pub(super) fn apply_engine_file(
    loaded: &mut LoadedConfig,
    engine: &EngineConfigFile,
    source: &ConfigSource,
) {
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

pub(super) fn apply_security_file_sections(
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

pub(super) fn warn_ignored_project_sections(
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

pub(super) fn apply_environment(
    loaded: &mut LoadedConfig,
    environment: &BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    for (name, key) in [
        (ENV_ENGINE_SESSIONS, "engine.max_concurrent_sessions"),
        (ENV_SUBAGENT_DEPTH, "engine.subagent_max_depth"),
        (ENV_SUBAGENT_CONCURRENCY, "engine.subagent_max_concurrency"),
        (ENV_MODEL_DEFAULT, "models.default"),
        (ENV_COMPACTION_AUTO, "compaction.auto"),
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
        (ENV_BUDGET_SESSION_TOKEN_CAP, "budget.session_token_cap"),
        (ENV_BUDGET_DAILY_TOKEN_CAP, "budget.daily_token_cap"),
        (
            ENV_BUDGET_SPEND_RATE,
            "budget.spend_rate_alarm_micros_usd_per_minute",
        ),
        (
            ENV_BUDGET_CREDIT_RATE,
            "budget.ai_credit_rate_alarm_micros_per_minute",
        ),
        (ENV_BUDGET_TOKEN_RATE, "budget.token_rate_alarm_per_minute"),
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

pub(super) fn apply_override(
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
                value.parse().map_err(|_| ConfigError::CliOverride {
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
                value.parse().map_err(|_| ConfigError::CliOverride {
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
            let level = value.parse().map_err(|_| ConfigError::CliOverride {
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

pub(super) fn apply_engine_override(
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

pub(super) fn apply_m3_override(
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
        "budget.session_token_cap" => {
            loaded.config.budget.session_token_cap = parse_optional_u64(value, raw)?;
        }
        "budget.daily_token_cap" => {
            loaded.config.budget.daily_token_cap = parse_optional_u64(value, raw)?;
        }
        "budget.spend_rate_alarm_micros_usd_per_minute" => {
            loaded.config.budget.spend_rate_alarm_micros_usd_per_minute =
                parse_optional_u64(value, raw)?;
        }
        "budget.ai_credit_rate_alarm_micros_per_minute" => {
            loaded.config.budget.ai_credit_rate_alarm_micros_per_minute =
                parse_optional_u64(value, raw)?;
        }
        "budget.token_rate_alarm_per_minute" => {
            loaded.config.budget.token_rate_alarm_per_minute = parse_optional_u64(value, raw)?;
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

pub(super) fn set_source(loaded: &mut LoadedConfig, key: &str, source: &ConfigSource) {
    loaded.provenance.insert(key.to_owned(), source.clone());
}

pub(super) fn apply_provider_override(
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

pub(super) fn nonempty_override(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_owned())
}

pub(super) fn optional_string(value: &str) -> Option<String> {
    (!value.trim().is_empty() && !matches!(value.trim(), "none" | "unset"))
        .then(|| value.to_owned())
}

pub(super) fn parse_optional_u64(value: &str, raw: &str) -> Result<Option<u64>, ConfigError> {
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

pub(super) fn set_provider_sources(
    loaded: &mut LoadedConfig,
    name: &str,
    provider: &ProviderConfig,
    source: &ConfigSource,
) {
    set_source(loaded, &format!("providers.{name}.kind"), source);
    for (present, field) in [
        (provider.base_url.is_some(), "base_url"),
        (provider.path_template.is_some(), "path_template"),
        (!provider.headers.is_empty(), "headers"),
        (
            !provider.header_credentials.is_empty(),
            "header_credentials",
        ),
        (provider.auth_scheme.is_some(), "auth_scheme"),
        (!provider.extra_query.is_empty(), "extra_query"),
        (!provider.extra_body.is_empty(), "extra_body"),
        (!provider.model_ids.is_empty(), "model_ids"),
        (!provider.pricing.is_empty(), "pricing"),
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
    for model in provider.pricing.keys() {
        set_source(
            loaded,
            &format!("providers.{name}.pricing.{}", quoted(model)),
            source,
        );
    }
}

pub(super) fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(super) fn nonempty_value<'a>(
    environment: &'a BTreeMap<String, String>,
    key: &str,
) -> Option<&'a str> {
    environment
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
}

/// Resolve existing ancestors before appending absent configuration directories.
/// A missing .rottweiler directory must retain the workspace's canonical identity.
fn canonical_config_parent(parent: &Path) -> std::io::Result<std::path::PathBuf> {
    let mut missing = Vec::new();
    let mut existing = parent;
    loop {
        match fs::canonicalize(existing) {
            Ok(mut canonical) => {
                for component in missing.into_iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if fs::symlink_metadata(existing).is_ok() {
                    return Err(error);
                }
                missing.push(
                    existing
                        .file_name()
                        .ok_or_else(|| std::io::Error::other("invalid configuration parent"))?,
                );
                existing = existing.parent().ok_or_else(|| {
                    std::io::Error::other("configuration parent has no existing ancestor")
                })?;
            }
            Err(error) => return Err(error),
        }
    }
}
