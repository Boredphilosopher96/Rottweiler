use std::{fmt, pin::Pin};

use async_trait::async_trait;
use futures_core::Stream;
use rw_types::{Turn, config::ThinkingLevel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use ts_rs::TS;

use crate::ModelPricing;

/// Maximum encoded arguments accepted for one streamed provider tool call.
pub(crate) const MAX_PROVIDER_TOOL_ARGUMENT_BYTES: usize = 1_048_576;
/// Maximum response body accepted from a provider model-catalog endpoint.
pub(crate) const MAX_PROVIDER_MODEL_CATALOG_BYTES: usize = 4 * 1024 * 1024;
/// Maximum structured provider error body retained for classification.
pub(crate) const MAX_PROVIDER_ERROR_BYTES: usize = 64 * 1024;

/// Reserved marker translated only by adapters that explicitly advertise
/// provider-native web search.
pub const NATIVE_WEB_SEARCH_TOOL_NAME: &str = "__rottweiler_provider_native_web_search";

/// Whether a selected model/API can execute a provider-hosted web search.
///
/// This is intentionally separate from [`Capabilities`] until model discovery
/// has positively identified the feature; adapters must not infer support only
/// from their wire dialect.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeWebSearchCapability {
    #[default]
    Unsupported,
    Supported,
}

/// Provider-neutral representation of an explicitly requested native search.
/// Adapters that advertise [`NativeWebSearchCapability::Supported`] translate
/// this into their provider's built-in search tool shape.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NativeWebSearchRequest {
    pub query: String,
    pub max_results: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recency_days: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_domains: Vec<String>,
}

impl NativeWebSearchRequest {
    /// Validate provider-independent limits before an adapter serializes a
    /// request.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request error for an empty/oversized query, an
    /// out-of-range result count, or malformed domain filters.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.query.trim().is_empty() || self.query.len() > 4_096 {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "web search query must contain 1 to 4096 bytes",
            ));
        }
        if !(1..=50).contains(&self.max_results) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "web search result limit must be between 1 and 50",
            ));
        }
        if self.allowed_domains.len() > 20
            || self
                .allowed_domains
                .iter()
                .any(|domain| !valid_search_domain(domain))
        {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "web search domain filter is invalid",
            ));
        }
        Ok(())
    }

    /// Validate both the request and the model-discovered feature flag.
    ///
    /// # Errors
    ///
    /// Returns unsupported when the selected model has not positively
    /// advertised native search, or invalid request when request bounds fail.
    pub fn validate_for(&self, capability: NativeWebSearchCapability) -> Result<(), ProviderError> {
        if capability != NativeWebSearchCapability::Supported {
            return Err(ProviderError::new(
                ProviderErrorKind::Unsupported,
                "selected model does not advertise provider-native web search",
            ));
        }
        self.validate()
    }

    /// Encode this request into the normal provider request stream so existing
    /// record/replay middleware captures native search without a side channel.
    ///
    /// # Errors
    ///
    /// Returns invalid request when bounds fail or encoding is unavailable.
    pub fn tool_definition(&self) -> Result<ToolDefinition, ProviderError> {
        self.validate()?;
        let input_schema = serde_json::to_value(self).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "native web-search request could not be encoded",
            )
        })?;
        Ok(ToolDefinition {
            name: NATIVE_WEB_SEARCH_TOOL_NAME.to_owned(),
            description: "Internal provider-native web search".to_owned(),
            input_schema,
        })
    }
}

pub(crate) fn native_web_search_request(
    tool: &ToolDefinition,
) -> Result<Option<NativeWebSearchRequest>, ProviderError> {
    if tool.name != NATIVE_WEB_SEARCH_TOOL_NAME {
        return Ok(None);
    }
    let request: NativeWebSearchRequest = serde_json::from_value(tool.input_schema.clone())
        .map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "native web-search marker is malformed",
            )
        })?;
    request.validate()?;
    Ok(Some(request))
}

fn valid_search_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.is_ascii()
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// A provider-neutral request assembled by the engine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderRequest {
    /// Provider-local model name. The router replaces aliases before dispatch.
    pub model: String,
    /// Conversation in Rottweiler's shared message IR.
    pub turns: Vec<Turn>,
    /// Function tools exposed for this turn.
    pub tools: Vec<ToolDefinition>,
    /// Whether the model may, must, must not, or must specifically call a tool.
    pub tool_choice: ToolChoice,
    /// Maximum number of output tokens.
    pub max_output_tokens: u32,
    /// Optional sampling temperature.
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "rw_types::schema::required_nullable::<f32>")]
    pub temperature: Option<f32>,
    /// Provider-independent reasoning control.
    pub thinking: ThinkingLevel,
    /// Stable-prefix boundary assembled by the provider-neutral context engine.
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "rw_types::schema::required_nullable::<CacheHint>")]
    pub cache_hint: Option<CacheHint>,
}

