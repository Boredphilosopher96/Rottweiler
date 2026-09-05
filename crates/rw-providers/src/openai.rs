mod catalog;
use catalog::{
    bounded_catalog_bytes, discovery_endpoint, is_loopback, parse_openai_models,
    parse_subscription_models,
};

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use base64::Engine as _;
use futures_util::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use rw_types::{Block, ImageRef, Role, ToolOutput, ToolOutputPart, Turn, config::ThinkingLevel};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use url::Url;

use crate::types::{
    MAX_PROVIDER_MODEL_CATALOG_BYTES, MAX_PROVIDER_TOOL_ARGUMENT_BYTES, RawSseFrame,
};
use crate::{
    AuthProvider, BoxEventStream, CacheBreakpointSupport, Capabilities, DiscoveredModel,
    DiscoveredProviderCatalog, FinishReason, NativeWebSearchCapability, NetworkPolicy, Provider,
    ProviderError, ProviderErrorKind, ProviderEvent, ProviderRequest, ProxyAuthentication, Secret,
    TokenUsage, ToolChoice, WireFrameSink, WireMode,
    http::{
        bounded_error_json, build_client_with_proxy_auth, require_network, response_error,
        transport_error,
    },
    sse::{SseDecoder, SseEvent},
};

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

/// Request shape used for an OpenAI-compatible Chat Completions connection.
///
/// This is selected by the configured connection, never by a hardcoded model
/// catalog. Strict `OpenAI` connections use the current `OpenAI` fields, while
/// generic compatible servers receive the older, broadly-supported shape.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OpenAiChatRequestProfile {
    /// Current `OpenAI` Chat Completions request fields.
    #[default]
    OpenAi,
    /// Conservative fields accepted by common OpenAI-compatible servers.
    Compatible,
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
    /// Connection-level Chat Completions request shape. Ignored for Responses.
    pub chat_request_profile: OpenAiChatRequestProfile,
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
    /// Static non-secret request headers.
    pub headers: BTreeMap<String, String>,
    /// Secret request headers resolved from credential references.
    pub header_credentials: BTreeMap<String, Secret>,
    /// Extra request-body fields validated not to collide with engine fields.
    pub extra_body: BTreeMap<String, Value>,
    /// Catalog-facing to on-wire model identifier mappings.
    pub model_ids: BTreeMap<String, String>,
    /// Optional absolute request path containing one `{model}` segment.
    pub path_template: Option<String>,
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
        let wire_model = self
            .config
            .model_ids
            .get(&request.model)
            .map_or(request.model.as_str(), String::as_str);
        let endpoint = self.endpoint_for_model(wire_model)?;
        let mut body = match self.config.wire_mode {
            OpenAiWireMode::ChatCompletions => build_chat_request(
                &request,
                reasoning_endpoint,
                self.config.chat_request_profile,
            ),
            OpenAiWireMode::Responses => build_responses_request(&request, reasoning_endpoint),
        };
        let object = body.as_object_mut().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                "OpenAI-compatible request body was not an object",
            )
        })?;
        object.insert("model".to_owned(), Value::String(wire_model.to_owned()));
        object.extend(self.config.extra_body.clone());
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
        self.apply_configured_headers(&mut headers)?;
        material.apply_openai(&mut headers)?;
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let response = self
            .client
            .post(endpoint)
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

    fn endpoint_for_model(&self, model: &str) -> Result<Url, ProviderError> {
        let Some(template) = &self.config.path_template else {
            return Ok(self.config.endpoint.clone());
        };
        let mut endpoint = self.config.endpoint.clone();
        let mut segments = endpoint.path_segments_mut().map_err(|()| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "configured endpoint cannot accept a path template",
            )
        })?;
        segments.clear();
        for segment in template.trim_start_matches('/').split('/') {
            segments.push(if segment == "{model}" { model } else { segment });
        }
        drop(segments);
        Ok(endpoint)
    }

    fn apply_configured_headers(&self, headers: &mut HeaderMap) -> Result<(), ProviderError> {
        for (name, value) in &self.config.headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "configured provider header name is invalid",
                )
            })?;
            crate::auth::insert_header(headers, name, value)?;
        }
        for (name, secret) in &self.config.header_credentials {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "configured credential header name is invalid",
                )
            })?;
            crate::auth::insert_sensitive(headers, name, secret.expose_secret())?;
        }
        Ok(())
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
        self.apply_configured_headers(&mut headers)?;
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
    async fn continuation_provenance(
        &self,
    ) -> Result<Option<crate::ContinuationProvenance>, ProviderError> {
        let identity = serde_json::json!({
            "endpoint": self.config.endpoint.as_str(),
            "dialect": format!("{:?}", self.config.wire_mode),
            "profile": format!("{:?}", self.config.chat_request_profile),
            "headers": self.config.headers,
            "body": self.config.extra_body,
            "models": self.config.model_ids,
            "path": self.config.path_template,
        });
        Ok(Some(crate::ContinuationProvenance::bind(&[
            b"openai",
            identity.to_string().as_bytes(),
        ])))
    }

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

