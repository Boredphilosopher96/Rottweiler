use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use base64::Engine as _;
use futures_util::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use rw_types::{Block, ImageRef, Role, ToolOutput, ToolOutputPart, Turn};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use url::Url;

use crate::types::RawSseFrame;
use crate::{
    AuthProvider, BoxEventStream, CacheBreakpointSupport, Capabilities, DiscoveredModel,
    DiscoveredProviderCatalog, FinishReason, NativeWebSearchCapability, NetworkPolicy, Provider,
    ProviderError, ProviderErrorKind, ProviderEvent, ProviderRequest, ProxyAuthentication,
    ThinkingLevel, TokenUsage, ToolChoice, WireFrameSink, WireMode,
    http::{build_client_with_proxy_auth, require_network, response_error, transport_error},
    sse::{SseDecoder, SseEvent},
};

const MAX_TOOL_ARGUMENT_BYTES: usize = 1_048_576;
const MAX_MODEL_CATALOG_BYTES: usize = 4 * 1024 * 1024;
const RESPONSES_REASONING_SIGNATURE_PREFIX: &str = "openai.responses.reasoning.v1:";
const CHAT_REASONING_SIGNATURE_PREFIX: &str = "openai.chat.reasoning.v1:";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ResponsesReasoningSignature {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypted_content: Option<String>,
}

/// OpenAI-compatible API dialect.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OpenAiWireMode {
    /// Widely supported `/chat/completions` protocol.
    #[default]
    ChatCompletions,
    /// `OpenAI` `/responses` protocol.
    Responses,
}

/// Runtime settings for an OpenAI-compatible endpoint.
#[derive(Clone, Debug)]
pub struct OpenAiCompatibleConfig {
    /// Router provider key.
    pub name: String,
    /// Exact endpoint (`.../chat/completions` or `.../responses`).
    pub endpoint: Url,
    /// API key, OAuth source, or unauthenticated local endpoint.
    pub auth: Arc<dyn AuthProvider>,
    /// Already-resolved proxy for this provider.
    pub proxy: Option<Url>,
    /// Optional HTTP Basic credentials for the resolved proxy.
    pub proxy_authentication: Option<ProxyAuthentication>,
    /// Live/replay network guard.
    pub network_policy: NetworkPolicy,
    /// Selected request/stream dialect.
    pub wire_mode: OpenAiWireMode,
    /// Whether this endpoint supports function tool calls.
    pub tool_calling: bool,
    /// Prompt-cache behavior declared by this endpoint.
    pub cache_breakpoints: CacheBreakpointSupport,
    /// Exact reasoning efforts accepted by this endpoint. An empty list means
    /// it is not a reasoning endpoint. Reasoning-only models may omit
    /// [`ThinkingLevel::Off`] when the wire API does not accept `none`.
    pub supported_reasoning_efforts: Vec<ThinkingLevel>,
    /// Whether images are accepted.
    pub supports_vision: bool,
    /// Known context limit.
    pub max_context_tokens: Option<u64>,
    /// Known output limit.
    pub max_output_tokens: Option<u64>,
}

/// `OpenAI` Chat Completions / Responses adapter.
pub struct OpenAiCompatibleProvider {
    config: OpenAiCompatibleConfig,
    client: reqwest::Client,
}

impl std::fmt::Debug for OpenAiCompatibleProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiCompatibleProvider")
            .field("name", &self.config.name)
            .field("endpoint", &self.config.endpoint)
            .field("wire_mode", &self.config.wire_mode)
            .field("network_policy", &self.config.network_policy)
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatibleProvider {
    /// Builds an adapter and deterministic proxy-configured HTTP client.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured proxy cannot initialize an HTTP client.
    pub fn new(config: OpenAiCompatibleConfig) -> Result<Self, ProviderError> {
        let client = build_client_with_proxy_auth(
            config.proxy.as_ref(),
            config.proxy_authentication.as_ref(),
        )?;
        Ok(Self { config, client })
    }

    async fn stream_impl(
        &self,
        request: ProviderRequest,
        wire_sink: Option<Arc<dyn WireFrameSink>>,
    ) -> Result<BoxEventStream, ProviderError> {
        self.validate_request_capabilities(&request)?;
        require_network(self.config.network_policy)?;
        let reasoning_endpoint = !self.config.supported_reasoning_efforts.is_empty();
        let material = self.config.auth.material().await?;
        let mut body = match self.config.wire_mode {
            OpenAiWireMode::ChatCompletions => build_chat_request(&request, reasoning_endpoint),
            OpenAiWireMode::Responses => build_responses_request(&request, reasoning_endpoint),
        };
        apply_auth_request_shape(&mut body, &material);
        if let Some(session_id) = material.openai_subscription_session_id() {
            if self.config.wire_mode != OpenAiWireMode::Responses {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "ChatGPT subscription authentication requires Responses wire mode",
                ));
            }
            apply_subscription_request_shape(&mut body, session_id)?;
        }
        let mut headers = HeaderMap::new();
        material.apply_openai(&mut headers)?;
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let response = self
            .client
            .post(self.config.endpoint.clone())
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;
        if let Some(generic) = response_error(&response) {
            let classified = bounded_error_json(response)
                .await
                .map(|value| openai_stream_error(&value))
                .filter(|error| error.kind == ProviderErrorKind::ContextOverflow);
            return Err(classified.unwrap_or(generic));
        }
        let chunks = response.bytes_stream();
        let wire_mode = self.config.wire_mode;
        let stream = async_stream::try_stream! {
            let mut chunks = chunks;
            let mut decoder = SseDecoder::default();
            let mut state = OpenAiState::new(wire_mode);
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.map_err(transport_error)?;
                for event in decoder.push(&chunk)? {
                    if let Some(sink) = &wire_sink {
                        sink.capture(event.event.as_deref(), &event.data);
                    }
                    for normalized in state.handle(&event)? {
                        yield normalized;
                    }
                }
            }
            for event in decoder.finish()? {
                if let Some(sink) = &wire_sink {
                    sink.capture(event.event.as_deref(), &event.data);
                }
                for normalized in state.handle(&event)? {
                    yield normalized;
                }
            }
            if !state.finished {
                Err(ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "OpenAI stream ended before its terminal event",
                ))?;
            }
        };
        Ok(Box::pin(stream))
    }

    fn validate_request_capabilities(
        &self,
        request: &ProviderRequest,
    ) -> Result<(), ProviderError> {
        request.validate_tool_choice()?;
        let native_search = request
            .tools
            .iter()
            .map(crate::types::native_web_search_request)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        if native_search.len() > 1 || (!native_search.is_empty() && request.tools.len() != 1) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "provider-native web search must be the only request tool",
            ));
        }
        if let Some(search) = native_search.first() {
            search.validate_for(self.native_web_search_capability())?;
        }
        if !self.config.tool_calling && !request.tools.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::Unsupported,
                "configured OpenAI-compatible endpoint does not support function tools",
            ));
        }
        if !self.config.supports_vision && request_has_image(request) {
            return Err(ProviderError::new(
                ProviderErrorKind::Unsupported,
                "configured OpenAI-compatible endpoint does not support image input",
            ));
        }
        let reasoning_endpoint = !self.config.supported_reasoning_efforts.is_empty();
        let unsupported_reasoning = if reasoning_endpoint {
            !self
                .config
                .supported_reasoning_efforts
                .contains(&request.thinking)
        } else {
            request.thinking != ThinkingLevel::Off
        };
        if unsupported_reasoning {
            return Err(ProviderError::new(
                ProviderErrorKind::Unsupported,
                format!(
                    "configured OpenAI-compatible endpoint does not support {:?} reasoning effort",
                    request.thinking
                ),
            ));
        }
        Ok(())
    }

    async fn discover_models_impl(
        &self,
    ) -> Result<Option<DiscoveredProviderCatalog>, ProviderError> {
        require_network(self.config.network_policy)?;
        let material = self.config.auth.material().await?;
        let subscription = matches!(material, crate::AuthMaterial::OpenAiSubscription { .. });
        let endpoint = discovery_endpoint(&self.config.endpoint, subscription)?;
        let optional_loopback_catalog = is_loopback(&endpoint) && !subscription;
        let mut headers = HeaderMap::new();
        material.apply_openai(&mut headers)?;
        if subscription {
            headers.insert(
                HeaderName::from_static("version"),
                HeaderValue::from_static(crate::OPENAI_SUBSCRIPTION_MODELS_COMPATIBILITY_VERSION),
            );
        }
        let response = self
            .client
            .get(endpoint)
            .headers(headers)
            .send()
            .await
            .map_err(transport_error)?;
        if optional_loopback_catalog
            && matches!(
                response.status(),
                reqwest::StatusCode::NOT_FOUND
                    | reqwest::StatusCode::METHOD_NOT_ALLOWED
                    | reqwest::StatusCode::NOT_IMPLEMENTED
            )
        {
            // Local OpenAI-compatible servers frequently expose inference
            // without implementing `GET /models`. That is a static route, not
            // a failed live catalog. Public and subscription endpoints remain
            // authoritative and still fail closed on the same statuses.
            return Ok(None);
        }
        if let Some(error) = response_error(&response) {
            return Err(error);
        }
        let bytes = bounded_catalog_bytes(response).await?;
        let models = if subscription {
            parse_subscription_models(&bytes)?
        } else {
            parse_openai_models(&bytes)?
        };
        Ok(Some(DiscoveredProviderCatalog {
            provider: self.config.name.clone(),
            models,
        }))
    }
}