/// Provider-neutral stable prompt prefix eligible for adapter cache mapping.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct CacheHint {
    /// Count of leading turns in the stable prefix.
    pub stable_prefix_turns: u32,
    /// Whether all tool definitions are part of that stable prefix.
    pub tools_in_prefix: bool,
}

/// Provider-neutral tool selection policy.
///
/// Adapters translate this into each provider's wire shape. A named choice is
/// validated against [`ProviderRequest::tools`] before a request is sent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case", tag = "mode", deny_unknown_fields)]
pub enum ToolChoice {
    /// The model may answer normally or call any exposed tool.
    Auto {},
    /// The model must call at least one exposed tool.
    Required {},
    /// The model must not call a tool, even when tools are exposed.
    None {},
    /// The model must call this exact function tool.
    Named {
        /// Name of a function present in [`ProviderRequest::tools`].
        name: String,
    },
}

impl Default for ToolChoice {
    fn default() -> Self {
        Self::Auto {}
    }
}

impl ProviderRequest {
    /// Validates the provider-independent invariants of [`Self::tool_choice`].
    ///
    /// # Errors
    ///
    /// Returns a sanitized invalid-request error when a required or named tool
    /// choice cannot be satisfied by this request's tool definitions.
    pub fn validate_tool_choice(&self) -> Result<(), ProviderError> {
        match &self.tool_choice {
            ToolChoice::Required {} if self.tools.is_empty() => Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "required tool choice needs at least one tool definition",
            )),
            ToolChoice::Named { name }
                if name.is_empty() || !self.tools.iter().any(|tool| tool.name == *name) =>
            {
                Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "named tool choice must match an exposed tool definition",
                ))
            }
            ToolChoice::Auto {}
            | ToolChoice::Required {}
            | ToolChoice::None {}
            | ToolChoice::Named { .. } => Ok(()),
        }
    }
}

/// A function tool definition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    /// Tool name presented to the model.
    pub name: String,
    /// Human-readable behavior summary.
    pub description: String,
    /// JSON Schema accepted by the tool.
    pub input_schema: Value,
}

/// Prompt-cache behavior offered by an adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheBreakpointSupport {
    /// The API has no explicit cache breakpoint controls.
    None,
    /// The API accepts explicit cache breakpoint markers.
    Explicit,
    /// The provider manages caching without client markers.
    Automatic,
}

/// Capabilities used by the engine to degrade gracefully.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Supports function tool calls.
    pub tool_calling: bool,
    /// Supports image inputs.
    pub vision: bool,
    /// Supports a reasoning/thinking control.
    pub thinking: bool,
    /// Prompt caching behavior.
    pub cache_breakpoints: CacheBreakpointSupport,
    /// Maximum context size when known.
    pub max_context_tokens: Option<u64>,
    /// Maximum output size when separately known.
    pub max_output_tokens: Option<u64>,
    /// Wire protocol used for record/replay parser selection.
    pub wire_mode: WireMode,
}

/// Billing/quota unit associated with dynamically discovered model metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum UsageAccounting {
    /// Ordinary API usage with dollar-denominated pricing.
    ApiDollars,
    /// Ordinary API usage without an authoritative rate.
    UnpricedApi,
    /// Subscription quota that must not be presented as a zero-dollar API.
    SubscriptionQuota,
    /// Provider credits with an explicit nominal micro-USD conversion.
    AiCredits {
        /// Nominal value of one credit in micro-US-dollars.
        micros_usd_per_credit: u64,
    },
}

/// Capabilities, rates, and billing unit discovered asynchronously by a provider.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelMetadata {
    /// Model-specific capabilities enforced by the provider.
    pub capabilities: Capabilities,
    /// Optional authenticated model pricing.
    pub pricing: Option<ModelPricing>,
    /// Unit used to interpret rates and usage.
    pub accounting: UsageAccounting,
}

/// One model returned by an authenticated provider's live catalog.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredModel {
    /// Provider-local model identifier accepted by inference requests.
    pub id: String,
    /// Human-readable provider-supplied name when available.
    pub display_name: Option<String>,
    /// Provider-supplied description when available.
    pub description: Option<String>,
    /// Authoritative live capabilities when the catalog exposes them.
    pub capabilities: Option<Capabilities>,
    /// Authoritative live pricing when the catalog exposes it.
    pub pricing: Option<ModelPricing>,
}

/// Bounded snapshot of models currently available from one authenticated provider.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredProviderCatalog {
    /// Stable, sanitized logical provider key.
    pub provider: String,
    /// Picker-visible models in deterministic provider-local id order.
    pub models: Vec<DiscoveredModel>,
}