fn build_chat_request(
    request: &ProviderRequest,
    reasoning_endpoint: bool,
    profile: OpenAiChatRequestProfile,
) -> Value {
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
    ]);
    match profile {
        OpenAiChatRequestProfile::OpenAi => {
            object.insert(
                "stream_options".to_owned(),
                json!({ "include_usage": true }),
            );
            object.insert(
                "max_completion_tokens".to_owned(),
                json!(request.max_output_tokens),
            );
        }
        OpenAiChatRequestProfile::Compatible => {
            object.insert("max_tokens".to_owned(), json!(request.max_output_tokens));
        }
    }
    if !tools.is_empty() {
        object.insert("tools".to_owned(), Value::Array(tools));
    }
    if !request.tools.is_empty() || request.tool_choice == (ToolChoice::None {}) {
        let tool_choice = match &request.tool_choice {
            ToolChoice::Auto {} => json!("auto"),
            ToolChoice::Required {} => json!("required"),
            ToolChoice::None {} => json!("none"),
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
    if !request.tools.is_empty() || request.tool_choice == (ToolChoice::None {}) {
        let tool_choice = match &request.tool_choice {
            ToolChoice::Auto {} => json!("auto"),
            ToolChoice::Required {} => json!("required"),
            ToolChoice::None {} => json!("none"),
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
            let text = chat_text_delta(delta);
            if !text.is_empty() {
                events.push(ProviderEvent::TextDelta { text });
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
                let tool_index = tool["index"].as_u64().ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::Protocol,
                        "tool call start omitted its index",
                    )
                })?;
                self.handle_chat_tool_delta(
                    &mut events,
                    choice_index,
                    tool_index,
                    tool["id"].as_str(),
                    &tool["function"],
                )?;
            }
            if delta["function_call"].is_object() {
                self.handle_chat_tool_delta(
                    &mut events,
                    choice_index,
                    u64::MAX,
                    None,
                    &delta["function_call"],
                )?;
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

    fn handle_chat_tool_delta(
        &mut self,
        events: &mut Vec<ProviderEvent>,
        choice_index: u64,
        tool_index: u64,
        provider_id: Option<&str>,
        function: &Value,
    ) -> Result<(), ProviderError> {
        let key = (choice_index, tool_index);
        let start = provider_id.is_some() || function.get("name").is_some();
        let (inserted, state) = match self.tools.entry(key) {
            std::collections::btree_map::Entry::Occupied(entry) => {
                if start {
                    return Err(ProviderError::new(
                        ProviderErrorKind::Protocol,
                        "tool call start reused an active index",
                    ));
                }
                (false, entry.into_mut())
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                let id = provider_id
                    .filter(|id| !id.trim().is_empty())
                    .ok_or_else(|| {
                        ProviderError::new(
                            ProviderErrorKind::Protocol,
                            "tool call start omitted its id",
                        )
                    })?;
                let name = function["name"]
                    .as_str()
                    .filter(|name| !name.trim().is_empty())
                    .ok_or_else(|| {
                        ProviderError::new(
                            ProviderErrorKind::Protocol,
                            "tool call start omitted its name",
                        )
                    })?;
                (
                    true,
                    entry.insert(OpenAiToolState {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        arguments: String::new(),
                    }),
                )
            }
        };
        if inserted {
            events.push(ProviderEvent::ToolCallStart {
                id: state.id.clone(),
                name: state.name.clone(),
            });
        }
        if let Some(fragment) = function["arguments"]
            .as_str()
            .filter(|fragment| !fragment.is_empty())
        {
            append_arguments(&mut state.arguments, fragment)?;
            events.push(ProviderEvent::ToolCallArgumentsDelta {
                id: state.id.clone(),
                json_fragment: fragment.to_owned(),
            });
        }
        Ok(())
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
                if let Some(state) = self.reasoning.get_mut(&index) {
                    state.streamed_summary.push_str(&content);
                }
                if content.is_empty() {
                    Ok(Vec::new())
                } else {
                    Ok(vec![ProviderEvent::ThinkingDelta {
                        content,
                        signature: None,
                    }])
                }
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
                self.reasoning.insert(
                    index,
                    OpenAiReasoningState {
                        signature,
                        streamed_summary: String::new(),
                    },
                );
                Ok(Vec::new())
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
                let index = value["output_index"].as_u64().ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::Protocol,
                        "tool call start omitted its index",
                    )
                })?;
                let id = value["item"]["call_id"]
                    .as_str()
                    .or_else(|| value["item"]["id"].as_str())
                    .filter(|id| !id.trim().is_empty())
                    .ok_or_else(|| {
                        ProviderError::new(
                            ProviderErrorKind::Protocol,
                            "tool call start omitted its id",
                        )
                    })?
                    .to_owned();
                let name = value["item"]["name"]
                    .as_str()
                    .filter(|name| !name.trim().is_empty())
                    .ok_or_else(|| {
                        ProviderError::new(
                            ProviderErrorKind::Protocol,
                            "tool call start omitted its name",
                        )
                    })?
                    .to_owned();
                if self.tools.contains_key(&(0, index)) {
                    return Err(ProviderError::new(
                        ProviderErrorKind::Protocol,
                        "tool call start reused an active index",
                    ));
                }
                self.tools.insert(
                    (0, index),
                    OpenAiToolState {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
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

fn chat_text_delta(delta: &Value) -> String {
    let mut text = String::new();
    append_chat_content(&delta["content"], &mut text);
    append_chat_content(&delta["refusal"], &mut text);
    text
}

fn append_chat_content(value: &Value, output: &mut String) {
    match value {
        Value::String(text) => output.push_str(text),
        Value::Array(parts) => {
            for part in parts {
                append_chat_content(part, output);
            }
        }
        Value::Object(part) => {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                output.push_str(text);
            } else if let Some(text) = part
                .get("text")
                .and_then(|text| text.get("value"))
                .and_then(Value::as_str)
            {
                output.push_str(text);
            } else if let Some(refusal) = part.get("refusal").and_then(Value::as_str) {
                output.push_str(refusal);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
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
    if target.len().saturating_add(fragment.len()) > MAX_PROVIDER_TOOL_ARGUMENT_BYTES {
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
        ProviderErrorKind::EffectsUnsettled => "provider effects remain unsettled",
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
mod tests;