fn request_has_image(request: &ProviderRequest) -> bool {
    request.turns.iter().any(|turn| {
        turn.blocks.iter().any(|block| match block {
            Block::Image { .. } => true,
            Block::ToolResult {
                output: ToolOutput::Mixed { parts },
                ..
            } => parts
                .iter()
                .any(|part| matches!(part, ToolOutputPart::Image { .. })),
            Block::Text { .. }
            | Block::Thinking { .. }
            | Block::ToolCall { .. }
            | Block::ToolResult { .. }
            | Block::Citation { .. } => false,
        })
    })
}

fn discovery_endpoint(endpoint: &Url, subscription: bool) -> Result<Url, ProviderError> {
    if subscription && !is_loopback(endpoint) {
        let mut discovered =
            Url::parse(crate::OPENAI_SUBSCRIPTION_MODELS_ENDPOINT).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "built-in ChatGPT model catalog endpoint is invalid",
                )
            })?;
        append_subscription_catalog_version(&mut discovered);
        return Ok(discovered);
    }
    let path = endpoint.path();
    let base = path
        .strip_suffix("/chat/completions")
        .or_else(|| path.strip_suffix("/responses"))
        .unwrap_or_else(|| path.trim_end_matches('/'));
    let model_path = if subscription {
        "/backend-api/codex/models".to_owned()
    } else {
        format!("{base}/models")
    };
    let mut discovered = endpoint.clone();
    discovered.set_path(&model_path);
    discovered.set_query(None);
    discovered.set_fragment(None);
    if subscription {
        append_subscription_catalog_version(&mut discovered);
    }
    Ok(discovered)
}

fn append_subscription_catalog_version(endpoint: &mut Url) {
    endpoint.query_pairs_mut().append_pair(
        "client_version",
        crate::OPENAI_SUBSCRIPTION_MODELS_COMPATIBILITY_VERSION,
    );
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

async fn bounded_catalog_bytes(response: reqwest::Response) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MODEL_CATALOG_BYTES as u64)
    {
        return Err(model_catalog_too_large());
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport_error)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_MODEL_CATALOG_BYTES {
            return Err(model_catalog_too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn model_catalog_too_large() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Protocol,
        "OpenAI-compatible model discovery response exceeded the size limit",
    )
}

fn parse_openai_models(bytes: &[u8]) -> Result<Vec<DiscoveredModel>, ProviderError> {
    let envelope: Value = serde_json::from_slice(bytes).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            "OpenAI-compatible model discovery returned invalid JSON",
        )
    })?;
    let data = envelope
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                "OpenAI-compatible model discovery returned an invalid envelope",
            )
        })?;
    let models = data
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .filter_map(nonempty)
        .map(|id| DiscoveredModel {
            id: id.to_owned(),
            display_name: None,
            description: None,
            capabilities: None,
            pricing: None,
        })
        .map(|model| (model.id.clone(), model))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect();
    Ok(models)
}

fn parse_subscription_models(bytes: &[u8]) -> Result<Vec<DiscoveredModel>, ProviderError> {
    let envelope: Value = serde_json::from_slice(bytes).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            "ChatGPT model discovery returned invalid JSON",
        )
    })?;
    let data = envelope
        .as_array()
        .or_else(|| envelope.get("models").and_then(Value::as_array))
        .or_else(|| envelope.get("data").and_then(Value::as_array))
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                "ChatGPT model discovery returned an invalid envelope",
            )
        })?;
    Ok(data
        .iter()
        .filter(|model| {
            model
                .get("visibility")
                .and_then(Value::as_str)
                .is_none_or(|visibility| visibility == "list")
        })
        .filter_map(subscription_model)
        .map(|model| (model.id.clone(), model))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect())
}

fn subscription_model(model: &Value) -> Option<DiscoveredModel> {
    let id = model
        .get("slug")
        .or_else(|| model.get("id"))
        .and_then(Value::as_str)
        .and_then(nonempty)?;
    let context = model
        .get("context_window")
        .or_else(|| model.get("max_context_tokens"))
        .and_then(Value::as_u64);
    let output = model.get("max_output_tokens").and_then(Value::as_u64);
    let vision = model
        .get("input_modalities")
        .and_then(Value::as_array)
        .is_some_and(|modalities| {
            modalities
                .iter()
                .any(|value| value.as_str() == Some("image"))
        });
    let thinking = model
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .is_some_and(|levels| !levels.is_empty());
    Some(DiscoveredModel {
        id: id.to_owned(),
        display_name: string_field(model, "display_name"),
        description: string_field(model, "description"),
        capabilities: Some(Capabilities {
            tool_calling: model
                .get("supports_tool_calls")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            vision,
            thinking,
            cache_breakpoints: CacheBreakpointSupport::Automatic,
            max_context_tokens: context,
            max_output_tokens: output,
            wire_mode: WireMode::OpenAiResponses,
        }),
        pricing: None,
    })
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .and_then(nonempty)
        .map(str::to_owned)
}

fn nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn apply_auth_request_shape(body: &mut Value, material: &crate::AuthMaterial) {
    if material.omit_max_output_tokens()
        && let Some(object) = body.as_object_mut()
    {
        object.remove("max_completion_tokens");
        object.remove("max_output_tokens");
    }
}

