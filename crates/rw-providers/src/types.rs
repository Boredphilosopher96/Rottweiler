use std::{fmt, pin::Pin};

use async_trait::async_trait;
use futures_core::Stream;
use rw_types::Turn;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// A provider-neutral request assembled by the engine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderRequest {
    /// Provider-local model name. The router replaces aliases before dispatch.
    pub model: String,
    /// Conversation in Rottweiler's shared message IR.
    pub turns: Vec<Turn>,
    /// Function tools exposed for this turn.
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    /// Whether the model may, must, must not, or must specifically call a tool.
    #[serde(default)]
    pub tool_choice: ToolChoice,
    /// Maximum number of output tokens.
    pub max_output_tokens: u32,
    /// Optional sampling temperature.
    pub temperature: Option<f32>,
    /// Provider-independent reasoning control.
    pub thinking: ThinkingLevel,
}

/// Provider-neutral tool selection policy.
///
/// Adapters translate this into each provider's wire shape. A named choice is
/// validated against [`ProviderRequest::tools`] before a request is sent.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum ToolChoice {
    /// The model may answer normally or call any exposed tool.
    #[default]
    Auto,
    /// The model must call at least one exposed tool.
    Required,
    /// The model must not call a tool, even when tools are exposed.
    None,
    /// The model must call this exact function tool.
    Named {
        /// Name of a function present in [`ProviderRequest::tools`].
        name: String,
    },
}

impl ProviderRequest {
    /// Validates the provider-independent invariants of [`Self::tool_choice`].
    ///
    /// # Errors
    ///
    /// Returns a sanitized invalid-request error when a required or named tool
    /// choice cannot be satisfied by this request's tool definitions.
    pub(crate) fn validate_tool_choice(&self) -> Result<(), ProviderError> {
        match &self.tool_choice {
            ToolChoice::Required if self.tools.is_empty() => Err(ProviderError::new(
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
            ToolChoice::Auto
            | ToolChoice::Required
            | ToolChoice::None
            | ToolChoice::Named { .. } => Ok(()),
        }
    }
}

/// A function tool definition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name presented to the model.
    pub name: String,
    /// Human-readable behavior summary.
    pub description: String,
    /// JSON Schema accepted by the tool.
    pub input_schema: Value,
}

/// User-facing reasoning effort.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    /// Do not request hidden reasoning.
    #[default]
    Off,
    /// Prefer the smallest supported reasoning budget.
    Low,
    /// Use a balanced reasoning budget.
    Medium,
    /// Prefer the provider's largest generally supported reasoning budget.
    High,
}

/// Prompt-cache behavior offered by an adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheBreakpointSupport {
    /// The API has no explicit cache breakpoint controls.
    None,
    /// The API accepts explicit cache breakpoint markers.
    Explicit,
    /// The provider manages caching without client markers.
    Automatic,
}

/// Capabilities used by the engine to degrade gracefully.
#[derive(Clone, Debug, Eq, PartialEq)]
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
    /// Already-normalized replay events.
    NormalizedReplay,
}

/// Provider usage normalized across API families.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Non-cached input tokens.
    pub input_tokens: u64,
    /// Generated tokens.
    pub output_tokens: u64,
    /// Input tokens served from a prompt cache.
    pub cache_read_tokens: u64,
    /// Input tokens written into a prompt cache.
    pub cache_write_tokens: u64,
    /// Reasoning tokens when separately reported.
    pub reasoning_tokens: u64,
}

/// Why a model stream ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderEvent {
    /// Provider accepted the message and selected a concrete model.
    MessageStart { model: String },
    /// Visible assistant text.
    TextDelta { text: String },
    /// Hidden or summarized model reasoning.
    ThinkingDelta {
        content: String,
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
        title: Option<String>,
        start_index: Option<u64>,
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
    /// Stable provider key used by qualified model ids.
    fn name(&self) -> &str;

    /// Declared features for graceful engine degradation.
    fn capabilities(&self) -> Capabilities;

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