/// Provider HTTP dialect. Kept inside the provider boundary and recorded so
/// raw replay frames are routed through the same parser as live traffic.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireMode {
    /// Anthropic Messages SSE.
    AnthropicMessages,
    /// OpenAI-compatible Chat Completions SSE.
    OpenAiChatCompletions,
    /// `OpenAI` Responses SSE.
    OpenAiResponses,
    /// Dynamically discovered GitHub Copilot Messages, Responses, or Chat SSE.
    GitHubCopilot,
    /// Resolved GitHub Copilot Anthropic-compatible Messages SSE.
    GitHubCopilotMessages,
    /// Resolved GitHub Copilot OpenAI-compatible Responses SSE.
    GitHubCopilotResponses,
    /// Resolved GitHub Copilot OpenAI-compatible Chat Completions SSE.
    GitHubCopilotChatCompletions,
    /// Already-normalized replay events.
    NormalizedReplay,
}

/// Provider usage normalized across API families.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct TokenUsage {
    /// Non-cached input tokens.
    #[ts(type = "number")]
    pub input_tokens: u64,
    /// Generated tokens.
    #[ts(type = "number")]
    pub output_tokens: u64,
    /// Input tokens served from a prompt cache.
    #[ts(type = "number")]
    pub cache_read_tokens: u64,
    /// Input tokens written into a prompt cache.
    #[ts(type = "number")]
    pub cache_write_tokens: u64,
    /// Reasoning tokens when separately reported.
    #[ts(type = "number")]
    pub reasoning_tokens: u64,
}

/// Why a model stream ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Normal end of generation.
    Stop,
    /// Output token limit reached.
    Length,
    /// The model requested one or more tools.
    ToolCalls,
    /// Provider content filter stopped the request.
    ContentFilter,
    /// Provider-specific reason retained as an unknown category.
    Unknown,
}

/// The normalized event stream consumed by the engine and replay harness.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderEvent {
    /// Opaque router-owned identity for the candidate that served this stream.
    /// It is consumed by accounting and never exposed as a provider name.
    #[serde(skip_deserializing)]
    #[schemars(skip)]
    #[ts(skip)]
    RouteSelected { route: String },
    /// Provider accepted the message and selected a concrete model.
    MessageStart { model: String },
    /// Visible assistant text.
    TextDelta { text: String },
    /// Hidden or summarized model reasoning.
    ThinkingDelta {
        content: String,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "rw_types::schema::required_nullable::<String>")]
        signature: Option<String>,
    },
    /// Beginning of a function call.
    ToolCallStart { id: String, name: String },
    /// Incremental JSON argument bytes.
    ToolCallArgumentsDelta { id: String, json_fragment: String },
    /// Completed and parsed function arguments.
    ToolCallEnd { id: String, arguments: Value },
    /// A source citation emitted by a provider-native response.
    Citation {
        uri: String,
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "rw_types::schema::required_nullable::<String>")]
        title: Option<String>,
        #[ts(type = "number | null")]
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "rw_types::schema::required_nullable::<u64>")]
        start_index: Option<u64>,
        #[ts(type = "number | null")]
        #[serde(deserialize_with = "Option::deserialize")]
        #[schemars(schema_with = "rw_types::schema::required_nullable::<u64>")]
        end_index: Option<u64>,
    },
    /// Latest usage totals for the response.
    Usage { usage: TokenUsage },
    /// Terminal event.
    Finished { reason: FinishReason },
}

/// Stable provider error categories used by retry and failover.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    /// Local physical execution capacity was exhausted before dispatch; never fail over.
    ResourceExhausted,
    /// Local effects or their accounting could not be proven settled; never retry.
    EffectsUnsettled,
    /// Missing or rejected credentials.
    Authentication,
    /// Provider rate limit.
    RateLimited,
    /// Request timeout.
    Timeout,
    /// Provider returned a server error.
    Server,
    /// Request was rejected as invalid.
    InvalidRequest,
    /// Request exceeded the selected model's context window.
    ContextOverflow,
    /// Provider response did not match its documented stream protocol.
    Protocol,
    /// Connection or transport failure.
    Network,
    /// The consumer stopped reading a provider stream before its terminal event.
    Cancelled,
    /// Replay fixture did not contain the request.
    ReplayMiss,
    /// Network access was disabled by policy.
    NetworkDisabled,
    /// Requested capability is unsupported.
    Unsupported,
}

/// An adapter error with retry metadata but no credential-bearing response body.
#[derive(Clone, Debug, Error, Eq, PartialEq, Serialize, Deserialize)]
#[error("{message}")]
pub struct ProviderError {
    /// Stable classification.
    pub kind: ProviderErrorKind,
    /// Sanitized, user-actionable message.
    pub message: String,
    /// Server-requested delay in milliseconds, if supplied.
    pub retry_after_ms: Option<u64>,
}

