//! Provider-blind routing, capabilities, pricing, and adapters.
//!
//! Provider wire formats terminate in this crate. Consumers submit the common
//! [`ProviderRequest`] and receive a stream of [`ProviderEvent`] values whether
//! the selected backend speaks Anthropic Messages, `OpenAI` Chat Completions, or
//! the deterministic replay format.

mod anthropic;
mod auth;
mod github_copilot;
mod http;
mod models_dev;
mod openai;
mod openai_subscription;
mod pricing;
mod proxy;
mod recording;
mod retry;
mod router;
mod sse;
mod token_response;
mod types;

pub use anthropic::{AnthropicConfig, AnthropicProvider, AnthropicThinkingStrategy};
pub use auth::{
    AuthMaterial, AuthProvider, DEFAULT_OAUTH_CALLBACK_TIMEOUT, KnownSecretRegistrar,
    OAuthAuthorizationCode, OAuthAuthorizationCodeConfig, OAuthEntropy, OAuthLoginSession,
    OAuthRefreshConfig, OAuthTokenSet, ProxyAuthentication, RefreshTokenSink, RefreshingOAuth,
    Secret, StaticAuth, SystemOAuthEntropy,
};
pub use github_copilot::{
    DeviceFlowCancellation, GITHUB_COPILOT_ACCESS_TOKEN_ENDPOINT, GITHUB_COPILOT_API_VERSION,
    GITHUB_COPILOT_BASE_URL, GITHUB_COPILOT_CLIENT_ID, GITHUB_COPILOT_DEVICE_CODE_ENDPOINT,
    GitHubCopilotAccessToken, GitHubCopilotCatalog, GitHubCopilotDeviceAuthorization,
    GitHubCopilotDeviceFlow, GitHubCopilotDeviceSession, GitHubCopilotEndpoint, GitHubCopilotModel,
    GitHubCopilotPricing, GitHubCopilotProvider, GitHubCopilotProviderConfig, GitHubCopilotRuntime,
    GitHubDeviceFlowTransport, GitHubDevicePoll, github_copilot_ai_credits,
    github_copilot_micros_usd_per_million, parse_github_copilot_models,
};
pub use http::{
    GuardedHttpByteStream, GuardedHttpFetchError, GuardedHttpFetchRequest,
    GuardedHttpFetchResponse, GuardedHttpMethod, GuardedHttpRequest, GuardedHttpStreamResponse,
    ProcessNetworkDenyGuard, ProviderReachabilityRequest, deny_outbound_network_for_process,
    guarded_http_fetch, guarded_http_request, provider_reachability_probe,
};
pub use models_dev::{
    DEFAULT_MODELS_DEV_URL, ModelsRefreshReport, default_models_path, refresh_models_dev,
    refresh_models_dev_with_proxy_auth,
};
pub use openai::{
    OpenAiChatRequestProfile, OpenAiCompatibleConfig, OpenAiCompatibleProvider, OpenAiWireMode,
};
pub use openai_subscription::{
    OPENAI_SUBSCRIPTION_AUTHORIZATION_ENDPOINT, OPENAI_SUBSCRIPTION_CLIENT_ID,
    OPENAI_SUBSCRIPTION_MODELS_COMPATIBILITY_VERSION, OPENAI_SUBSCRIPTION_MODELS_ENDPOINT,
    OPENAI_SUBSCRIPTION_REDIRECT_URI, OPENAI_SUBSCRIPTION_RESPONSES_ENDPOINT,
    OPENAI_SUBSCRIPTION_TOKEN_ENDPOINT, OpenAiSubscriptionAuth, OpenAiSubscriptionAuthConfig,
    OpenAiSubscriptionTokenSink, extract_openai_subscription_account_id,
    openai_subscription_oauth_flow, openai_subscription_oauth_flow_with_endpoints,
};
pub use pricing::{CostBreakdown, ModelPricing, PricingTable};
pub use proxy::{ProxyEnvironment, ProxyResolution, ProxySettings, ProxySource};
pub use recording::{FixtureRedactor, Recorder, ReplayProvider};
pub use retry::{
    Clock, Delay, JitterSource, ProductionJitter, RetryPolicy, SeededJitter, TokioClock, TokioDelay,
};
pub use router::{ModelCandidate, ProviderRouter, RouterError};
pub use types::{
    BoxEventStream, CacheBreakpointSupport, CacheHint, Capabilities, DiscoveredModel,
    DiscoveredProviderCatalog, FinishReason, NATIVE_WEB_SEARCH_TOOL_NAME,
    NativeWebSearchCapability, NativeWebSearchRequest, NetworkPolicy, Provider, ProviderError,
    ProviderErrorKind, ProviderEvent, ProviderModelMetadata, ProviderRequest, TokenUsage,
    ToolChoice, ToolDefinition, UsageAccounting, WireFrameSink, WireMode,
};

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "providers";