fn apply_subscription_request_shape(
    body: &mut Value,
    session_id: &str,
) -> Result<(), ProviderError> {
    let object = body.as_object_mut().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            "ChatGPT subscription request body was not an object",
        )
    })?;
    let mut system_parts = Vec::new();
    if let Some(input) = object.get_mut("input").and_then(Value::as_array_mut) {
        input.retain(|item| {
            let is_system = item.get("role").and_then(Value::as_str) == Some("system");
            if is_system && let Some(content) = item.get("content").and_then(Value::as_str) {
                system_parts.push(content.to_owned());
            }
            !is_system
        });
    }
    object.insert("store".to_owned(), Value::Bool(false));
    object.remove("max_output_tokens");
    object.insert(
        "instructions".to_owned(),
        Value::String(system_parts.join("\n\n")),
    );
    object.insert(
        "prompt_cache_key".to_owned(),
        Value::String(session_id.to_owned()),
    );
    object.insert("include".to_owned(), json!(["reasoning.encrypted_content"]));
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if let Some(tool) = tool.as_object_mut() {
                tool.insert("strict".to_owned(), Value::Bool(false));
            }
        }
    }
    Ok(())
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calling: self.config.tool_calling,
            vision: self.config.supports_vision,
            thinking: !self.config.supported_reasoning_efforts.is_empty(),
            cache_breakpoints: self.config.cache_breakpoints,
            max_context_tokens: self.config.max_context_tokens,
            max_output_tokens: self.config.max_output_tokens,
            wire_mode: match self.config.wire_mode {
                OpenAiWireMode::ChatCompletions => WireMode::OpenAiChatCompletions,
                OpenAiWireMode::Responses => WireMode::OpenAiResponses,
            },
        }
    }

    fn native_web_search_capability(&self) -> NativeWebSearchCapability {
        if self.config.wire_mode == OpenAiWireMode::Responses
            && self.config.endpoint.host_str() == Some("api.openai.com")
        {
            NativeWebSearchCapability::Supported
        } else {
            NativeWebSearchCapability::Unsupported
        }
    }

    async fn discover_models(&self) -> Result<Option<DiscoveredProviderCatalog>, ProviderError> {
        self.discover_models_impl().await
    }

    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        self.stream_impl(request, None).await
    }

    async fn stream_with_wire_sink(
        &self,
        request: ProviderRequest,
        sink: Arc<dyn WireFrameSink>,
    ) -> Result<BoxEventStream, ProviderError> {
        self.stream_impl(request, Some(sink)).await
    }
}

fn build_chat_request(request: &ProviderRequest, reasoning_endpoint: bool) -> Value {
    let messages = request
        .turns
        .iter()
        .flat_map(chat_messages)
        .collect::<Vec<_>>();
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect::<Vec<_>>();
    let mut object = Map::from_iter([
        ("model".to_owned(), json!(request.model)),
        ("messages".to_owned(), Value::Array(messages)),
        ("stream".to_owned(), Value::Bool(true)),
        (
            "stream_options".to_owned(),
            json!({ "include_usage": true }),
        ),
        (
            "max_completion_tokens".to_owned(),
            json!(request.max_output_tokens),
        ),
    ]);
    if !tools.is_empty() {
        object.insert("tools".to_owned(), Value::Array(tools));
    }
    if !request.tools.is_empty() || request.tool_choice == ToolChoice::None {
        let tool_choice = match &request.tool_choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::Required => json!("required"),
            ToolChoice::None => json!("none"),
            ToolChoice::Named { name } => {
                json!({ "type": "function", "function": { "name": name } })
            }
        };
        object.insert("tool_choice".to_owned(), tool_choice);
    }
    if let Some(temperature) = request.temperature {
        object.insert("temperature".to_owned(), json!(temperature));
    }
    if reasoning_endpoint {
        object.insert(
            "reasoning_effort".to_owned(),
            json!(openai_effort(request.thinking)),
        );
    }
    Value::Object(object)
}

fn build_responses_request(request: &ProviderRequest, reasoning_endpoint: bool) -> Value {
    let mut input = Vec::new();
    for turn in &request.turns {
        input.extend(responses_items(turn));
    }
    let native_search = request
        .tools
        .iter()
        .find_map(|tool| crate::types::native_web_search_request(tool).ok().flatten());
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            if let Ok(Some(search)) = crate::types::native_web_search_request(tool) {
                if search.allowed_domains.is_empty() {
                    json!({"type":"web_search"})
                } else {
                    json!({"type":"web_search", "filters":{"allowed_domains":search.allowed_domains}})
                }
            } else {
                json!({
                    "type": "function", "name": tool.name,
                    "description": tool.description, "parameters": tool.input_schema,
                })
            }
        })
        .collect::<Vec<_>>();
    let mut object = Map::from_iter([
        ("model".to_owned(), json!(request.model)),
        ("input".to_owned(), Value::Array(input)),
        ("stream".to_owned(), Value::Bool(true)),
        (
            "max_output_tokens".to_owned(),
            json!(request.max_output_tokens),
        ),
    ]);
    if !tools.is_empty() {
        object.insert("tools".to_owned(), Value::Array(tools));
    }
    if native_search.is_some() {
        object.insert(
            "include".to_owned(),
            json!(["web_search_call.action.sources"]),
        );
    }
    if !request.tools.is_empty() || request.tool_choice == ToolChoice::None {
        let tool_choice = match &request.tool_choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::Required => json!("required"),
            ToolChoice::None => json!("none"),
            ToolChoice::Named { name } => json!({ "type": "function", "name": name }),
        };
        object.insert("tool_choice".to_owned(), tool_choice);
    }
    if let Some(temperature) = request.temperature {
        object.insert("temperature".to_owned(), json!(temperature));
    }
    if reasoning_endpoint {
        object.insert(
            "reasoning".to_owned(),
            json!({ "effort": openai_effort(request.thinking), "summary": "auto" }),
        );
        object.insert("include".to_owned(), json!(["reasoning.encrypted_content"]));
    }
    Value::Object(object)
}

const fn openai_effort(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "none",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
    }
}

fn chat_messages(turn: &Turn) -> Vec<Value> {
    let role = match turn.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut text = String::new();
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();
    let mut messages = Vec::new();
    let mut reasoning_content = String::new();
    let mut reasoning_opaque = None;
    for block in &turn.blocks {
        match block {
            Block::Text { text: value } => {
                text.push_str(value);
                content.push(json!({ "type": "text", "text": value }));
            }
            Block::Image { media_type, data } => {
                let url = match data {
                    ImageRef::InlineBase64 { data } => {
                        format!("data:{media_type};base64,{data}")
                    }
                    ImageRef::Url { url } => url.clone(),
                };
                content.push(json!({ "type": "image_url", "image_url": { "url": url } }));
            }
            Block::ToolCall { id, name, args } => tool_calls.push(json!({
                "id": id.0, "type": "function",
                "function": { "name": name, "arguments": args.to_string() },
            })),
            Block::ToolResult { id, output, .. } => messages.push(json!({
                "role": "tool", "tool_call_id": id.0, "content": tool_output_text(output),
            })),
            Block::Thinking { content, signature } => {
                reasoning_content.push_str(content);
                reasoning_opaque = signature
                    .as_deref()
                    .and_then(decode_chat_reasoning_signature);
            }
            Block::Citation { .. } => {}
        }
    }
    if !text.is_empty()
        || !content.is_empty()
        || !tool_calls.is_empty()
        || !reasoning_content.is_empty()
        || reasoning_opaque.is_some()
    {
        let mut message = Map::from_iter([("role".to_owned(), json!(role))]);
        if content.iter().any(|part| part["type"] == "image_url") {
            message.insert("content".to_owned(), Value::Array(content));
        } else {
            message.insert("content".to_owned(), json!(text));
        }
        if !tool_calls.is_empty() {
            message.insert("tool_calls".to_owned(), Value::Array(tool_calls));
        }
        if !reasoning_content.is_empty() {
            message.insert("reasoning_content".to_owned(), json!(reasoning_content));
        }
        if let Some(reasoning_opaque) = reasoning_opaque {
            message.insert("reasoning_opaque".to_owned(), json!(reasoning_opaque));
        }
        messages.insert(0, Value::Object(message));
    }
    messages
}