impl ProviderError {
    /// Constructs a sanitized provider error.
    #[must_use]
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after_ms: None,
        }
    }

    /// Adds a provider-specified retry delay.
    #[must_use]
    pub const fn with_retry_after(mut self, retry_after_ms: u64) -> Self {
        self.retry_after_ms = Some(retry_after_ms);
        self
    }

    /// Whether retry/failover is safe without changing the request.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ProviderErrorKind::RateLimited
                | ProviderErrorKind::Timeout
                | ProviderErrorKind::Server
                | ProviderErrorKind::Network
        )
    }
}

#[cfg(test)]
mod web_search_tests {
    use super::*;

    #[test]
    fn native_search_request_is_explicitly_bounded() {
        let request = NativeWebSearchRequest {
            query: "rust language server".to_owned(),
            max_results: 10,
            recency_days: Some(30),
            allowed_domains: vec!["rust-lang.org".to_owned()],
        };
        assert_eq!(request.validate(), Ok(()));
        assert!(matches!(
            request.validate_for(NativeWebSearchCapability::Unsupported),
            Err(ProviderError {
                kind: ProviderErrorKind::Unsupported,
                ..
            })
        ));

        let mut invalid = request;
        invalid.allowed_domains = vec!["https://attacker.invalid/path".to_owned()];
        assert!(matches!(
            invalid.validate(),
            Err(ProviderError {
                kind: ProviderErrorKind::InvalidRequest,
                ..
            })
        ));
    }
}

/// A sendable provider event stream.
pub type BoxEventStream =
    Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send + 'static>>;

/// Observer used by record middleware to capture canonical SSE frames without
/// exposing transport-only values in the engine's normalized event stream.
pub trait WireFrameSink: Send + Sync {
    /// Captures one fully decoded SSE frame.
    fn capture(&self, event: Option<&str>, data: &str);
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RawSseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// A backend adapter. Record/replay implements this same boundary.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Resolves the configuration and approved code identity for opaque state.
    /// `None` declares a provider whose events cannot carry continuation data.
    /// Lazy adapters may activate here; the operation owner retains their effects.
    async fn continuation_provenance(
        &self,
    ) -> Result<Option<crate::ContinuationProvenance>, ProviderError> {
        Ok(None)
    }

    /// Waits for host-owned effects abandoned by a dropped invocation or stream.
    /// This never proves that a remote HTTP service stopped work or billing.
    async fn settle_effects(&self) -> Result<(), ProviderError>;

    /// Stable provider key used by qualified model ids.
    fn name(&self) -> &str;

    /// Declared features for graceful engine degradation.
    fn capabilities(&self) -> Capabilities;

    /// Explicit adapter-level provider-native search capability.
    fn native_web_search_capability(&self) -> NativeWebSearchCapability {
        NativeWebSearchCapability::Unsupported
    }

    /// Resolves authenticated model metadata when a provider has a dynamic
    /// catalog. Static providers use the default `None` result.
    ///
    /// # Errors
    ///
    /// Returns a sanitized discovery or validation error.
    async fn model_metadata(&self) -> Result<Option<ProviderModelMetadata>, ProviderError> {
        Ok(None)
    }

    /// Returns already-resolved dynamic metadata without performing I/O.
    /// Providers with lazy catalogs expose `None` until discovery succeeds.
    fn cached_model_metadata(&self) -> Option<ProviderModelMetadata> {
        None
    }

    /// Returns cached metadata for one concrete model from a multi-model
    /// catalog. Providers whose metadata is not model-specific may use the
    /// default aggregate result.
    fn cached_model_metadata_for(&self, _model: &str) -> Option<ProviderModelMetadata> {
        self.cached_model_metadata()
    }

    /// Queries the authenticated provider for its currently selectable models.
    /// Static and replay-only providers use the default `None` result.
    ///
    /// # Errors
    ///
    /// Returns a sanitized authentication, network, or catalog protocol error.
    async fn discover_models(&self) -> Result<Option<DiscoveredProviderCatalog>, ProviderError> {
        Ok(None)
    }

    /// Starts a normalized streaming response.
    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError>;

    /// Starts a stream while capturing raw frames for deterministic recording.
    /// Non-HTTP fixture providers can use the default normalized-only path.
    async fn stream_with_wire_sink(
        &self,
        request: ProviderRequest,
        _sink: std::sync::Arc<dyn WireFrameSink>,
    ) -> Result<BoxEventStream, ProviderError> {
        self.stream(request).await
    }
}

/// Whether an HTTP adapter is allowed to open sockets.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NetworkPolicy {
    /// Normal live operation.
    #[default]
    Allow,
    /// Fail before any request is sent; used by deterministic CI replay.
    Deny,
}

impl fmt::Display for ProviderErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}
