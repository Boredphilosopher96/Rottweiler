//! Typed configuration schema shared by the engine and SDK consumers.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Fully resolved Rottweiler configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Engine concurrency limits.
    pub engine: EngineConfig,
    /// Provider-blind model role configuration.
    pub models: ModelConfig,
    /// User-scoped provider connection settings, keyed by a local provider name.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Outbound networking configuration.
    pub network: NetworkConfig,
    /// Permission defaults.
    pub permissions: PermissionConfig,
    /// Sandbox classification configuration.
    pub sandbox: SandboxConfig,
    /// Opt-in telemetry configuration.
    pub telemetry: TelemetryConfig,
    /// Self-update channel configuration.
    pub updates: UpdateConfig,
}

/// Engine runtime settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    /// Maximum number of sessions that may execute concurrently.
    pub max_concurrent_sessions: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_concurrent_sessions: 4,
        }
    }
}

/// Provider-blind model aliases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// Alias selected for a newly created interactive session.
    pub default: String,
    /// User-defined role alias to provider/model candidate chains.
    pub aliases: BTreeMap<String, Vec<String>>,
    /// Default thinking effort for each model alias.
    pub thinking: BTreeMap<String, ThinkingLevel>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            default: "fast".to_owned(),
            aliases: BTreeMap::new(),
            thinking: BTreeMap::new(),
        }
    }
}

/// Provider-neutral thinking effort selected by a model alias or session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    /// Do not request provider reasoning output.
    #[default]
    Off,
    /// Prefer the provider's least expensive reasoning mode.
    Low,
    /// Prefer a balanced reasoning mode.
    Medium,
    /// Prefer the provider's strongest reasoning mode.
    High,
}

/// Connection settings for one locally named provider adapter.
///
/// This type contains only credential references. Secret values are resolved
/// at runtime and must never be serialized into configuration or session data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Adapter kind understood by `rw-providers`.
    pub kind: String,
    /// Optional API endpoint override, including any required path prefix.
    pub base_url: Option<String>,
    /// Optional provider-specific outbound proxy override.
    pub proxy: Option<String>,
    /// Optional username for HTTP Basic proxy authentication.
    pub proxy_username: Option<String>,
    /// Optional keychain credential identifier containing the proxy password.
    pub proxy_password_credential: Option<String>,
    /// Optional environment-variable name containing an API key.
    pub api_key_env: Option<String>,
    /// Optional environment-variable name containing an OAuth access token.
    pub oauth_token_env: Option<String>,
    /// Provider-documented browser authorization endpoint for OAuth login.
    pub oauth_authorization_endpoint: Option<String>,
    /// Provider-documented authorization-code and refresh token endpoint.
    pub oauth_token_endpoint: Option<String>,
    /// Public native-client identifier registered with the provider.
    pub oauth_client_id: Option<String>,
    /// OAuth scopes requested during interactive login.
    #[serde(default)]
    pub oauth_scopes: Vec<String>,
    /// Optional credential-store identifier for the latest access token.
    pub oauth_access_token_credential: Option<String>,
    /// Optional credential-store identifier for the long-lived refresh token.
    pub oauth_refresh_token_credential: Option<String>,
}

/// Global outbound network settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// Optional HTTP(S) proxy URL. Credentials are resolved separately.
    pub proxy: Option<String>,
    /// Optional username for HTTP Basic proxy authentication.
    pub proxy_username: Option<String>,
    /// Optional keychain credential identifier containing the proxy password.
    pub proxy_password_credential: Option<String>,
}

/// Default permission decision used when no rule matches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Ask the active driver for approval.
    #[default]
    Ask,
    /// Allow the operation.
    Allow,
    /// Deny the operation.
    Deny,
}

/// Permission settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermissionConfig {
    /// Decision used when no more-specific policy applies.
    pub default: PermissionDecision,
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            default: PermissionDecision::Ask,
        }
    }
}

/// Sandbox settings that may only be changed by user-level configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Additional command patterns classified as safe by the user.
    pub safe_list: Vec<String>,
}

/// Telemetry settings. Telemetry is disabled unless explicitly enabled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Whether opt-in telemetry export is enabled.
    pub enabled: bool,
}

/// Signed update channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    /// Stable signed releases.
    #[default]
    Stable,
    /// Beta signed releases.
    Beta,
}

/// Self-update configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateConfig {
    /// Signed release channel.
    pub channel: UpdateChannel,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            channel: UpdateChannel::Stable,
        }
    }
}

/// Partially specified configuration read from TOML.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// Optional engine settings.
    pub engine: Option<EngineConfigFile>,
    /// Optional model settings.
    pub models: Option<ModelConfigFile>,
    /// Optional user-scoped provider connection settings.
    pub providers: Option<BTreeMap<String, ProviderConfig>>,
    /// Optional network settings.
    pub network: Option<NetworkConfigFile>,
    /// Optional permission settings.
    pub permissions: Option<PermissionConfigFile>,
    /// Optional sandbox settings.
    pub sandbox: Option<SandboxConfigFile>,
    /// Optional telemetry settings.
    pub telemetry: Option<TelemetryConfigFile>,
    /// Optional update settings.
    pub updates: Option<UpdateConfigFile>,
}

/// Partial engine configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineConfigFile {
    /// Optional concurrency override.
    pub max_concurrent_sessions: Option<usize>,
}

/// Partial model configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfigFile {
    /// Optional default alias override.
    pub default: Option<String>,
    /// Optional alias map replacement.
    pub aliases: Option<BTreeMap<String, Vec<String>>>,
    /// Optional per-alias thinking effort overrides.
    pub thinking: Option<BTreeMap<String, ThinkingLevel>>,
}

/// Partial network configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfigFile {
    /// Optional global proxy override.
    pub proxy: Option<String>,
    /// Optional global proxy username.
    pub proxy_username: Option<String>,
    /// Optional keychain credential identifier for the global proxy password.
    pub proxy_password_credential: Option<String>,
}

/// Partial permission configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionConfigFile {
    /// Optional default-decision override.
    pub default: Option<PermissionDecision>,
}

/// Partial sandbox configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfigFile {
    /// Optional safe-list replacement.
    pub safe_list: Option<Vec<String>>,
}

/// Partial telemetry configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfigFile {
    /// Optional telemetry override.
    pub enabled: Option<bool>,
}

/// Partial update configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateConfigFile {
    /// Optional update-channel override.
    pub channel: Option<UpdateChannel>,
}
