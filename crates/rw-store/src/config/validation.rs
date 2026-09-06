use std::fs;
use std::path::Path;

use rw_types::config::{Config, ProviderConfig};
use url::{Host, Url};

use super::ConfigError;

pub(super) fn validate(config: &Config) -> Result<(), ConfigError> {
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

pub(super) fn valid_theme_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

pub(super) fn validate_ui(config: &Config) -> Result<(), ConfigError> {
    if valid_theme_name(&config.ui.theme) {
        Ok(())
    } else {
        Err(ConfigError::Validation(
            "ui.theme must be a safe theme name".to_owned(),
        ))
    }
}

pub(super) fn validate_tui_setting(
    config: &Config,
    key: &str,
    value: &str,
) -> Result<(), ConfigError> {
    match key {
        "budget.session_cost_cap_micros_usd" | "budget.daily_cost_cap_micros_usd" => {
            return parse_tui_budget_cap(key, value).map(|_| ());
        }
        "budget.warn_at_percent" => {
            return parse_tui_budget_warning(key, value).map(|_| ());
        }
        "budget.session_token_cap"
        | "budget.daily_token_cap"
        | "budget.token_rate_alarm_per_minute" => {
            return parse_tui_token_limit(key, value).map(|_| ());
        }
        _ => {}
    }
    let valid = match key {
        "ui.theme" => valid_theme_name(value),
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

pub(super) fn parse_tui_budget_cap(key: &str, value: &str) -> Result<Option<u64>, ConfigError> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("unlimited") {
        return Ok(None);
    }
    let mut parts = value.split('.');
    let dollars = parts.next().unwrap_or_default();
    let cents = parts.next();
    let valid_cents = cents.is_none_or(|fraction| {
        (1..=2).contains(&fraction.len()) && fraction.bytes().all(|byte| byte.is_ascii_digit())
    });
    if dollars.is_empty()
        || dollars.len() > 6
        || !dollars.bytes().all(|byte| byte.is_ascii_digit())
        || !valid_cents
        || parts.next().is_some()
    {
        return Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "budget cap must be \"unlimited\" or a dollar amount from 0.01 through 999999.99 with at most two decimal places".to_owned(),
        });
    }
    let dollars = dollars
        .parse::<u64>()
        .map_err(|_| ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "budget cap dollar amount is invalid".to_owned(),
        })?;
    let cents = match cents.unwrap_or_default().as_bytes() {
        [] => 0,
        [tenths] => u64::from(tenths - b'0') * 10,
        [tenths, hundredths] => u64::from(tenths - b'0') * 10 + u64::from(hundredths - b'0'),
        _ => unreachable!("validated cent precision"),
    };
    let micros = dollars * 1_000_000 + cents * 10_000;
    if micros == 0 {
        return Err(ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "budget cap must be greater than zero".to_owned(),
        });
    }
    Ok(Some(micros))
}

pub(super) fn parse_tui_budget_warning(key: &str, value: &str) -> Result<u8, ConfigError> {
    let value = value.trim();
    let valid_integer = !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    let percent = valid_integer
        .then(|| value.parse::<u8>().ok())
        .flatten()
        .filter(|percent| (1..=100).contains(percent));
    percent.ok_or_else(|| ConfigError::InvalidUserSetting {
        key: key.to_owned(),
        reason: "warning threshold must be an integer from 1 through 100".to_owned(),
    })
}

pub(super) fn parse_tui_token_limit(key: &str, value: &str) -> Result<Option<u64>, ConfigError> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("unlimited") {
        return Ok(None);
    }
    let tokens = (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse::<u64>().ok())
        .flatten()
        .filter(|tokens| *tokens > 0 && i64::try_from(*tokens).is_ok());
    tokens
        .map(Some)
        .ok_or_else(|| ConfigError::InvalidUserSetting {
            key: key.to_owned(),
            reason: "token limit must be \"unlimited\" or a positive whole number".to_owned(),
        })
}

pub(super) fn validate_toolchain(
    config: &rw_types::config::ToolchainConfig,
) -> Result<(), ConfigError> {
    if !rw_types::config::valid_toolchain_runtime_read_roots(&config.runtime_read_roots) {
        return Err(ConfigError::Validation(format!(
            "toolchain.runtime_read_roots requires at most {} absolute UTF-8 paths of at most {} bytes",
            rw_types::config::MAX_TOOLCHAIN_RUNTIME_READ_ROOTS,
            rw_types::config::MAX_TOOLCHAIN_RUNTIME_ROOT_BYTES,
        )));
    }
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

pub(super) fn validate_websearch(
    config: &rw_types::config::WebSearchConfig,
) -> Result<(), ConfigError> {
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

pub(super) fn is_http_token_byte(byte: u8) -> bool {
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

pub(super) fn invalid_toolchain_command(command: &str) -> bool {
    command.trim().is_empty()
        || command
            .chars()
            .any(|character| character == '\0' || matches!(character, '\n' | '\r'))
}

pub(super) fn validate_provider(name: &str, provider: &ProviderConfig) -> Result<(), ConfigError> {
    if name.trim().is_empty() || provider.kind.trim().is_empty() {
        return Err(ConfigError::Validation(format!(
            "provider {name:?} must have a non-empty name and kind"
        )));
    }
    provider
        .validate_gateway_options()
        .map_err(|message| ConfigError::Validation(format!("providers.{name}: {message}")))?;
    provider
        .validate_pricing()
        .map_err(|message| ConfigError::Validation(format!("providers.{name}: {message}")))?;
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

pub(super) fn validate_provider_oauth(
    name: &str,
    provider: &ProviderConfig,
) -> Result<(), ConfigError> {
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

pub(super) fn validate_remote_endpoint(key: &str, value: &str) -> Result<(), ConfigError> {
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

pub(super) fn is_loopback_endpoint(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

pub(super) fn validate_proxy(key: &str, proxy: &str) -> Result<(), ConfigError> {
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

pub(super) fn validate_proxy_authentication(
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

pub(super) fn valid_environment_name(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

pub(super) fn paths_collide(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}