fn encode_chat_reasoning_signature(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| {
        format!(
            "{CHAT_REASONING_SIGNATURE_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.as_bytes())
        )
    })
}

fn decode_chat_reasoning_signature(value: &str) -> Option<String> {
    let payload = value.strip_prefix(CHAT_REASONING_SIGNATURE_PREFIX)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    String::from_utf8(bytes)
        .ok()
        .filter(|value| !value.is_empty())
}

fn responses_items(turn: &Turn) -> Vec<Value> {
    let role = match turn.role {
        Role::System => "system",
        Role::User | Role::Tool => "user",
        Role::Assistant => "assistant",
    };
    let mut text = String::new();
    let mut items = Vec::new();
    for block in &turn.blocks {
        match block {
            Block::Text { text: value } => text.push_str(value),
            Block::ToolCall { id, name, args } => items.push(json!({
                "type": "function_call", "call_id": id.0, "name": name,
                "arguments": args.to_string(),
            })),
            Block::ToolResult { id, output, .. } => items.push(json!({
                "type": "function_call_output", "call_id": id.0,
                "output": tool_output_text(output),
            })),
            Block::Image { media_type, data } => {
                let url = match data {
                    ImageRef::InlineBase64 { data } => {
                        format!("data:{media_type};base64,{data}")
                    }
                    ImageRef::Url { url } => url.clone(),
                };
                items.push(json!({
                    "role": role, "content": [{ "type": "input_image", "image_url": url }],
                }));
            }
            Block::Thinking { content, signature } => {
                let Some(signature) = signature
                    .as_deref()
                    .and_then(decode_responses_reasoning_signature)
                else {
                    continue;
                };
                let summary = if content.is_empty() {
                    Vec::new()
                } else {
                    vec![json!({ "type": "summary_text", "text": content })]
                };
                let mut item = Map::from_iter([
                    ("type".to_owned(), json!("reasoning")),
                    ("id".to_owned(), json!(signature.id)),
                    ("summary".to_owned(), Value::Array(summary)),
                ]);
                if let Some(encrypted_content) = signature.encrypted_content {
                    item.insert("encrypted_content".to_owned(), json!(encrypted_content));
                }
                items.push(Value::Object(item));
            }
            Block::Citation { .. } => {}
        }
    }
    if !text.is_empty() {
        items.insert(0, json!({ "role": role, "content": text }));
    }
    items
}

fn tool_output_text(output: &ToolOutput) -> String {
    match output {
        ToolOutput::Text { text } => text.clone(),
        ToolOutput::Structured { value } => value.to_string(),
        ToolOutput::Mixed { parts } => serde_json::to_string(parts).unwrap_or_default(),
    }
}

fn encode_responses_reasoning_signature(signature: &ResponsesReasoningSignature) -> Option<String> {
    if signature.id.trim().is_empty() {
        return None;
    }
    let payload = serde_json::to_string(signature).ok()?;
    Some(format!("{RESPONSES_REASONING_SIGNATURE_PREFIX}{payload}"))
}

fn decode_responses_reasoning_signature(value: &str) -> Option<ResponsesReasoningSignature> {
    let payload = value.strip_prefix(RESPONSES_REASONING_SIGNATURE_PREFIX)?;
    let signature: ResponsesReasoningSignature = serde_json::from_str(payload).ok()?;
    (!signature.id.trim().is_empty()).then_some(signature)
}

struct OpenAiState {
    wire_mode: OpenAiWireMode,
    tools: BTreeMap<(u64, u64), OpenAiToolState>,
    reasoning: BTreeMap<u64, OpenAiReasoningState>,
    started: bool,
    finished: bool,
    finish_reason: Option<FinishReason>,
}

struct OpenAiToolState {
    id: String,
    name: String,
    arguments: String,
    emitted_start: bool,
}

struct OpenAiReasoningState {
    signature: ResponsesReasoningSignature,
    streamed_summary: String,
}

impl OpenAiState {
    fn new(wire_mode: OpenAiWireMode) -> Self {
        Self {
            wire_mode,
            tools: BTreeMap::new(),
            reasoning: BTreeMap::new(),
            started: false,
            finished: false,
            finish_reason: None,
        }
    }

    fn handle(&mut self, event: &SseEvent) -> Result<Vec<ProviderEvent>, ProviderError> {
        match self.wire_mode {
            OpenAiWireMode::ChatCompletions => self.handle_chat(event),
            OpenAiWireMode::Responses => self.handle_responses(event),
        }
    }

    fn handle_chat(&mut self, event: &SseEvent) -> Result<Vec<ProviderEvent>, ProviderError> {
        if event.data == "[DONE]" {
            let mut events = self.finish_tools()?;
            self.finished = true;
            events.push(ProviderEvent::Finished {
                reason: self.finish_reason.unwrap_or(FinishReason::Stop),
            });
            return Ok(events);
        }
        let value: Value = parse_openai_json(&event.data)?;
        if value.get("error").is_some() {
            return Err(openai_stream_error(&value));
        }
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            events.push(ProviderEvent::MessageStart {
                model: value["model"].as_str().unwrap_or_default().to_owned(),
            });
        }
        for choice in value["choices"].as_array().into_iter().flatten() {
            let choice_index = choice["index"].as_u64().unwrap_or_default();
            let delta = &choice["delta"];
            if let Some(text) = delta["content"].as_str()
                && !text.is_empty()
            {
                events.push(ProviderEvent::TextDelta {
                    text: text.to_owned(),
                });
            }
            let reasoning = delta["reasoning_content"]
                .as_str()
                .or_else(|| delta["reasoning"].as_str());
            let reasoning_opaque = delta["reasoning_opaque"]
                .as_str()
                .and_then(encode_chat_reasoning_signature);
            if reasoning.is_some_and(|content| !content.is_empty()) || reasoning_opaque.is_some() {
                events.push(ProviderEvent::ThinkingDelta {
                    content: reasoning.unwrap_or_default().to_owned(),
                    signature: reasoning_opaque,
                });
            }
            for tool in delta["tool_calls"].as_array().into_iter().flatten() {
                let tool_index = tool["index"].as_u64().unwrap_or_default();
                let key = (choice_index, tool_index);
                let state = self.tools.entry(key).or_insert_with(|| OpenAiToolState {
                    id: tool["id"].as_str().unwrap_or_default().to_owned(),
                    name: tool["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    arguments: String::new(),
                    emitted_start: false,
                });
                if let Some(id) = tool["id"].as_str()
                    && !id.is_empty()
                {
                    id.clone_into(&mut state.id);
                }
                if let Some(name) = tool["function"]["name"].as_str()
                    && !name.is_empty()
                {
                    name.clone_into(&mut state.name);
                }
                if !state.emitted_start && !state.id.is_empty() && !state.name.is_empty() {
                    state.emitted_start = true;
                    events.push(ProviderEvent::ToolCallStart {
                        id: state.id.clone(),
                        name: state.name.clone(),
                    });
                }
                if let Some(fragment) = tool["function"]["arguments"].as_str()
                    && !fragment.is_empty()
                {
                    append_arguments(&mut state.arguments, fragment)?;
                    events.push(ProviderEvent::ToolCallArgumentsDelta {
                        id: state.id.clone(),
                        json_fragment: fragment.to_owned(),
                    });
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.finish_reason = Some(map_finish(Some(reason)));
            }
        }
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            events.push(ProviderEvent::Usage {
                usage: parse_usage(usage),
            });
        }
        Ok(events)
    }

