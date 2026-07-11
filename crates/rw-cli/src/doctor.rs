//! Bounded, non-destructive installation and provider diagnostics.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{IsTerminal as _, Read as _},
    path::Path,
    time::Duration,
};

use rw_core::{Config, ProviderConfig, default_provider_api_key_credential_id};
use rw_store::{
    config::ConfigLoader,
    credentials::{
        CredentialEnvironment, CredentialInventoryItem, CredentialKeychain, CredentialManager,
        CredentialReference, CredentialSource,
    },
};
use serde::Serialize;
use url::Url;

const DOCTOR_SCHEMA_VERSION: u16 = 1;
const MAX_DOCTOR_PROVIDERS: usize = 128;
const MIN_NETWORK_TIMEOUT_MS: u64 = 250;
const MAX_NETWORK_TIMEOUT_MS: u64 = 10_000;
const MAX_OS_RELEASE_BYTES: u64 = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DoctorOptions {
    pub(crate) network: bool,
    pub(crate) timeout_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckStatus {
    Pass,
    Warning,
    Fail,
    Skipped,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct DoctorCheck {
    pub(crate) id: String,
    pub(crate) status: CheckStatus,
    pub(crate) code: String,
    pub(crate) summary: String,
    pub(crate) details: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct DoctorReport {
    pub(crate) schema_version: u16,
    pub(crate) healthy: bool,
    pub(crate) network_probes_requested: bool,
    pub(crate) checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    #[must_use]
    pub(crate) fn has_failures(&self) -> bool {
        !self.healthy
    }
}

struct DoctorSecret(String);

impl std::fmt::Debug for DoctorSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DoctorSecret([REDACTED])")
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ReferenceKey {
    identifier: String,
    environment: Option<String>,
}

impl ReferenceKey {
    fn reference(&self) -> CredentialReference {
        let reference = CredentialReference::new(self.identifier.clone());
        self.environment
            .as_ref()
            .map_or(reference.clone(), |variable| {
                reference.with_environment(variable.clone())
            })
    }
}

#[derive(Debug)]
enum InventoryValue {
    Present {
        source: &'static str,
        secret: DoctorSecret,
    },
    Missing,
    Unavailable,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthScheme {
    Bearer,
    AnthropicApiKey,
    OpaqueBundle,
    None,
}

#[derive(Clone, Debug)]
struct ProviderPlan {
    name: String,
    kind: String,
    endpoint: Option<Url>,
    auth: Option<ReferenceKey>,
    auth_scheme: AuthScheme,
    refresh: Option<ReferenceKey>,
    proxy: Option<Url>,
    proxy_username: Option<String>,
    proxy_password: Option<ReferenceKey>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Reachability {
    Reachable(u16),
    ReachableAuthUnverified(u16),
    EndpointResponseUnexpected(u16),
    RateLimited(u16),
    ServiceUnavailable(u16),
    CredentialRejected(u16),
    RefreshRequired(u16),
    ProxyCredentialRejected,
    Unreachable,
}

pub(crate) async fn collect(options: DoctorOptions) -> DoctorReport {
    let mut checks = Vec::new();
    let timeout_ms = options
        .timeout_ms
        .clamp(MIN_NETWORK_TIMEOUT_MS, MAX_NETWORK_TIMEOUT_MS);
    let Ok(config_loader) = ConfigLoader::from_environment() else {
        checks.push(check(
            "config",
            CheckStatus::Fail,
            "config_discovery_failed",
            "configuration paths could not be discovered",
        ));
        append_local_checks(&mut checks);
        return finish_report(options.network, checks);
    };
    let credentials_path = config_loader.credentials_path();
    append_runtime_path_checks(&mut checks, &credentials_path);
    append_local_checks(&mut checks);
    let Ok(effective) = config_loader.load() else {
        checks.push(check(
            "config",
            CheckStatus::Fail,
            "config_invalid",
            "effective configuration is invalid; run `rw config check` for sanitized details",
        ));
        return finish_report(options.network, checks);
    };
    let warning_count = effective.warnings().len();
    let mut config_check = check(
        "config",
        if warning_count == 0 {
            CheckStatus::Pass
        } else {
            CheckStatus::Warning
        },
        if warning_count == 0 {
            "config_valid"
        } else {
            "config_valid_with_warnings"
        },
        if warning_count == 0 {
            "effective configuration is valid"
        } else {
            "effective configuration is valid with security warnings"
        },
    );
    config_check
        .details
        .insert("warnings".to_owned(), warning_count.to_string());
    checks.push(config_check);

    if effective.config.providers.len() > MAX_DOCTOR_PROVIDERS {
        checks.push(check(
            "providers",
            CheckStatus::Fail,
            "provider_limit_exceeded",
            "provider diagnostics exceed the bounded provider limit",
        ));
        return finish_report(options.network, checks);
    }
    let plans = provider_plans(&effective.config);
    let references = plans
        .iter()
        .flat_map(|plan| {
            [&plan.auth, &plan.refresh, &plan.proxy_password]
                .into_iter()
                .filter_map(Clone::clone)
        })
        .collect::<BTreeSet<_>>();
    // One manager and one inventory function are used for the entire command.
    // The manager's process cache guarantees at most one vault read/keychain prompt.
    let inventory = inventory_credentials(&credentials_path, references);
    append_provider_checks(&mut checks, &plans, &inventory, options.network, timeout_ms).await;
    finish_report(options.network, checks)
}

fn finish_report(network: bool, checks: Vec<DoctorCheck>) -> DoctorReport {
    let healthy = checks.iter().all(|item| item.status != CheckStatus::Fail);
    DoctorReport {
        schema_version: DOCTOR_SCHEMA_VERSION,
        healthy,
        network_probes_requested: network,
        checks,
    }
}

fn check(
    id: impl Into<String>,
    status: CheckStatus,
    code: impl Into<String>,
    summary: impl Into<String>,
) -> DoctorCheck {
    DoctorCheck {
        id: id.into(),
        status,
        code: code.into(),
        summary: summary.into(),
        details: BTreeMap::new(),
    }
}

fn append_runtime_path_checks(checks: &mut Vec<DoctorCheck>, credentials_path: &Path) {
    let config_root = credentials_path.parent();
    let executable = std::env::current_exe().ok();
    let workspace = std::env::current_dir().ok();
    checks.push(runtime_path_check(
        config_root,
        executable.as_deref(),
        workspace.as_deref(),
    ));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigRootState {
    Ready,
    Creatable,
    Unavailable,
}

fn runtime_path_check(
    config_root: Option<&Path>,
    executable: Option<&Path>,
    workspace: Option<&Path>,
) -> DoctorCheck {
    let root_state = config_root_state(config_root);
    let executable_ok = executable.as_ref().is_some_and(|path| path.is_file());
    let workspace_ok = workspace.as_ref().is_some_and(|path| path.is_dir());
    let runtime_ok = executable_ok && workspace_ok;
    let (status, code, summary) = match (runtime_ok, root_state) {
        (true, ConfigRootState::Ready) => (
            CheckStatus::Pass,
            "runtime_paths_valid",
            "runtime, configuration, and workspace paths are available",
        ),
        (true, ConfigRootState::Creatable) => (
            CheckStatus::Warning,
            "config_root_not_created",
            "the configuration root does not exist yet but can be created on first use",
        ),
        _ => (
            CheckStatus::Fail,
            "runtime_path_invalid",
            "a required runtime, configuration, or workspace path is unavailable",
        ),
    };
    let mut item = check("runtime_paths", status, code, summary);
    item.details.insert(
        "config_root".to_owned(),
        config_root.map_or_else(
            || "unavailable".to_owned(),
            |path| path.display().to_string(),
        ),
    );
    item.details.insert(
        "executable".to_owned(),
        executable.map_or_else(
            || "unavailable".to_owned(),
            |path| path.display().to_string(),
        ),
    );
    item.details.insert(
        "workspace".to_owned(),
        workspace.map_or_else(
            || "unavailable".to_owned(),
            |path| path.display().to_string(),
        ),
    );
    item.details.insert(
        "config_root_state".to_owned(),
        match root_state {
            ConfigRootState::Ready => "ready",
            ConfigRootState::Creatable => "creatable",
            ConfigRootState::Unavailable => "unavailable",
        }
        .to_owned(),
    );
    item
}

fn config_root_state(config_root: Option<&Path>) -> ConfigRootState {
    let Some(config_root) = config_root else {
        return ConfigRootState::Unavailable;
    };
    match std::fs::metadata(config_root) {
        Ok(metadata) if metadata.is_dir() => ConfigRootState::Ready,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => config_root
            .ancestors()
            .skip(1)
            .find_map(|ancestor| std::fs::metadata(ancestor).ok())
            .filter(|metadata| metadata.is_dir() && !metadata.permissions().readonly())
            .map_or(ConfigRootState::Unavailable, |_| ConfigRootState::Creatable),
        Ok(_) | Err(_) => ConfigRootState::Unavailable,
    }
}

fn append_local_checks(checks: &mut Vec<DoctorCheck>) {
    checks.push(os_check());
    checks.push(terminal_check_from(
        std::env::var("TERM").ok().as_deref(),
        std::env::var("COLORTERM").ok().as_deref(),
        std::io::stdout().is_terminal(),
    ));
    let sandbox = rw_core::runtime_support::probe_sandbox();
    checks.push(sandbox_check(
        sandbox.support == rw_core::runtime_support::SandboxSupport::Enforced,
        sandbox.backend,
    ));
    let egress = rw_core::runtime_support::probe_policy_egress();
    checks.push(check(
        "sandbox_egress",
        if egress.support == rw_core::runtime_support::SandboxSupport::Enforced {
            CheckStatus::Pass
        } else {
            CheckStatus::Warning
        },
        if egress.support == rw_core::runtime_support::SandboxSupport::Enforced {
            "policy_egress_available"
        } else {
            "policy_egress_unavailable"
        },
        if egress.support == rw_core::runtime_support::SandboxSupport::Enforced {
            "sandbox policy-egress enforcement is available"
        } else {
            "sandbox policy-egress enforcement is unavailable; networked commands fail closed"
        },
    ));
}

fn os_check() -> DoctorCheck {
    let wsl = std::env::var_os("WSL_INTEROP").is_some()
        || std::env::var_os("WSL_DISTRO_NAME").is_some()
        || bounded_os_release_contains("microsoft");
    os_check_from(std::env::consts::OS, std::env::consts::ARCH, wsl)
}

fn os_check_from(os: &str, arch: &str, wsl: bool) -> DoctorCheck {
    let supported_os = matches!(os, "macos" | "linux");
    let supported_arch = matches!(arch, "x86_64" | "aarch64");
    let wsl = wsl && os == "linux";
    let (status, code, summary) = if !supported_os {
        (
            CheckStatus::Fail,
            "os_unsupported",
            "native operating system is unsupported; use macOS, Linux, or WSL",
        )
    } else if !supported_arch {
        (
            CheckStatus::Warning,
            "architecture_unverified",
            "operating system is supported, but this architecture is not in the verified matrix",
        )
    } else if wsl {
        (
            CheckStatus::Pass,
            "wsl_detected",
            "Linux under WSL was detected",
        )
    } else {
        (
            CheckStatus::Pass,
            "os_supported",
            "operating system and architecture are supported",
        )
    };
    let mut item = check("os", status, code, summary);
    item.details.insert("os".to_owned(), os.to_owned());
    item.details.insert("arch".to_owned(), arch.to_owned());
    item.details.insert("wsl".to_owned(), wsl.to_string());
    item
}

fn bounded_os_release_contains(needle: &str) -> bool {
    let Ok(file) = std::fs::File::open("/proc/sys/kernel/osrelease") else {
        return false;
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_OS_RELEASE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
        || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_OS_RELEASE_BYTES
    {
        return false;
    }
    String::from_utf8_lossy(&bytes)
        .to_ascii_lowercase()
        .contains(needle)
}

fn terminal_check_from(term: Option<&str>, colorterm: Option<&str>, tty: bool) -> DoctorCheck {
    let normalized = term.unwrap_or_default().trim().to_ascii_lowercase();
    let dumb = normalized == "dumb";
    let missing = normalized.is_empty();
    let (status, code, summary) = if dumb {
        (
            CheckStatus::Fail,
            "terminal_dumb",
            "TERM=dumb cannot support the interactive TUI",
        )
    } else if missing || !tty {
        (
            CheckStatus::Warning,
            "terminal_noninteractive",
            "terminal capabilities are limited or output is not attached to a TTY",
        )
    } else {
        (
            CheckStatus::Pass,
            "terminal_capable",
            "terminal supports interactive rendering",
        )
    };
    let mut item = check("terminal", status, code, summary);
    item.details.insert(
        "term".to_owned(),
        if missing {
            "unset"
        } else {
            normalized.as_str()
        }
        .to_owned(),
    );
    item.details.insert(
        "color".to_owned(),
        color_capability(&normalized, colorterm).to_owned(),
    );
    item.details.insert("tty".to_owned(), tty.to_string());
    item
}

fn color_capability(term: &str, colorterm: Option<&str>) -> &'static str {
    let colorterm = colorterm.unwrap_or_default().to_ascii_lowercase();
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        "truecolor"
    } else if term.contains("256color") {
        "256"
    } else if term.is_empty() || term == "dumb" {
        "none"
    } else {
        "basic"
    }
}

fn sandbox_check(available: bool, backend: &str) -> DoctorCheck {
    let mut item = check(
        "sandbox",
        if available {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        if available {
            "sandbox_available"
        } else {
            "sandbox_unavailable"
        },
        if available {
            "native command sandbox enforcement is available"
        } else {
            "native command sandbox enforcement is unavailable; mutating commands require strict prompting"
        },
    );
    item.details
        .insert("backend".to_owned(), backend.to_owned());
    item
}

fn provider_plans(config: &Config) -> Vec<ProviderPlan> {
    let settings = rw_core::runtime_support::ProxySettings {
        global: config
            .network
            .proxy
            .as_deref()
            .and_then(|value| Url::parse(value).ok()),
        per_provider: config
            .providers
            .iter()
            .filter_map(|(name, provider)| {
                provider
                    .proxy
                    .as_deref()
                    .and_then(|value| Url::parse(value).ok())
                    .map(|url| (name.clone(), url))
            })
            .collect(),
        environment: rw_core::runtime_support::ProxyEnvironment::capture(),
    };
    config
        .providers
        .iter()
        .map(|(name, provider)| {
            let mut plan = provider_plan(name, provider, config);
            if let Some(endpoint) = plan.endpoint.as_ref() {
                if let Some(resolution) = settings.resolve(name, endpoint) {
                    plan.proxy = Some(resolution.url);
                    if resolution.source == rw_core::runtime_support::ProxySource::Environment {
                        plan.proxy_username = None;
                        plan.proxy_password = None;
                    }
                } else {
                    plan.proxy = None;
                    plan.proxy_username = None;
                    plan.proxy_password = None;
                }
            }
            plan
        })
        .collect()
}

fn provider_plan(name: &str, provider: &ProviderConfig, config: &Config) -> ProviderPlan {
    let endpoint = provider_endpoint(provider);
    let oauth = oauth_configured(provider);
    let (auth, auth_scheme, refresh) = match provider.kind.as_str() {
        "openai_codex" | "openai_subscription" => (
            Some(reference_key(
                format!("providers.{name}.openai_subscription"),
                None,
            )),
            AuthScheme::OpaqueBundle,
            None,
        ),
        "github_copilot" => (
            Some(reference_key(
                format!("providers.{name}.github_copilot"),
                None,
            )),
            AuthScheme::OpaqueBundle,
            None,
        ),
        _ if oauth => {
            let access = provider
                .oauth_access_token_credential
                .clone()
                .unwrap_or_else(|| format!("providers.{name}.oauth.access_token"));
            let refresh = (provider.oauth_refresh_token_credential.is_some()
                || (provider.oauth_token_env.is_none()
                    && provider.oauth_token_endpoint.is_some()
                    && provider.oauth_client_id.is_some()))
            .then(|| {
                provider
                    .oauth_refresh_token_credential
                    .clone()
                    .unwrap_or_else(|| format!("providers.{name}.oauth.refresh_token"))
            });
            (
                Some(reference_key(access, provider.oauth_token_env.clone())),
                AuthScheme::Bearer,
                refresh.map(|identifier| reference_key(identifier, None)),
            )
        }
        _ if provider_requires_api_key(provider, endpoint.as_ref()) => {
            let identifier = provider.api_key_credential.clone().unwrap_or_else(|| {
                default_provider_api_key_credential_id(name)
                    .unwrap_or_else(|_| format!("providers.{name}.api_key"))
            });
            let environment =
                provider
                    .api_key_env
                    .clone()
                    .or_else(|| match provider.kind.as_str() {
                        "anthropic" => Some("ANTHROPIC_API_KEY".to_owned()),
                        "openai" | "openai_responses" | "openai_chat" => {
                            Some("OPENAI_API_KEY".to_owned())
                        }
                        _ => None,
                    });
            (
                Some(reference_key(identifier, environment)),
                if provider.kind == "anthropic" {
                    AuthScheme::AnthropicApiKey
                } else {
                    AuthScheme::Bearer
                },
                None,
            )
        }
        _ => (None, AuthScheme::None, None),
    };
    let proxy_value = provider.proxy.as_ref().or(config.network.proxy.as_ref());
    let proxy_password_id = provider
        .proxy_password_credential
        .as_ref()
        .or(config.network.proxy_password_credential.as_ref());
    let proxy_username = provider
        .proxy_username
        .clone()
        .or_else(|| config.network.proxy_username.clone());
    ProviderPlan {
        name: name.to_owned(),
        kind: provider.kind.clone(),
        endpoint,
        auth,
        auth_scheme,
        refresh,
        proxy: proxy_value.and_then(|value| Url::parse(value).ok()),
        proxy_username,
        proxy_password: proxy_password_id.map(|value| reference_key(value.clone(), None)),
    }
}

fn reference_key(identifier: String, environment: Option<String>) -> ReferenceKey {
    ReferenceKey {
        identifier,
        environment,
    }
}

fn oauth_configured(provider: &ProviderConfig) -> bool {
    provider.oauth_token_env.is_some()
        || provider.oauth_authorization_endpoint.is_some()
        || provider.oauth_token_endpoint.is_some()
        || provider.oauth_client_id.is_some()
        || provider.oauth_access_token_credential.is_some()
        || provider.oauth_refresh_token_credential.is_some()
        || !provider.oauth_scopes.is_empty()
}

fn provider_requires_api_key(provider: &ProviderConfig, endpoint: Option<&Url>) -> bool {
    provider.api_key_env.is_some()
        || provider.api_key_credential.is_some()
        || matches!(
            provider.kind.as_str(),
            "anthropic" | "openai" | "openai_responses" | "openai_chat"
        ) && endpoint.is_some_and(|value| !url_is_loopback(value))
}

fn url_is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn provider_endpoint(provider: &ProviderConfig) -> Option<Url> {
    let value = provider
        .base_url
        .as_deref()
        .or(match provider.kind.as_str() {
            "anthropic" => Some("https://api.anthropic.com/v1/messages"),
            "openai" | "openai_responses" => Some("https://api.openai.com/v1/responses"),
            "openai_chat" => Some("https://api.openai.com/v1/chat/completions"),
            "openai_codex" | "openai_subscription" => {
                Some("https://chatgpt.com/backend-api/codex/responses")
            }
            "github_copilot" => Some("https://api.githubcopilot.com"),
            _ => None,
        })?;
    Url::parse(value).ok()
}

fn inventory_credentials(
    credentials_path: &Path,
    references: BTreeSet<ReferenceKey>,
) -> BTreeMap<ReferenceKey, InventoryValue> {
    let manager = CredentialManager::system(credentials_path);
    inventory_credentials_with_manager(&manager, references)
}

fn inventory_credentials_with_manager<E, K>(
    manager: &CredentialManager<E, K>,
    references: BTreeSet<ReferenceKey>,
) -> BTreeMap<ReferenceKey, InventoryValue>
where
    E: CredentialEnvironment,
    K: CredentialKeychain,
{
    let keys = references.into_iter().collect::<Vec<_>>();
    let resolved =
        manager.resolve_inventory(&keys.iter().map(ReferenceKey::reference).collect::<Vec<_>>());
    match resolved {
        Ok(values) => keys
            .into_iter()
            .zip(values)
            .map(|(key, value)| {
                let value = match value {
                    CredentialInventoryItem::Present(resolved) => InventoryValue::Present {
                        source: credential_source_name(resolved.source()),
                        secret: DoctorSecret(resolved.secret().expose_secret().clone()),
                    },
                    CredentialInventoryItem::Missing => InventoryValue::Missing,
                    CredentialInventoryItem::StoreUnavailable => InventoryValue::Unavailable,
                };
                (key, value)
            })
            .collect(),
        Err(_) => keys
            .into_iter()
            .map(|key| (key, InventoryValue::Invalid))
            .collect(),
    }
}

fn credential_source_name(source: &CredentialSource) -> &'static str {
    match source {
        CredentialSource::Environment(_) => "environment",
        CredentialSource::OsKeychain => "os_keychain",
        CredentialSource::FallbackFile(_) => "fallback_file",
    }
}

async fn append_provider_checks(
    checks: &mut Vec<DoctorCheck>,
    plans: &[ProviderPlan],
    inventory: &BTreeMap<ReferenceKey, InventoryValue>,
    network: bool,
    timeout_ms: u64,
) {
    for plan in plans {
        checks.push(provider_auth_check(plan, inventory));
        if let Some(proxy_password) = plan.proxy_password.as_ref() {
            checks.push(credential_presence_check(
                format!("provider.{}.proxy_auth", plan.name),
                proxy_password,
                inventory,
                "proxy credential",
            ));
        }
        if !network {
            checks.push(check(
                format!("provider.{}.reachability", plan.name),
                CheckStatus::Skipped,
                "network_probe_not_requested",
                "provider reachability was not probed; pass `--network` to opt in",
            ));
            continue;
        }
        let reachability = probe_provider(plan, inventory, timeout_ms).await;
        checks.push(reachability_check(&plan.name, reachability, timeout_ms));
    }
}

fn provider_auth_check(
    plan: &ProviderPlan,
    inventory: &BTreeMap<ReferenceKey, InventoryValue>,
) -> DoctorCheck {
    let id = format!("provider.{}.auth", plan.name);
    let Some(reference) = plan.auth.as_ref() else {
        return check(
            id,
            CheckStatus::Pass,
            "credential_not_required",
            "provider is configured for an authenticated loopback or credential-free route",
        );
    };
    let value = inventory.get(reference);
    let refresh_value = plan
        .refresh
        .as_ref()
        .and_then(|refresh| inventory.get(refresh));
    let malformed_bundle = matches!(plan.auth_scheme, AuthScheme::OpaqueBundle)
        && value.is_some_and(|value| match value {
            InventoryValue::Present { secret, .. } => {
                rw_core::validate_stored_provider_credential(&plan.kind, &secret.0).is_err()
            }
            _ => false,
        });
    let (status, code, summary) = if malformed_bundle {
        (
            CheckStatus::Fail,
            "credential_invalid",
            "provider credential bundle is malformed",
        )
    } else {
        match value {
            Some(InventoryValue::Present { .. }) => (
                CheckStatus::Pass,
                "credential_present",
                "provider credential is present",
            ),
            Some(InventoryValue::Missing) | None
                if matches!(refresh_value, Some(InventoryValue::Present { .. })) =>
            {
                (
                    CheckStatus::Pass,
                    "refresh_credential_present",
                    "provider refresh credential is present",
                )
            }
            Some(InventoryValue::Missing) | None => (
                CheckStatus::Fail,
                "credential_missing",
                "provider credential is missing",
            ),
            Some(InventoryValue::Unavailable) => (
                CheckStatus::Fail,
                "credential_store_unavailable",
                "provider credential store is unavailable",
            ),
            Some(InventoryValue::Invalid) => (
                CheckStatus::Fail,
                "credential_store_invalid",
                "provider credential inventory is malformed or unsafe",
            ),
        }
    };
    let mut item = check(id, status, code, summary);
    if let Some(InventoryValue::Present { source, .. }) = value {
        item.details
            .insert("source".to_owned(), (*source).to_owned());
    }
    item.details.insert("kind".to_owned(), plan.kind.clone());
    item
}

fn credential_presence_check(
    id: String,
    reference: &ReferenceKey,
    inventory: &BTreeMap<ReferenceKey, InventoryValue>,
    label: &str,
) -> DoctorCheck {
    let (status, code, summary) = match inventory.get(reference) {
        Some(InventoryValue::Present { .. }) => (
            CheckStatus::Pass,
            "credential_present",
            format!("{label} is present"),
        ),
        Some(InventoryValue::Missing) | None => (
            CheckStatus::Fail,
            "credential_missing",
            format!("{label} is missing"),
        ),
        Some(InventoryValue::Unavailable) => (
            CheckStatus::Fail,
            "credential_store_unavailable",
            format!("{label} store is unavailable"),
        ),
        Some(InventoryValue::Invalid) => (
            CheckStatus::Fail,
            "credential_store_invalid",
            format!("{label} inventory is malformed or unsafe"),
        ),
    };
    check(id, status, code, summary)
}

async fn probe_provider(
    plan: &ProviderPlan,
    inventory: &BTreeMap<ReferenceKey, InventoryValue>,
    timeout_ms: u64,
) -> Reachability {
    let Some(endpoint) = plan.endpoint.clone() else {
        return Reachability::Unreachable;
    };
    let mut headers = Vec::new();
    if let Some(reference) = plan.auth.as_ref()
        && let Some(InventoryValue::Present { secret, .. }) = inventory.get(reference)
    {
        match plan.auth_scheme {
            AuthScheme::Bearer => {
                headers.push(("authorization".to_owned(), format!("Bearer {}", secret.0)));
            }
            AuthScheme::AnthropicApiKey => {
                headers.push(("x-api-key".to_owned(), secret.0.clone()));
            }
            AuthScheme::OpaqueBundle | AuthScheme::None => {}
        }
    }
    let proxy_authentication = if let (Some(username), Some(password_key)) =
        (plan.proxy_username.as_deref(), plan.proxy_password.as_ref())
        && let Some(InventoryValue::Present { secret, .. }) = inventory.get(password_key)
    {
        Some(rw_core::runtime_support::ProxyAuthentication::new(
            username,
            rw_core::runtime_support::ProviderSecret::new(secret.0.clone()),
        ))
    } else {
        None
    };
    let Ok(status) = rw_core::runtime_support::provider_reachability_probe(
        rw_core::runtime_support::ProviderReachabilityRequest {
            url: endpoint,
            headers,
            proxy: plan.proxy.clone(),
            proxy_authentication,
            timeout: Duration::from_millis(timeout_ms),
        },
    )
    .await
    else {
        return Reachability::Unreachable;
    };
    classify_reachability(status, plan, inventory)
}

fn classify_reachability(
    status: u16,
    plan: &ProviderPlan,
    inventory: &BTreeMap<ReferenceKey, InventoryValue>,
) -> Reachability {
    let refresh_available = plan.refresh.as_ref().is_some_and(|reference| {
        matches!(
            inventory.get(reference),
            Some(InventoryValue::Present { .. })
        )
    });
    if status == 407 {
        Reachability::ProxyCredentialRejected
    } else if matches!(status, 401 | 403) && refresh_available {
        Reachability::RefreshRequired(status)
    } else if matches!(status, 401 | 403) && plan.auth_scheme == AuthScheme::OpaqueBundle {
        // Stored subscription bundles are deliberately opaque to doctor. The
        // probe proves only that the configured route is reachable; it must not
        // be reported as a successful credential validation when no bearer
        // token was sent.
        Reachability::ReachableAuthUnverified(status)
    } else if matches!(status, 401 | 403) {
        Reachability::CredentialRejected(status)
    } else if status == 429 {
        Reachability::RateLimited(status)
    } else if (500..=599).contains(&status) {
        Reachability::ServiceUnavailable(status)
    } else if (200..=299).contains(&status) && plan.auth_scheme == AuthScheme::OpaqueBundle {
        Reachability::ReachableAuthUnverified(status)
    } else if (200..=299).contains(&status) {
        Reachability::Reachable(status)
    } else {
        Reachability::EndpointResponseUnexpected(status)
    }
}

fn reachability_check(name: &str, value: Reachability, timeout_ms: u64) -> DoctorCheck {
    let id = format!("provider.{name}.reachability");
    let (status, code, summary) = match value {
        Reachability::Reachable(_) => (
            CheckStatus::Pass,
            "provider_reachable",
            "provider endpoint is reachable through the configured network path",
        ),
        Reachability::ReachableAuthUnverified(_) => (
            CheckStatus::Warning,
            "provider_reachable_auth_unverified",
            "provider endpoint is reachable, but doctor did not transmit the opaque subscription credential",
        ),
        Reachability::EndpointResponseUnexpected(_) => (
            CheckStatus::Warning,
            "provider_endpoint_response_unexpected",
            "provider route responded, but its status did not confirm a healthy API endpoint",
        ),
        Reachability::RateLimited(_) => (
            CheckStatus::Warning,
            "provider_rate_limited",
            "provider route is reachable but currently rate limited",
        ),
        Reachability::ServiceUnavailable(_) => (
            CheckStatus::Fail,
            "provider_service_unavailable",
            "provider route responded with a server-side failure",
        ),
        Reachability::CredentialRejected(_) => (
            CheckStatus::Fail,
            "credential_rejected",
            "provider rejected the configured credential",
        ),
        Reachability::RefreshRequired(_) => (
            CheckStatus::Warning,
            "credential_refresh_required",
            "provider endpoint is reachable, but validating it requires a token refresh that doctor will not perform",
        ),
        Reachability::ProxyCredentialRejected => (
            CheckStatus::Fail,
            "proxy_credential_rejected",
            "configured proxy rejected its credential",
        ),
        Reachability::Unreachable => (
            CheckStatus::Fail,
            "provider_unreachable",
            "provider endpoint was unreachable within the bounded timeout",
        ),
    };
    let mut item = check(id, status, code, summary);
    item.details
        .insert("timeout_ms".to_owned(), timeout_ms.to_string());
    if let Reachability::Reachable(http_status)
    | Reachability::ReachableAuthUnverified(http_status)
    | Reachability::EndpointResponseUnexpected(http_status)
    | Reachability::RateLimited(http_status)
    | Reachability::ServiceUnavailable(http_status)
    | Reachability::CredentialRejected(http_status)
    | Reachability::RefreshRequired(http_status) = value
    {
        item.details
            .insert("http_status".to_owned(), http_status.to_string());
    }
    item
}

pub(crate) fn render_text(report: &DoctorReport) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    let _ = writeln!(
        output,
        "Rottweiler doctor: {}",
        if report.healthy {
            "healthy"
        } else {
            "issues found"
        }
    );
    for item in &report.checks {
        let marker = match item.status {
            CheckStatus::Pass => "PASS",
            CheckStatus::Warning => "WARN",
            CheckStatus::Fail => "FAIL",
            CheckStatus::Skipped => "SKIP",
        };
        let _ = writeln!(output, "[{marker}] {}: {}", item.id, item.summary);
    }
    output
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use rw_store::credentials::{CredentialError, KeychainUnavailable, Secret};
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    #[derive(Clone, Copy)]
    struct EmptyEnvironment;

    impl CredentialEnvironment for EmptyEnvironment {
        fn get(&self, _name: &str) -> std::result::Result<Option<String>, CredentialError> {
            Ok(None)
        }
    }

    #[derive(Clone)]
    struct SeededEnvironment {
        name: String,
        value: String,
    }

    impl CredentialEnvironment for SeededEnvironment {
        fn get(&self, name: &str) -> std::result::Result<Option<String>, CredentialError> {
            Ok((name == self.name).then(|| self.value.clone()))
        }
    }

    #[derive(Clone)]
    struct CountingVault {
        reads: Arc<AtomicUsize>,
    }

    impl CredentialKeychain for CountingVault {
        fn get(
            &self,
            identifier: &str,
        ) -> std::result::Result<Option<Secret<String>>, KeychainUnavailable> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            assert_eq!(identifier, rw_store::credentials::KEYCHAIN_VAULT_ID);
            Ok(Some(Secret::new(
                "version = 1\n[credentials]\nfirst = 'one'\nsecond = 'two'\n".to_owned(),
            )))
        }

        fn set(
            &self,
            _identifier: &str,
            _secret: &Secret<String>,
        ) -> std::result::Result<(), KeychainUnavailable> {
            Err(KeychainUnavailable)
        }
    }

    #[derive(Clone)]
    struct EmptyCountingVault {
        reads: Arc<AtomicUsize>,
        writes: Arc<AtomicUsize>,
        fresh_reads: Arc<AtomicUsize>,
        legacy_reads: Arc<AtomicUsize>,
    }

    impl CredentialKeychain for EmptyCountingVault {
        fn get(
            &self,
            identifier: &str,
        ) -> std::result::Result<Option<Secret<String>>, KeychainUnavailable> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            assert_eq!(identifier, rw_store::credentials::KEYCHAIN_VAULT_ID);
            Ok(None)
        }

        fn set(
            &self,
            _identifier: &str,
            _secret: &Secret<String>,
        ) -> std::result::Result<(), KeychainUnavailable> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn get_fresh(
            &self,
            _identifier: &str,
        ) -> std::result::Result<Option<Secret<String>>, KeychainUnavailable> {
            self.fresh_reads.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }

        fn get_legacy(
            &self,
            _identifier: &str,
        ) -> std::result::Result<Option<Secret<String>>, KeychainUnavailable> {
            self.legacy_reads.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
    }

    fn seeded_report(checks: Vec<DoctorCheck>) -> DoctorReport {
        finish_report(false, checks)
    }

    #[test]
    fn seeded_bad_credential_is_diagnosed_deterministically() {
        let report = seeded_report(vec![reachability_check(
            "fixture",
            Reachability::CredentialRejected(401),
            500,
        )]);
        assert!(report.has_failures());
        assert_eq!(report.checks[0].code, "credential_rejected");
        assert_eq!(report.checks[0].details["http_status"], "401");
    }

    #[test]
    fn malformed_subscription_bundle_is_not_reported_as_present() {
        let reference = reference_key("subscription".to_owned(), None);
        let plan = ProviderPlan {
            name: "openai".to_owned(),
            kind: "openai_subscription".to_owned(),
            endpoint: None,
            auth: Some(reference.clone()),
            auth_scheme: AuthScheme::OpaqueBundle,
            refresh: None,
            proxy: None,
            proxy_username: None,
            proxy_password: None,
        };
        let inventory = BTreeMap::from([(
            reference,
            InventoryValue::Present {
                source: "test",
                secret: DoctorSecret("malformed-canary".to_owned()),
            },
        )]);
        let item = provider_auth_check(&plan, &inventory);
        assert_eq!(item.status, CheckStatus::Fail);
        assert_eq!(item.code, "credential_invalid");
        assert!(
            !serde_json::to_string(&item)
                .expect("JSON")
                .contains("malformed-canary")
        );
    }

    #[test]
    fn opaque_subscription_probe_never_claims_authentication_succeeded() {
        let reference = reference_key("subscription".to_owned(), None);
        let plan = ProviderPlan {
            name: "openai".to_owned(),
            kind: "openai_subscription".to_owned(),
            endpoint: None,
            auth: Some(reference.clone()),
            auth_scheme: AuthScheme::OpaqueBundle,
            refresh: None,
            proxy: None,
            proxy_username: None,
            proxy_password: None,
        };
        let inventory = BTreeMap::from([(
            reference,
            InventoryValue::Present {
                source: "test",
                secret: DoctorSecret("opaque-canary".to_owned()),
            },
        )]);
        let value = classify_reachability(401, &plan, &inventory);
        assert_eq!(value, Reachability::ReachableAuthUnverified(401));
        let item = reachability_check("openai", value, 500);
        assert_eq!(item.status, CheckStatus::Warning);
        assert_eq!(item.code, "provider_reachable_auth_unverified");
    }

    #[test]
    fn oauth_rejection_with_any_refresh_token_requires_refresh() {
        let access = reference_key("oauth-access".to_owned(), None);
        let refresh = reference_key("oauth-refresh".to_owned(), None);
        let plan = ProviderPlan {
            name: "oauth".to_owned(),
            kind: "generic".to_owned(),
            endpoint: None,
            auth: Some(access.clone()),
            auth_scheme: AuthScheme::Bearer,
            refresh: Some(refresh.clone()),
            proxy: None,
            proxy_username: None,
            proxy_password: None,
        };
        let inventory = BTreeMap::from([
            (
                access,
                InventoryValue::Present {
                    source: "test",
                    secret: DoctorSecret("expired-access".to_owned()),
                },
            ),
            (
                refresh,
                InventoryValue::Present {
                    source: "test",
                    secret: DoctorSecret("refresh-canary".to_owned()),
                },
            ),
        ]);
        assert_eq!(
            classify_reachability(401, &plan, &inventory),
            Reachability::RefreshRequired(401)
        );
    }

    #[test]
    fn seeded_unreachable_provider_is_distinct_from_bad_auth() {
        let report = seeded_report(vec![reachability_check(
            "fixture",
            Reachability::Unreachable,
            500,
        )]);
        assert!(report.has_failures());
        assert_eq!(report.checks[0].code, "provider_unreachable");
        assert!(!report.checks[0].details.contains_key("http_status"));
    }

    #[test]
    fn non_success_provider_statuses_are_never_reported_as_healthy() {
        let plan = ProviderPlan {
            name: "fixture".to_owned(),
            kind: "credential_free".to_owned(),
            endpoint: None,
            auth: None,
            auth_scheme: AuthScheme::None,
            refresh: None,
            proxy: None,
            proxy_username: None,
            proxy_password: None,
        };
        let inventory = BTreeMap::new();

        let not_found = reachability_check(
            "fixture",
            classify_reachability(404, &plan, &inventory),
            500,
        );
        assert_eq!(not_found.status, CheckStatus::Warning);
        assert_eq!(not_found.code, "provider_endpoint_response_unexpected");

        let limited = reachability_check(
            "fixture",
            classify_reachability(429, &plan, &inventory),
            500,
        );
        assert_eq!(limited.status, CheckStatus::Warning);
        assert_eq!(limited.code, "provider_rate_limited");

        let unavailable = reachability_check(
            "fixture",
            classify_reachability(500, &plan, &inventory),
            500,
        );
        assert_eq!(unavailable.status, CheckStatus::Fail);
        assert_eq!(unavailable.code, "provider_service_unavailable");
    }

    #[test]
    fn seeded_unavailable_sandbox_is_a_failure() {
        let report = seeded_report(vec![sandbox_check(false, "none")]);
        assert!(report.has_failures());
        assert_eq!(report.checks[0].code, "sandbox_unavailable");
    }

    #[test]
    fn seeded_dumb_terminal_is_a_failure() {
        let report = seeded_report(vec![terminal_check_from(Some("dumb"), None, true)]);
        assert!(report.has_failures());
        assert_eq!(report.checks[0].code, "terminal_dumb");
    }

    #[test]
    fn unsupported_native_windows_fails_and_unknown_architecture_warns() {
        let windows = os_check_from("windows", "x86_64", false);
        assert_eq!(windows.status, CheckStatus::Fail);
        assert_eq!(windows.code, "os_unsupported");

        let unknown_arch = os_check_from("linux", "mystery", false);
        assert_eq!(unknown_arch.status, CheckStatus::Warning);
        assert_eq!(unknown_arch.code, "architecture_unverified");
    }

    #[test]
    fn missing_but_creatable_config_root_does_not_fail() {
        let root = tempdir().expect("runtime root");
        let executable = std::env::current_exe().expect("test executable");
        let missing = root.path().join("new").join("config");
        let item = runtime_path_check(Some(&missing), Some(&executable), Some(root.path()));
        assert_eq!(item.status, CheckStatus::Warning);
        assert_eq!(item.code, "config_root_not_created");
        assert_eq!(item.details["config_root_state"], "creatable");
    }

    #[test]
    fn network_probes_are_skipped_unless_explicitly_requested() {
        let report = seeded_report(vec![check(
            "provider.fixture.reachability",
            CheckStatus::Skipped,
            "network_probe_not_requested",
            "skipped",
        )]);
        assert!(!report.has_failures());
        assert_eq!(report.checks[0].status, CheckStatus::Skipped);
    }

    #[test]
    fn stable_json_and_text_never_have_a_secret_field() {
        let report = seeded_report(vec![check(
            "provider.fixture.auth",
            CheckStatus::Pass,
            "credential_present",
            "provider credential is present",
        )]);
        let first = serde_json::to_string(&report).expect("doctor JSON");
        assert_eq!(first, serde_json::to_string(&report).expect("doctor JSON"));
        assert!(!first.contains("secret"));
        assert!(!render_text(&report).contains("secret"));
    }

    #[test]
    fn timeout_is_clamped_to_a_bounded_range() {
        assert_eq!(
            1_u64.clamp(MIN_NETWORK_TIMEOUT_MS, MAX_NETWORK_TIMEOUT_MS),
            250
        );
        assert_eq!(
            u64::MAX.clamp(MIN_NETWORK_TIMEOUT_MS, MAX_NETWORK_TIMEOUT_MS),
            10_000
        );
    }

    #[test]
    fn credential_inventory_reads_the_shared_vault_once() {
        let root = tempdir().expect("credential root");
        let reads = Arc::new(AtomicUsize::new(0));
        let manager = CredentialManager::with_backends(
            EmptyEnvironment,
            CountingVault {
                reads: Arc::clone(&reads),
            },
            root.path().join("credentials.toml"),
        );
        let references = [
            reference_key("first".to_owned(), None),
            reference_key("second".to_owned(), None),
        ]
        .into_iter()
        .collect();
        let inventory = inventory_credentials_with_manager(&manager, references);
        assert!(matches!(
            inventory.get(&reference_key("first".to_owned(), None)),
            Some(InventoryValue::Present { .. })
        ));
        assert!(matches!(
            inventory.get(&reference_key("second".to_owned(), None)),
            Some(InventoryValue::Present { .. })
        ));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn empty_vault_inventory_never_writes_or_reads_legacy_items() {
        let root = tempdir().expect("credential root");
        let reads = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(AtomicUsize::new(0));
        let fresh_reads = Arc::new(AtomicUsize::new(0));
        let legacy_reads = Arc::new(AtomicUsize::new(0));
        let manager = CredentialManager::with_backends(
            EmptyEnvironment,
            EmptyCountingVault {
                reads: Arc::clone(&reads),
                writes: Arc::clone(&writes),
                fresh_reads: Arc::clone(&fresh_reads),
                legacy_reads: Arc::clone(&legacy_reads),
            },
            root.path().join("credentials.toml"),
        );
        let references = [
            reference_key("first".to_owned(), None),
            reference_key("second".to_owned(), None),
        ]
        .into_iter()
        .collect();
        let inventory = inventory_credentials_with_manager(&manager, references);
        assert!(
            inventory
                .values()
                .all(|value| matches!(value, InventoryValue::Missing))
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert_eq!(fresh_reads.load(Ordering::SeqCst), 0);
        assert_eq!(legacy_reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn empty_or_environment_only_inventory_does_not_touch_the_vault() {
        let root = tempdir().expect("credential root");
        let reads = Arc::new(AtomicUsize::new(0));
        let manager = CredentialManager::with_backends(
            EmptyEnvironment,
            CountingVault {
                reads: Arc::clone(&reads),
            },
            root.path().join("credentials.toml"),
        );
        assert!(inventory_credentials_with_manager(&manager, BTreeSet::new()).is_empty());
        assert_eq!(reads.load(Ordering::SeqCst), 0);

        let env_reads = Arc::new(AtomicUsize::new(0));
        let manager = CredentialManager::with_backends(
            SeededEnvironment {
                name: "DOCTOR_TOKEN".to_owned(),
                value: "environment-canary".to_owned(),
            },
            CountingVault {
                reads: Arc::clone(&env_reads),
            },
            root.path().join("credentials.toml"),
        );
        let reference = reference_key(
            "environment-only".to_owned(),
            Some("DOCTOR_TOKEN".to_owned()),
        );
        let inventory =
            inventory_credentials_with_manager(&manager, [reference.clone()].into_iter().collect());
        assert!(matches!(
            inventory.get(&reference),
            Some(InventoryValue::Present {
                source: "environment",
                ..
            })
        ));
        assert_eq!(env_reads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn loopback_probe_distinguishes_a_rejected_credential() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = vec![0_u8; 8 * 1024];
            let read = socket.read(&mut request).await.expect("request");
            let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
            assert!(request.contains("authorization: bearer rejected-canary"));
            socket
                .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("response");
        });
        let reference = reference_key("fixture-key".to_owned(), None);
        let inventory = BTreeMap::from([(
            reference.clone(),
            InventoryValue::Present {
                source: "test",
                secret: DoctorSecret("rejected-canary".to_owned()),
            },
        )]);
        let plan = ProviderPlan {
            name: "fixture".to_owned(),
            kind: "openai_compatible".to_owned(),
            endpoint: Some(Url::parse(&format!("http://{address}/v1")).expect("endpoint")),
            auth: Some(reference),
            auth_scheme: AuthScheme::Bearer,
            refresh: None,
            proxy: None,
            proxy_username: None,
            proxy_password: None,
        };
        assert_eq!(
            probe_provider(&plan, &inventory, 1_000).await,
            Reachability::CredentialRejected(401)
        );
        server.await.expect("server");
    }

    #[tokio::test]
    async fn explicit_proxy_path_and_proxy_auth_are_used() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy listener");
        let address = listener.local_addr().expect("proxy address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("proxy accept");
            let mut request = vec![0_u8; 8 * 1024];
            let read = socket.read(&mut request).await.expect("proxy request");
            let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
            assert!(request.starts_with("head http://doctor.invalid/probe "));
            assert!(request.contains("proxy-authorization: basic "));
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("proxy response");
        });
        let proxy_reference = reference_key("proxy-password".to_owned(), None);
        let inventory = BTreeMap::from([(
            proxy_reference.clone(),
            InventoryValue::Present {
                source: "test",
                secret: DoctorSecret("proxy-canary".to_owned()),
            },
        )]);
        let plan = ProviderPlan {
            name: "fixture".to_owned(),
            kind: "openai_compatible".to_owned(),
            endpoint: Some(Url::parse("http://doctor.invalid/probe").expect("endpoint")),
            auth: None,
            auth_scheme: AuthScheme::None,
            refresh: None,
            proxy: Some(Url::parse(&format!("http://{address}")).expect("proxy")),
            proxy_username: Some("doctor".to_owned()),
            proxy_password: Some(proxy_reference),
        };
        assert_eq!(
            probe_provider(&plan, &inventory, 1_000).await,
            Reachability::Reachable(204)
        );
        server.await.expect("proxy server");
    }
}
