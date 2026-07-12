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
    /// Automatic/manual context compaction settings.
    #[serde(default)]
    pub compaction: CompactionConfig,
    /// Monetary and provider-credit budget guardrails.
    #[serde(default)]
    pub budget: BudgetConfig,
    /// User-scoped provider connection settings, keyed by a local provider name.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Outbound networking configuration.
    pub network: NetworkConfig,
    /// User-scoped configured web-search API boundary.
    #[serde(default)]
    pub websearch: WebSearchConfig,
    /// Permission defaults.
    pub permissions: PermissionConfig,
    /// Sandbox classification configuration.
    pub sandbox: SandboxConfig,
    /// Declarative formatter, linter, and test hooks.
    #[serde(default)]
    pub toolchain: ToolchainConfig,
    /// Opt-in telemetry configuration.
    pub telemetry: TelemetryConfig,
    /// Self-update channel configuration.
    pub updates: UpdateConfig,
    /// Safe presentation preferences managed by the TUI or user config.
    #[serde(default)]
    pub ui: UiConfig,
}

/// User-facing presentation preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Built-in theme name.
    pub theme: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "kennel-dark".to_owned(),
        }
    }
}

/// Provider-blind context compaction settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    /// Whether local overflow estimates trigger automatic compaction.
    pub auto: bool,
    /// Explicit token reserve; absent uses `min(20_000, max_output_tokens)`.
    #[serde(rename = "reserved", alias = "reserved_tokens")]
    pub reserved_tokens: Option<u64>,
    /// Optional compaction model alias; absent or unresolved falls back to the
    /// current session alias.
    pub model_alias: Option<String>,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            auto: true,
            reserved_tokens: None,
            model_alias: None,
        }
    }
}

impl CompactionConfig {
    /// Validates context-independent compaction invariants.
    ///
    /// Resolved model context-window validation remains an engine concern.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero reserve or blank model alias.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.reserved_tokens == Some(0) {
            return Err("compaction.reserved must be greater than zero");
        }
        if self
            .model_alias
            .as_deref()
            .is_some_and(|alias| alias.trim().is_empty())
        {
            return Err("compaction.model_alias must not be empty");
        }
        Ok(())
    }
}

/// Session and daily spend guardrails in explicit billing units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    /// Session ordinary API-cost cap in micro-US-dollars.
    pub session_cost_cap_micros_usd: Option<u64>,
    /// UTC-day ordinary API-cost cap in micro-US-dollars.
    pub daily_cost_cap_micros_usd: Option<u64>,
    /// Session provider-credit cap in micro-credits.
    pub session_ai_credit_cap_micros: Option<u64>,
    /// UTC-day provider-credit cap in micro-credits.
    pub daily_ai_credit_cap_micros: Option<u64>,
    /// Ordinary API spend-rate alarm over a trailing minute.
    pub spend_rate_alarm_micros_usd_per_minute: Option<u64>,
    /// Provider-credit burn-rate alarm over a trailing minute.
    pub ai_credit_rate_alarm_micros_per_minute: Option<u64>,
    /// Percentage of a cap at which a warning is emitted.
    pub warn_at_percent: u8,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            session_cost_cap_micros_usd: None,
            daily_cost_cap_micros_usd: None,
            session_ai_credit_cap_micros: None,
            daily_ai_credit_cap_micros: None,
            spend_rate_alarm_micros_usd_per_minute: None,
            ai_credit_rate_alarm_micros_per_minute: None,
            warn_at_percent: 80,
        }
    }
}

impl BudgetConfig {
    /// Validates the configured warning threshold. Explicit zero caps remain
    /// valid and intentionally stop spending immediately.
    ///
    /// # Errors
    ///
    /// Returns an error when `warn_at_percent` is outside 1 through 100.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=100).contains(&self.warn_at_percent) {
            return Err("budget.warn_at_percent must be between 1 and 100");
        }
        Ok(())
    }
}

/// Engine runtime settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    /// Maximum number of sessions that may execute concurrently.
    pub max_concurrent_sessions: usize,
    /// Maximum nested child-session depth.
    pub subagent_max_depth: usize,
    /// Maximum child sessions that may execute concurrently per orchestrator.
    pub subagent_max_concurrency: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_concurrent_sessions: 4,
            subagent_max_depth: 2,
            subagent_max_concurrency: 4,
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
    /// Optional credential-store identifier containing an API key.
    pub api_key_credential: Option<String>,
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