    #[allow(clippy::too_many_lines)]
    fn handle_responses(&mut self, event: &SseEvent) -> Result<Vec<ProviderEvent>, ProviderError> {
        let value: Value = parse_openai_json(&event.data)?;
        let kind = event
            .event
            .as_deref()
            .or_else(|| value["type"].as_str())
            .unwrap_or_default();
        match kind {
            "response.created" => {
                self.started = true;
                Ok(vec![ProviderEvent::MessageStart {
                    model: value["response"]["model"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                }])
            }
            "response.output_text.delta" => Ok(vec![ProviderEvent::TextDelta {
                text: value["delta"].as_str().unwrap_or_default().to_owned(),
            }]),
            "response.reasoning_summary_text.delta" => {
                let index = value["output_index"].as_u64().unwrap_or_default();
                let content = value["delta"].as_str().unwrap_or_default().to_owned();
                let signature = self.reasoning.get_mut(&index).and_then(|state| {
                    state.streamed_summary.push_str(&content);
                    encode_responses_reasoning_signature(&state.signature)
                });
                Ok(vec![ProviderEvent::ThinkingDelta { content, signature }])
            }
            "response.output_item.added" if value["item"]["type"] == "reasoning" => {
                let index = value["output_index"].as_u64().unwrap_or_default();
                let Some(id) = value["item"]["id"]
                    .as_str()
                    .filter(|id| !id.trim().is_empty())
                else {
                    return Ok(Vec::new());
                };
                let signature = ResponsesReasoningSignature {
                    id: id.to_owned(),
                    encrypted_content: value["item"]["encrypted_content"]
                        .as_str()
                        .map(str::to_owned),
                };
                let opaque = encode_responses_reasoning_signature(&signature);
                self.reasoning.insert(
                    index,
                    OpenAiReasoningState {
                        signature,
                        streamed_summary: String::new(),
                    },
                );
                Ok(vec![ProviderEvent::ThinkingDelta {
                    content: String::new(),
                    signature: opaque,
                }])
            }
            "response.output_item.done" if value["item"]["type"] == "reasoning" => {
                let index = value["output_index"].as_u64().unwrap_or_default();
                let previous = self.reasoning.remove(&index);
                let id = value["item"]["id"]
                    .as_str()
                    .filter(|id| !id.trim().is_empty())
                    .map(str::to_owned)
                    .or_else(|| previous.as_ref().map(|state| state.signature.id.clone()));
                let streamed_summary = previous
                    .as_ref()
                    .map(|state| state.streamed_summary.as_str())
                    .unwrap_or_default();
                let final_summary = reasoning_summary(&value["item"]);
                let content = if streamed_summary.is_empty() {
                    final_summary
                } else {
                    String::new()
                };
                let signature = id.and_then(|id| {
                    encode_responses_reasoning_signature(&ResponsesReasoningSignature {
                        id,
                        encrypted_content: value["item"]["encrypted_content"]
                            .as_str()
                            .map(str::to_owned)
                            .or_else(|| {
                                previous.and_then(|state| state.signature.encrypted_content)
                            }),
                    })
                });
                if content.is_empty() && signature.is_none() {
                    Ok(Vec::new())
                } else {
                    Ok(vec![ProviderEvent::ThinkingDelta { content, signature }])
                }
            }
            "response.output_item.done" if value["item"]["type"] == "web_search_call" => Ok(value
                ["item"]["action"]["sources"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|source| {
                    let uri = source["url"].as_str()?.to_owned();
                    Some(ProviderEvent::Citation {
                        uri,
                        title: source["title"].as_str().map(str::to_owned),
                        start_index: None,
                        end_index: None,
                    })
                })
                .collect()),
            "response.output_item.added" if value["item"]["type"] == "function_call" => {
                self.finish_reason = Some(FinishReason::ToolCalls);
                let index = value["output_index"].as_u64().unwrap_or_default();
                let id = value["item"]["call_id"]
                    .as_str()
                    .or_else(|| value["item"]["id"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                let name = value["item"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                self.tools.insert(
                    (0, index),
                    OpenAiToolState {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                        emitted_start: true,
                    },
                );
                Ok(vec![ProviderEvent::ToolCallStart { id, name }])
            }
            "response.function_call_arguments.delta" => {
                let index = value["output_index"].as_u64().unwrap_or_default();
                let fragment = value["delta"].as_str().unwrap_or_default();
                let state = self.tools.get_mut(&(0, index)).ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::Protocol,
                        "Responses function arguments arrived before output item",
                    )
                })?;
                append_arguments(&mut state.arguments, fragment)?;
                Ok(vec![ProviderEvent::ToolCallArgumentsDelta {
                    id: state.id.clone(),
                    json_fragment: fragment.to_owned(),
                }])
            }
            "response.function_call_arguments.done" => {
                let index = value["output_index"].as_u64().unwrap_or_default();
                let state = self.tools.remove(&(0, index)).ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::Protocol,
                        "Responses function completion arrived before output item",
                    )
                })?;
                let raw = value["arguments"].as_str().unwrap_or(&state.arguments);
                Ok(vec![ProviderEvent::ToolCallEnd {
                    id: state.id,
                    arguments: parse_arguments(raw)?,
                }])
            }
            "response.output_text.annotation.added" => {
                let annotation = &value["annotation"];
                if annotation["type"] == "url_citation" {
                    Ok(vec![ProviderEvent::Citation {
                        uri: annotation["url"].as_str().unwrap_or_default().to_owned(),
                        title: annotation["title"].as_str().map(str::to_owned),
                        start_index: annotation["start_index"].as_u64(),
                        end_index: annotation["end_index"].as_u64(),
                    }])
                } else {
                    Ok(Vec::new())
                }
            }
            "response.completed" => {
                let mut events = self.finish_tools()?;
                events.push(ProviderEvent::Usage {
                    usage: parse_usage(&value["response"]["usage"]),
                });
                self.finished = true;
                events.push(ProviderEvent::Finished {
                    reason: self.finish_reason.unwrap_or(FinishReason::Stop),
                });
                Ok(events)
            }
            "response.failed" | "response.incomplete" | "error" => Err(openai_stream_error(&value)),
            _ => Ok(Vec::new()),
        }
    }

    fn finish_tools(&mut self) -> Result<Vec<ProviderEvent>, ProviderError> {
        let tools = std::mem::take(&mut self.tools);
        tools
            .into_values()
            .map(|tool| {
                Ok(ProviderEvent::ToolCallEnd {
                    id: tool.id,
                    arguments: parse_arguments(&tool.arguments)?,
                })
            })
            .collect()
    }
}

fn reasoning_summary(item: &Value) -> String {
    item["summary"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect()
}

fn append_arguments(target: &mut String, fragment: &str) -> Result<(), ProviderError> {
    if target.len().saturating_add(fragment.len()) > MAX_TOOL_ARGUMENT_BYTES {
        return Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            "provider tool arguments exceeded the 1 MiB safety limit",
        ));
    }
    target.push_str(fragment);
    Ok(())
}

fn parse_openai_json(data: &str) -> Result<Value, ProviderError> {
    serde_json::from_str(data).map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            format!("invalid OpenAI-compatible SSE JSON: {error}"),
        )
    })
}

fn parse_arguments(arguments: &str) -> Result<Value, ProviderError> {
    if arguments.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(arguments).map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            format!("tool arguments were not valid JSON: {error}"),
        )
    })
}

