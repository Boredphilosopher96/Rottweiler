//! Provider-blind routing, capabilities, pricing, and adapters.
//!
//! Provider wire formats terminate in this crate. Consumers submit the common
//! [`ProviderRequest`] and receive a stream of [`ProviderEvent`] values whether
//! the selected backend speaks Anthropic Messages, `OpenAI` Chat Completions, or
//! the deterministic replay format.

mod anthropic;
mod auth;
mod http;
mod models_dev;
mod openai;
mod pricing;
mod proxy;
mod recording;
mod retry;
mod router;
mod sse;
mod types;

pub use anthropic::{AnthropicConfig, AnthropicProvider, AnthropicThinkingStrategy};
pub use auth::{
    AuthMaterial, AuthProvider, DEFAULT_OAUTH_CALLBACK_TIMEOUT, KnownSecretRegistrar,
    OAuthAuthorizationCode, OAuthAuthorizationCodeConfig, OAuthEntropy, OAuthLoginSession,
    OAuthRefreshConfig, OAuthTokenSet, ProxyAuthentication, RefreshTokenSink, RefreshingOAuth,
    Secret, StaticAuth, SystemOAuthEntropy,
};
pub use http::{ProcessNetworkDenyGuard, deny_outbound_network_for_process};
pub use models_dev::{
    DEFAULT_MODELS_DEV_URL, ModelsRefreshReport, default_models_path, refresh_models_dev,
    refresh_models_dev_with_proxy_auth,
};
pub use openai::{OpenAiCompatibleConfig, OpenAiCompatibleProvider, OpenAiWireMode};
pub use pricing::{CostBreakdown, ModelPricing, PricingTable};
pub use proxy::{ProxyEnvironment, ProxyResolution, ProxySettings, ProxySource};
pub use recording::{FixtureRedactor, Recorder, ReplayProvider};
pub use retry::{
    Clock, Delay, JitterSource, ProductionJitter, RetryPolicy, SeededJitter, TokioClock, TokioDelay,
};
pub use router::{ModelCandidate, ProviderRouter, RouterError};
pub use types::{
    BoxEventStream, CacheBreakpointSupport, Capabilities, FinishReason, NetworkPolicy, Provider,
    ProviderError, ProviderErrorKind, ProviderEvent, ProviderRequest, ThinkingLevel, TokenUsage,
    ToolChoice, ToolDefinition, WireFrameSink, WireMode,
};

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "providers";