/// Optional generic search API used when provider-native search is unavailable.
/// This section is accepted only from user configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchConfig {
    /// Absolute HTTP(S) endpoint without credentials, query, or fragment.
    pub endpoint: Option<String>,
    /// Query-string key used for the search text.
    pub query_parameter: String,
    /// Request header to credential-store identifier mappings.
    pub header_credentials: BTreeMap<String, String>,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            query_parameter: "q".to_owned(),
            header_credentials: BTreeMap::new(),
        }
    }
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
    /// Tool/argument patterns evaluated before the default.
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            default: PermissionDecision::Ask,
            rules: Vec::new(),
        }
    }
}

/// One permission rule using `tool(glob-over-canonical-arguments)` syntax.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermissionRule {
    /// Matcher such as `bash(git status*)` or `write(/etc/**)`.
    #[serde(rename = "match")]
    pub pattern: String,
    /// Decision applied when the matcher covers the complete invocation.
    pub action: PermissionDecision,
}

/// Sandbox settings that may only be changed by user-level configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Additional command patterns classified as safe by the user.
    pub safe_list: Vec<String>,
}

/// Declarative commands registered onto the shared post-tool hook pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ToolchainConfig {
    /// Formatter applied when no more-specific rule overrides it.
    pub formatter: Option<String>,
    /// Linters applied when no more-specific rule overrides them.
    pub linters: Vec<String>,
    /// Optional default test command surfaced to initialization and commands.
    pub test: Option<String>,
    /// Glob-specific toolchain overrides, in declaration order.
    #[serde(rename = "rule", alias = "rules")]
    pub rules: Vec<ToolchainRule>,
}

/// One file-glob-specific toolchain rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolchainRule {
    /// Workspace-relative file glob, such as `**/*.rs`.
    #[serde(rename = "match")]
    pub pattern: String,
    /// Optional formatter override. `{file}` expands to the touched path.
    pub formatter: Option<String>,
    /// Linter override. `{file}` expands to the touched path.
    #[serde(default)]
    pub linters: Vec<String>,
    /// Optional test command associated with this file class.
    pub test: Option<String>,
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
    /// Optional compaction settings.
    pub compaction: Option<CompactionConfigFile>,
    /// Optional budget settings.
    pub budget: Option<BudgetConfigFile>,
    /// Optional user-scoped provider connection settings.
    pub providers: Option<BTreeMap<String, ProviderConfig>>,
    /// Optional network settings.
    pub network: Option<NetworkConfigFile>,
    /// Optional user-scoped configured web-search API.
    pub websearch: Option<WebSearchConfigFile>,
    /// Optional permission settings.
    pub permissions: Option<PermissionConfigFile>,
    /// Optional sandbox settings.
    pub sandbox: Option<SandboxConfigFile>,
    /// Optional declarative toolchain hooks.
    pub toolchain: Option<ToolchainConfig>,
    /// Optional telemetry settings.
    pub telemetry: Option<TelemetryConfigFile>,
    /// Optional update settings.
    pub updates: Option<UpdateConfigFile>,
    /// Optional presentation preferences.
    pub ui: Option<UiConfig>,
}

/// Partial compaction configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionConfigFile {
    pub auto: Option<bool>,
    #[serde(rename = "reserved", alias = "reserved_tokens")]
    pub reserved_tokens: Option<u64>,
    pub model_alias: Option<String>,
}

/// Partial spend guardrail configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfigFile {
    pub session_cost_cap_micros_usd: Option<u64>,
    pub daily_cost_cap_micros_usd: Option<u64>,
    pub session_ai_credit_cap_micros: Option<u64>,
    pub daily_ai_credit_cap_micros: Option<u64>,
    pub spend_rate_alarm_micros_usd_per_minute: Option<u64>,
    pub ai_credit_rate_alarm_micros_per_minute: Option<u64>,
    pub warn_at_percent: Option<u8>,
}

/// Partial engine configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineConfigFile {
    /// Optional concurrency override.
    pub max_concurrent_sessions: Option<usize>,
    /// Optional nested child depth override.
    pub subagent_max_depth: Option<usize>,
    /// Optional child concurrency override.
    pub subagent_max_concurrency: Option<usize>,
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

/// Partial user-scoped configured web-search API.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchConfigFile {
    pub endpoint: Option<String>,
    pub query_parameter: Option<String>,
    pub header_credentials: Option<BTreeMap<String, String>>,
}

/// Partial permission configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionConfigFile {
    /// Optional default-decision override.
    pub default: Option<PermissionDecision>,
    /// Optional replacement rule set.
    pub rules: Option<Vec<PermissionRule>>,
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