fn parse_usage(value: &Value) -> TokenUsage {
    let total_input_tokens = value["prompt_tokens"]
        .as_u64()
        .or_else(|| value["input_tokens"].as_u64())
        .unwrap_or_default();
    let cache_read_tokens = value["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .or_else(|| value["input_tokens_details"]["cached_tokens"].as_u64())
        .unwrap_or_default();
    let total_output_tokens = value["completion_tokens"]
        .as_u64()
        .or_else(|| value["output_tokens"].as_u64())
        .unwrap_or_default();
    let reasoning_tokens = value["completion_tokens_details"]["reasoning_tokens"]
        .as_u64()
        .or_else(|| value["output_tokens_details"]["reasoning_tokens"].as_u64())
        .unwrap_or_default();
    TokenUsage {
        input_tokens: total_input_tokens.saturating_sub(cache_read_tokens),
        output_tokens: total_output_tokens.saturating_sub(reasoning_tokens),
        cache_read_tokens,
        cache_write_tokens: 0,
        reasoning_tokens,
    }
}

fn openai_stream_error(value: &Value) -> ProviderError {
    let error = value
        .get("error")
        .filter(|error| error.is_object())
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("error"))
        })
        .unwrap_or(value);
    let kind = [error["code"].as_str(), error["type"].as_str()]
        .into_iter()
        .flatten()
        .find_map(classify_openai_error_code)
        .unwrap_or(ProviderErrorKind::Protocol);
    let message = match kind {
        ProviderErrorKind::ContextOverflow => "OpenAI context window exceeded",
        ProviderErrorKind::Authentication => "OpenAI-compatible authentication error",
        ProviderErrorKind::InvalidRequest => "OpenAI-compatible invalid request",
        ProviderErrorKind::RateLimited => "OpenAI-compatible rate limit exceeded",
        ProviderErrorKind::Server => "OpenAI-compatible server error",
        ProviderErrorKind::Protocol
        | ProviderErrorKind::Unsupported
        | ProviderErrorKind::ReplayMiss
        | ProviderErrorKind::Cancelled => "OpenAI-compatible protocol error",
        ProviderErrorKind::Timeout => "OpenAI-compatible request timed out",
        ProviderErrorKind::Network => "OpenAI-compatible network error",
        ProviderErrorKind::NetworkDisabled => "OpenAI-compatible network access disabled",
    };
    ProviderError::new(kind, message)
}

async fn bounded_error_json(response: reqwest::Response) -> Option<Value> {
    const MAX_ERROR_BYTES: usize = 64 * 1024;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ERROR_BYTES as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if bytes.len().saturating_add(chunk.len()) > MAX_ERROR_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).ok()
}

fn classify_openai_error_code(code: &str) -> Option<ProviderErrorKind> {
    match code {
        "authentication_error"
        | "invalid_api_key"
        | "incorrect_api_key"
        | "permission_error"
        | "insufficient_permissions"
        | "billing_error"
        | "insufficient_quota" => Some(ProviderErrorKind::Authentication),
        "invalid_request_error"
        | "bad_request"
        | "bad_request_error"
        | "model_not_found"
        | "invalid_prompt"
        | "invalid_value"
        | "unknown_parameter"
        | "unsupported_parameter"
        | "missing_required_parameter" => Some(ProviderErrorKind::InvalidRequest),
        "context_length_exceeded" => Some(ProviderErrorKind::ContextOverflow),
        "rate_limit_error" | "rate_limit_exceeded" => Some(ProviderErrorKind::RateLimited),
        "server_error" | "internal_server_error" | "overloaded_error" => {
            Some(ProviderErrorKind::Server)
        }
        _ => None,
    }
}

fn map_finish(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("tool_calls" | "function_call") => FinishReason::ToolCalls,
        Some("content_filter") => FinishReason::ContentFilter,
        Some(_) | None => FinishReason::Unknown,
    }
}

pub(crate) fn replay_sse_frames(
    wire_mode: OpenAiWireMode,
    frames: &[RawSseFrame],
) -> Vec<Result<ProviderEvent, ProviderError>> {
    let mut state = OpenAiState::new(wire_mode);
    let mut items = Vec::new();
    for frame in frames {
        let event = SseEvent {
            event: frame.event.clone(),
            data: frame.data.clone(),
        };
        match state.handle(&event) {
            Ok(events) => items.extend(events.into_iter().map(Ok)),
            Err(error) => {
                items.push(Err(error));
                return items;
            }
        }
    }
    if !state.finished {
        items.push(Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            "OpenAI replay ended before its terminal event",
        )));
    }
    items
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rw_types::{Block, ImageRef, Role, ToolCallId, ToolOutput, ToolOutputPart, Turn, TurnMeta};
    use serde_json::json;
    use url::Url;

    use crate::{
        AuthMaterial, CacheBreakpointSupport, NativeWebSearchRequest, NetworkPolicy,
        ProviderErrorKind, ProviderEvent, ProviderRequest, Secret, StaticAuth, ThinkingLevel,
        TokenUsage, ToolChoice, ToolDefinition, sse::SseEvent,
    };

    use super::{
        OpenAiCompatibleConfig, OpenAiCompatibleProvider, OpenAiState, OpenAiWireMode,
        ResponsesReasoningSignature, apply_auth_request_shape, apply_subscription_request_shape,
        build_chat_request, build_responses_request, decode_responses_reasoning_signature,
        discovery_endpoint, encode_responses_reasoning_signature, openai_stream_error, parse_usage,
        responses_items,
    };

    #[test]
    fn production_chatgpt_catalog_keeps_protocol_version_query() {
        let inference = Url::parse(crate::OPENAI_SUBSCRIPTION_RESPONSES_ENDPOINT)
            .unwrap_or_else(|error| panic!("built-in inference URL must parse: {error}"));
        let catalog = discovery_endpoint(&inference, true)
            .unwrap_or_else(|error| panic!("built-in catalog URL must compose: {error}"));

        assert_eq!(
            catalog.as_str(),
            format!(
                "{}?client_version={}",
                crate::OPENAI_SUBSCRIPTION_MODELS_ENDPOINT,
                crate::OPENAI_SUBSCRIPTION_MODELS_COMPATIBILITY_VERSION
            )
        );
    }

    #[test]
    fn chat_tool_fragments_and_usage_normalize() {
        let mut state = OpenAiState::new(OpenAiWireMode::ChatCompletions);
        let values = [
            json!({"model":"fixture","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read","arguments":"{\"path\":"}}]},"finish_reason":null}]}),
            json!({"model":"fixture","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"model":"fixture","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4}}),
        ];
        let mut events = Vec::new();
        for value in values {
            events.extend(
                state
                    .handle(&SseEvent {
                        event: None,
                        data: value.to_string(),
                    })
                    .unwrap_or_else(|error| panic!("chunk must parse: {error}")),
            );
        }
        events.extend(
            state
                .handle(&SseEvent {
                    event: None,
                    data: "[DONE]".to_owned(),
                })
                .unwrap_or_else(|error| panic!("done must parse: {error}")),
        );
        assert!(events.contains(&ProviderEvent::ToolCallEnd {
            id: "call-1".to_owned(),
            arguments: json!({"path":"a.rs"}),
        }));
        assert!(matches!(
            events.last(),
            Some(ProviderEvent::Finished { .. })
        ));
    }

    #[test]
    fn responses_text_reasoning_citation_and_usage_normalize() {
        let mut state = OpenAiState::new(OpenAiWireMode::Responses);
        let frames = [
            ("response.created", json!({"response":{"model":"fixture"}})),
            ("response.output_text.delta", json!({"delta":"hello"})),
            (
                "response.output_item.added",
                json!({"output_index":0,"item":{"type":"reasoning","id":"rs_fixture"}}),
            ),
            (
                "response.reasoning_summary_text.delta",
                json!({"output_index":0,"delta":"considering"}),
            ),
            (
                "response.output_item.done",
                json!({"output_index":0,"item":{"type":"reasoning","id":"rs_fixture","summary":[{"type":"summary_text","text":"considering"}],"encrypted_content":"encrypted-fixture"}}),
            ),
            (
                "response.output_text.annotation.added",
                json!({"annotation":{"type":"url_citation","url":"https://example.test","title":"Example","start_index":0,"end_index":5}}),
            ),
            (
                "response.output_item.done",
                json!({"item":{"type":"web_search_call","action":{"sources":[{"url":"https://source.example/path","title":"Source"}]}}}),
            ),
            (
                "response.completed",
                json!({"response":{"usage":{"input_tokens":8,"output_tokens":3,"output_tokens_details":{"reasoning_tokens":1}}}}),
            ),
        ];
        let mut events = Vec::new();
        for (kind, value) in frames {
            events.extend(
                state
                    .handle(&SseEvent {
                        event: Some(kind.to_owned()),
                        data: value.to_string(),
                    })
                    .unwrap_or_else(|error| panic!("response event must parse: {error}")),
            );
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::Citation { .. }))
        );
        assert!(events.iter().any(|event| matches!(event, ProviderEvent::Citation { uri, .. } if uri == "https://source.example/path")));
        let reasoning_signature = events.iter().rev().find_map(|event| match event {
            ProviderEvent::ThinkingDelta {
                signature: Some(signature),
                ..
            } => decode_responses_reasoning_signature(signature),
            _ => None,
        });
        assert_eq!(
            reasoning_signature,
            Some(ResponsesReasoningSignature {
                id: "rs_fixture".to_owned(),
                encrypted_content: Some("encrypted-fixture".to_owned()),
            })
        );
        assert!(matches!(
            events.last(),
            Some(ProviderEvent::Finished { .. })
        ));
    }

    #[test]
    fn responses_native_search_uses_official_tool_and_sources_shape() {
        let request = ProviderRequest {
            model: "gpt-fixture".to_owned(),
            turns: vec![Turn {
                role: Role::User,
                blocks: vec![Block::Text {
                    text: "rust lsp".to_owned(),
                }],
                meta: TurnMeta::default(),
            }],
            tools: vec![
                NativeWebSearchRequest {
                    query: "rust lsp".to_owned(),
                    max_results: 5,
                    recency_days: None,
                    allowed_domains: vec!["rust-lang.org".to_owned()],
                }
                .tool_definition()
                .unwrap_or_else(|error| panic!("native search marker must encode: {error}")),
            ],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 256,
            temperature: None,
            thinking: ThinkingLevel::Off,
            cache_hint: None,
        };
        let body = build_responses_request(&request, false);
        assert_eq!(body["tools"][0]["type"], "web_search");
        assert_eq!(
            body["tools"][0]["filters"]["allowed_domains"],
            json!(["rust-lang.org"])
        );
        assert_eq!(body["include"], json!(["web_search_call.action.sources"]));
        assert!(body.to_string().contains("rust lsp"));
    }

    #[test]
    fn responses_request_continues_only_valid_same_adapter_reasoning() {
        let opaque = encode_responses_reasoning_signature(&ResponsesReasoningSignature {
            id: "rs_previous".to_owned(),
            encrypted_content: Some("encrypted-previous".to_owned()),
        })
        .unwrap_or_else(|| panic!("valid reasoning signature must encode"));
        let turns = vec![Turn {
            role: Role::Assistant,
            blocks: vec![
                Block::Thinking {
                    content: "summary".to_owned(),
                    signature: Some(opaque),
                },
                Block::Thinking {
                    content: "must be ignored".to_owned(),
                    signature: Some("anthropic-or-corrupt-signature".to_owned()),
                },
            ],
            meta: TurnMeta::default(),
        }];
        let request = ProviderRequest {
            model: "gpt-fixture".to_owned(),
            turns: turns.clone(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 128,
            temperature: None,
            thinking: ThinkingLevel::Medium,
            cache_hint: None,
        };

        let body = build_responses_request(&request, true);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(
            body["input"],
            json!([{
                "type": "reasoning",
                "id": "rs_previous",
                "summary": [{"type": "summary_text", "text": "summary"}],
                "encrypted_content": "encrypted-previous",
            }])
        );
        assert_eq!(responses_items(&turns[0]).len(), 1);

        let mut disabled = request;
        disabled.thinking = ThinkingLevel::Off;
        let body = build_responses_request(&disabled, true);
        assert_eq!(body["reasoning"]["effort"], "none");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        let body = build_responses_request(&disabled, false);
        assert!(body.get("reasoning").is_none());
        assert!(body.get("include").is_none());
    }

    #[test]
    fn tool_choice_uses_each_openai_wire_shape() {
        let mut request = ProviderRequest {
            model: "gpt-fixture".to_owned(),
            turns: Vec::new(),
            tools: vec![ToolDefinition {
                name: "live_smoke_ping".to_owned(),
                description: "fixture".to_owned(),
                input_schema: json!({"type": "object"}),
            }],
            tool_choice: ToolChoice::Named {
                name: "live_smoke_ping".to_owned(),
            },
            max_output_tokens: 32,
            temperature: None,
            thinking: ThinkingLevel::Off,
            cache_hint: None,
        };

        assert_eq!(
            build_chat_request(&request, false)["tool_choice"],
            json!({"type":"function","function":{"name":"live_smoke_ping"}})
        );
        assert_eq!(
            build_responses_request(&request, false)["tool_choice"],
            json!({"type":"function","name":"live_smoke_ping"})
        );
        for (choice, expected) in [
            (ToolChoice::Auto, "auto"),
            (ToolChoice::Required, "required"),
            (ToolChoice::None, "none"),
        ] {
            request.tool_choice = choice;
            assert_eq!(build_chat_request(&request, false)["tool_choice"], expected);
            assert_eq!(
                build_responses_request(&request, false)["tool_choice"],
                expected
            );
        }
    }

    #[test]
    fn subscription_shape_moves_system_text_and_sets_codex_fields() {
        let request = ProviderRequest {
            model: "gpt-5.6".to_owned(),
            turns: vec![
                Turn {
                    role: Role::System,
                    blocks: vec![Block::Text {
                        text: "first system".to_owned(),
                    }],
                    meta: TurnMeta::default(),
                },
                Turn {
                    role: Role::System,
                    blocks: vec![Block::Text {
                        text: "second system".to_owned(),
                    }],
                    meta: TurnMeta::default(),
                },
                Turn {
                    role: Role::User,
                    blocks: vec![Block::Text {
                        text: "hello".to_owned(),
                    }],
                    meta: TurnMeta::default(),
                },
            ],
            tools: vec![ToolDefinition {
                name: "read_file".to_owned(),
                description: "read".to_owned(),
                input_schema: json!({"type":"object"}),
            }],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 321,
            temperature: None,
            thinking: ThinkingLevel::Medium,
            cache_hint: None,
        };
        let mut body = build_responses_request(&request, true);
        apply_subscription_request_shape(&mut body, "rw-session-fixture")
            .unwrap_or_else(|error| panic!("subscription shape must apply: {error}"));

        assert_eq!(body["instructions"], "first system\n\nsecond system");
        assert_eq!(body["store"], false);
        assert_eq!(body["prompt_cache_key"], "rw-session-fixture");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["tools"][0]["strict"], false);
        assert!(
            body["input"]
                .as_array()
                .is_some_and(|items| items.iter().all(|item| item["role"] != "system"))
        );

        let mut empty = build_responses_request(
            &ProviderRequest {
                turns: Vec::new(),
                tools: Vec::new(),
                ..request
            },
            false,
        );
        apply_subscription_request_shape(&mut empty, "rw-empty")
            .unwrap_or_else(|error| panic!("empty subscription shape must apply: {error}"));
        assert_eq!(empty["instructions"], "");
    }

    #[test]
    fn responses_reasoning_without_real_id_never_gets_an_opaque_signature() {
        let mut state = OpenAiState::new(OpenAiWireMode::Responses);
        let events = state
            .handle(&SseEvent {
                event: Some("response.output_item.done".to_owned()),
                data: json!({
                    "output_index": 0,
                    "item": {
                        "type": "reasoning",
                        "summary": [{"type":"summary_text","text":"visible summary"}],
                        "encrypted_content": "orphaned-encrypted-content"
                    }
                })
                .to_string(),
            })
            .unwrap_or_else(|error| panic!("reasoning event must parse: {error}"));
        assert_eq!(
            events,
            vec![ProviderEvent::ThinkingDelta {
                content: "visible summary".to_owned(),
                signature: None,
            }]
        );
    }

    #[test]
    fn usage_classes_are_disjoint_for_chat_and_responses() {
        for usage in [
            json!({
                "prompt_tokens": 13,
                "completion_tokens": 7,
                "prompt_tokens_details": {"cached_tokens": 2},
                "completion_tokens_details": {"reasoning_tokens": 1}
            }),
            json!({
                "input_tokens": 17,
                "output_tokens": 6,
                "input_tokens_details": {"cached_tokens": 4},
                "output_tokens_details": {"reasoning_tokens": 2}
            }),
        ] {
            let normalized = parse_usage(&usage);
            assert_eq!(
                normalized.input_tokens + normalized.cache_read_tokens,
                usage["prompt_tokens"]
                    .as_u64()
                    .or_else(|| usage["input_tokens"].as_u64())
                    .unwrap_or_default()
            );
            assert_eq!(
                normalized.output_tokens + normalized.reasoning_tokens,
                usage["completion_tokens"]
                    .as_u64()
                    .or_else(|| usage["output_tokens"].as_u64())
                    .unwrap_or_default()
            );
        }
        assert_eq!(
            parse_usage(&json!({
                "input_tokens": 17,
                "output_tokens": 6,
                "input_tokens_details": {"cached_tokens": 4},
                "output_tokens_details": {"reasoning_tokens": 2}
            })),
            TokenUsage {
                input_tokens: 13,
                output_tokens: 4,
                cache_read_tokens: 4,
                cache_write_tokens: 0,
                reasoning_tokens: 2,
            }
        );
    }

    #[test]
    fn streamed_errors_are_classified_without_retrying_client_faults() {
        let fixtures = [
            ("invalid_api_key", ProviderErrorKind::Authentication),
            ("invalid_request_error", ProviderErrorKind::InvalidRequest),
            (
                "context_length_exceeded",
                ProviderErrorKind::ContextOverflow,
            ),
            ("rate_limit_exceeded", ProviderErrorKind::RateLimited),
            ("server_error", ProviderErrorKind::Server),
            ("future_error_type", ProviderErrorKind::Protocol),
        ];
        for (code, expected) in fixtures {
            let error = openai_stream_error(&json!({
                "type": "error",
                "error": {"code": code, "message": "sanitized fixture"}
            }));
            assert_eq!(error.kind, expected, "classification for {code}");
        }
        let nested = openai_stream_error(&json!({
            "type": "response.failed",
            "response": {"error": {"type": "invalid_request_error"}}
        }));
        assert_eq!(nested.kind, ProviderErrorKind::InvalidRequest);
    }

    #[test]
    fn copilot_gpt_omits_wire_max_but_other_auth_keeps_it() {
        let request = ProviderRequest {
            model: "gpt-fixture".to_owned(),
            turns: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 512,
            temperature: None,
            thinking: ThinkingLevel::Off,
            cache_hint: None,
        };
        let mut copilot = build_responses_request(&request, false);
        apply_auth_request_shape(
            &mut copilot,
            &AuthMaterial::GitHubCopilot {
                access_token: Secret::new("fixture"),
                user_agent: "rottweiler/fixture".to_owned(),
                initiator: "user".to_owned(),
                vision: false,
                omit_max_output_tokens: true,
            },
        );
        assert!(copilot.get("max_output_tokens").is_none());

        let mut ordinary = build_responses_request(&request, false);
        apply_auth_request_shape(&mut ordinary, &AuthMaterial::Bearer(Secret::new("fixture")));
        assert_eq!(ordinary["max_output_tokens"], json!(512));
    }

    #[test]
    fn chat_reasoning_opaque_round_trips_through_ir_signature() {
        let mut state = OpenAiState::new(OpenAiWireMode::ChatCompletions);
        let events = state
            .handle(&SseEvent {
                event: None,
                data: json!({
                    "model": "fixture",
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "reasoning_content": "summary",
                            "reasoning_opaque": "opaque-fixture"
                        }
                    }]
                })
                .to_string(),
            })
            .unwrap_or_else(|error| panic!("chat reasoning must parse: {error}"));
        let signature = events.into_iter().find_map(|event| match event {
            ProviderEvent::ThinkingDelta {
                signature: Some(signature),
                ..
            } => Some(signature),
            _ => None,
        });
        let signature = signature.unwrap_or_else(|| panic!("signature must be emitted"));
        let messages = super::chat_messages(&Turn {
            role: Role::Assistant,
            blocks: vec![Block::Thinking {
                content: "summary".to_owned(),
                signature: Some(signature),
            }],
            meta: TurnMeta::default(),
        });
        assert_eq!(messages[0]["reasoning_content"], json!("summary"));
        assert_eq!(messages[0]["reasoning_opaque"], json!("opaque-fixture"));
    }

    #[test]
    fn vision_rejection_detects_images_nested_in_tool_results() {
        let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
            name: "fixture".to_owned(),
            endpoint: Url::parse("http://127.0.0.1:9/responses")
                .unwrap_or_else(|error| panic!("fixture URL must parse: {error}")),
            auth: Arc::new(StaticAuth::new(AuthMaterial::None)),
            proxy: None,
            proxy_authentication: None,
            network_policy: NetworkPolicy::Deny,
            wire_mode: OpenAiWireMode::Responses,
            tool_calling: true,
            cache_breakpoints: CacheBreakpointSupport::None,
            supported_reasoning_efforts: Vec::new(),
            supports_vision: false,
            max_context_tokens: None,
            max_output_tokens: None,
        })
        .unwrap_or_else(|error| panic!("fixture provider must build: {error}"));
        let request = ProviderRequest {
            model: "fixture".to_owned(),
            turns: vec![Turn {
                role: Role::Tool,
                blocks: vec![Block::ToolResult {
                    id: ToolCallId("call-1".to_owned()),
                    output: ToolOutput::Mixed {
                        parts: vec![ToolOutputPart::Image {
                            media_type: "image/png".to_owned(),
                            data: ImageRef::InlineBase64 {
                                data: "fixture".to_owned(),
                            },
                        }],
                    },
                    is_error: false,
                }],
                meta: TurnMeta::default(),
            }],
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 32,
            temperature: None,
            thinking: ThinkingLevel::Off,
            cache_hint: None,
        };
        let Err(error) = provider.validate_request_capabilities(&request) else {
            panic!("nested image must be rejected");
        };
        assert_eq!(error.kind, ProviderErrorKind::Unsupported);
    }
}
