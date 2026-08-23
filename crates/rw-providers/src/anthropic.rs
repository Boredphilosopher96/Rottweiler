use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use rw_types::{Block, ImageRef, Role, ToolOutput, ToolOutputPart, Turn};
use serde_json::{Value, json};
use url::Url;

use crate::types::RawSseFrame;
use crate::{
    AuthProvider, BoxEventStream, CacheBreakpointSupport, Capabilities, DiscoveredModel,
    DiscoveredProviderCatalog, FinishReason, NetworkPolicy, Provider, ProviderError,
    ProviderErrorKind, ProviderEvent, ProviderRequest, ProxyAuthentication, ThinkingLevel,
    TokenUsage, ToolChoice, WireFrameSink, WireMode,
    http::{build_client_with_proxy_auth, require_network, response_error, transport_error},
    sse::{SseDecoder, SseEvent},
};

const MAX_TOOL_ARGUMENT_BYTES: usize = 1_048_576;
const MAX_MODEL_CATALOG_BYTES: usize = 4 * 1024 * 1024;
const MAX_MODEL_CATALOG_PAGES: usize = 32;

/// How a configured Anthropic endpoint represents the thinking dial.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnthropicThinkingStrategy {
    /// Current adaptive thinking plus `output_config.effort`.
    Adaptive,
    /// Legacy explicit budgets supplied by endpoint capability metadata.
    FixedBudgets { low: u32, medium: u32, high: u32 },
}

/// Runtime settings for an Anthropic Messages endpoint.
#[derive(Clone, Debug)]
pub struct AnthropicConfig {
    /// Router provider key.
    pub name: String,
    /// Exact Messages endpoint, normally `.../v1/messages`.
    pub endpoint: Url,
    /// API key, OAuth source, or unauthenticated local gateway.
    pub auth: Arc<dyn AuthProvider>,
    /// Already-resolved proxy for this provider.
    pub proxy: Option<Url>,
    /// Optional HTTP Basic credentials for the resolved proxy.
    pub proxy_authentication: Option<ProxyAuthentication>,
    /// Live/replay network guard.
    pub network_policy: NetworkPolicy,
    /// Thinking mapping declared for the endpoint/model family.
    pub thinking_strategy: Option<AnthropicThinkingStrategy>,
    /// Known context limit.
    pub max_context_tokens: Option<u64>,
    /// Known output limit.
    pub max_output_tokens: Option<u64>,
}

/// Anthropic Messages adapter.
pub struct AnthropicProvider {
    config: AnthropicConfig,
    client: reqwest::Client,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnthropicProvider")
            .field("name", &self.config.name)
            .field("endpoint", &self.config.endpoint)
            .field("network_policy", &self.config.network_policy)
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    /// Builds an adapter and its deterministic proxy-configured HTTP client.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured proxy cannot initialize an HTTP client.
    pub fn new(config: AnthropicConfig) -> Result<Self, ProviderError> {
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
        let body = build_request(&request, self.config.thinking_strategy)?;
        require_network(self.config.network_policy)?;
        let material = self.config.auth.material().await?;
        let mut headers = HeaderMap::new();
        material.apply_anthropic(&mut headers)?;
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
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
                .map(|value| anthropic_stream_error(&value))
                .filter(|error| error.kind == ProviderErrorKind::ContextOverflow);
            return Err(classified.unwrap_or(generic));
        }
        let chunks = response.bytes_stream();
        let stream = async_stream::try_stream! {
            let mut chunks = chunks;
            let mut decoder = SseDecoder::default();
            let mut state = AnthropicState::default();
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
                    "Anthropic stream ended before message_stop",
                ))?;
            }
        };
        Ok(Box::pin(stream))
    }

    async fn discover_models_impl(&self) -> Result<DiscoveredProviderCatalog, ProviderError> {
        require_network(self.config.network_policy)?;
        let material = self.config.auth.material().await?;
        let mut headers = HeaderMap::new();
        material.apply_anthropic(&mut headers)?;
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        let endpoint = anthropic_models_endpoint(&self.config.endpoint)?;
        let mut after_id: Option<String> = None;
        let mut total_bytes = 0_usize;
        let mut models = BTreeMap::new();
        for _ in 0..MAX_MODEL_CATALOG_PAGES {
            let mut page_endpoint = endpoint.clone();
            {
                let mut query = page_endpoint.query_pairs_mut();
                query.append_pair("limit", "100");
                if let Some(cursor) = &after_id {
                    query.append_pair("after_id", cursor);
                }
            }
            let response = self
                .client
                .get(page_endpoint)
                .headers(headers.clone())
                .send()
                .await
                .map_err(transport_error)?;
            if let Some(error) = response_error(&response) {
                return Err(error);
            }
            let remaining = MAX_MODEL_CATALOG_BYTES.saturating_sub(total_bytes);
            let bytes = bounded_anthropic_catalog_bytes(response, remaining).await?;
            total_bytes = total_bytes.saturating_add(bytes.len());
            let page = parse_anthropic_models_page(&bytes)?;
            models.extend(
                page.models
                    .into_iter()
                    .map(|model| (model.id.clone(), model)),
            );
            if !page.has_more {
                return Ok(DiscoveredProviderCatalog {
                    provider: self.config.name.clone(),
                    models: models.into_values().collect(),
                });
            }
            let next = page
                .last_id
                .and_then(|cursor| nonempty(&cursor).map(str::to_owned));
            if next.is_none() || next == after_id {
                return Err(ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "Anthropic model discovery returned a non-advancing pagination cursor",
                ));
            }
            after_id = next;
        }
        Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            "Anthropic model discovery exceeded the page limit",
        ))
    }
}

struct AnthropicModelsPage {
    models: Vec<DiscoveredModel>,
    has_more: bool,
    last_id: Option<String>,
}

fn anthropic_models_endpoint(endpoint: &Url) -> Result<Url, ProviderError> {
    let base = endpoint
        .path()
        .strip_suffix("/messages")
        .unwrap_or_else(|| endpoint.path().trim_end_matches('/'));
    let mut discovered = endpoint.clone();
    discovered.set_path(&format!("{base}/models"));
    discovered.set_query(None);
    discovered.set_fragment(None);
    if discovered.cannot_be_a_base() {
        return Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            "could not construct Anthropic model catalog endpoint",
        ));
    }
    Ok(discovered)
}

async fn bounded_anthropic_catalog_bytes(
    response: reqwest::Response,
    remaining: usize,
) -> Result<Vec<u8>, ProviderError> {
    if remaining == 0
        || response
            .content_length()
            .is_some_and(|length| length > remaining as u64)
    {
        return Err(anthropic_catalog_too_large());
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport_error)?;
        if bytes.len().saturating_add(chunk.len()) > remaining {
            return Err(anthropic_catalog_too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn anthropic_catalog_too_large() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Protocol,
        "Anthropic model discovery response exceeded the size limit",
    )
}

fn parse_anthropic_models_page(bytes: &[u8]) -> Result<AnthropicModelsPage, ProviderError> {
    let envelope: Value = serde_json::from_slice(bytes).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            "Anthropic model discovery returned invalid JSON",
        )
    })?;
    let data = envelope
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                "Anthropic model discovery returned an invalid envelope",
            )
        })?;
    let models = data
        .iter()
        .filter(|model| {
            model
                .get("type")
                .and_then(Value::as_str)
                .is_none_or(|kind| kind == "model")
        })
        .filter_map(|model| {
            let id = model.get("id").and_then(Value::as_str).and_then(nonempty)?;
            Some(DiscoveredModel {
                id: id.to_owned(),
                display_name: model
                    .get("display_name")
                    .and_then(Value::as_str)
                    .and_then(nonempty)
                    .map(str::to_owned),
                description: None,
                capabilities: None,
                pricing: None,
            })
        })
        .collect();
    Ok(AnthropicModelsPage {
        models,
        has_more: envelope
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        last_id: envelope
            .get("last_id")
            .and_then(Value::as_str)
            .and_then(nonempty)
            .map(str::to_owned),
    })
}

fn nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calling: true,
            vision: true,
            thinking: self.config.thinking_strategy.is_some(),
            cache_breakpoints: CacheBreakpointSupport::Explicit,
            max_context_tokens: self.config.max_context_tokens,
            max_output_tokens: self.config.max_output_tokens,
            wire_mode: WireMode::AnthropicMessages,
        }
    }

    async fn discover_models(&self) -> Result<Option<DiscoveredProviderCatalog>, ProviderError> {
        self.discover_models_impl().await.map(Some)
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

#[allow(clippy::too_many_lines)]
fn build_request(
    request: &ProviderRequest,
    thinking_strategy: Option<AnthropicThinkingStrategy>,
) -> Result<Value, ProviderError> {
    request.validate_tool_choice()?;
    if request.thinking != ThinkingLevel::Off
        && matches!(
            &request.tool_choice,
            ToolChoice::Required | ToolChoice::Named { .. }
        )
    {
        return Err(ProviderError::new(
            ProviderErrorKind::Unsupported,
            "Anthropic thinking supports only auto or none tool choice",
        ));
    }
    let mut system = Vec::new();
    let mut last_stable_system = None;
    let mut messages = Vec::new();
    let stable_prefix_turns = request
        .cache_hint
        .map_or(0, |hint| hint.stable_prefix_turns as usize);
    for (turn_index, turn) in request.turns.iter().enumerate() {
        if turn.role == Role::System {
            for block in &turn.blocks {
                if let Block::Text { text } = block {
                    system.push(json!({ "type": "text", "text": text }));
                    if turn_index < stable_prefix_turns {
                        last_stable_system = Some(system.len().saturating_sub(1));
                    }
                }
            }
        } else {
            messages.push(anthropic_message(turn));
        }
    }
    let mut tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();
    let tools_in_prefix = request
        .cache_hint
        .is_some_and(|hint| hint.tools_in_prefix && !tools.is_empty());
    if let Some(index) = last_stable_system {
        system[index]["cache_control"] = json!({ "type": "ephemeral" });
    } else if tools_in_prefix && let Some(tool) = tools.last_mut() {
        tool["cache_control"] = json!({ "type": "ephemeral" });
    }
    if request.cache_hint.is_some() {
        mark_last_cacheable_message_block(&mut messages);
    }
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_output_tokens,
        "stream": true,
        "messages": messages,
    });
    let object = body.as_object_mut().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            "request object construction failed",
        )
    })?;
    if !system.is_empty() {
        object.insert("system".to_owned(), Value::Array(system));
    }
    if !tools.is_empty() {
        object.insert("tools".to_owned(), Value::Array(tools));
    }
    if !request.tools.is_empty() || request.tool_choice == ToolChoice::None {
        let tool_choice = match &request.tool_choice {
            ToolChoice::Auto => json!({ "type": "auto" }),
            ToolChoice::Required => json!({ "type": "any" }),
            ToolChoice::None => json!({ "type": "none" }),
            ToolChoice::Named { name } => json!({ "type": "tool", "name": name }),
        };
        object.insert("tool_choice".to_owned(), tool_choice);
    }
    if let Some(temperature) = request.temperature {
        object.insert("temperature".to_owned(), json!(temperature));
    }
    if request.thinking != ThinkingLevel::Off {
        match thinking_strategy {
            Some(AnthropicThinkingStrategy::Adaptive) => {
                object.insert("thinking".to_owned(), json!({ "type": "adaptive" }));
                object.insert(
                    "output_config".to_owned(),
                    json!({ "effort": thinking_name(request.thinking) }),
                );
            }
            Some(AnthropicThinkingStrategy::FixedBudgets { low, medium, high }) => {
                let budget = match request.thinking {
                    ThinkingLevel::Off => 0,
                    ThinkingLevel::Low => low,
                    ThinkingLevel::Medium => medium,
                    ThinkingLevel::High => high,
                };
                object.insert(
                    "thinking".to_owned(),
                    json!({ "type": "enabled", "budget_tokens": budget }),
                );
            }
            None => {
                return Err(ProviderError::new(
                    ProviderErrorKind::Unsupported,
                    "configured Anthropic endpoint does not support requested thinking effort",
                ));
            }
        }
    }
    Ok(body)
}

fn mark_last_cacheable_message_block(messages: &mut [Value]) {
    for message in messages.iter_mut().rev() {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let Some(block) = content.iter_mut().rev().find(|block| {
            matches!(
                block.get("type").and_then(Value::as_str),
                Some("text" | "image" | "tool_use" | "tool_result")
            )
        }) else {
            continue;
        };
        block["cache_control"] = json!({ "type": "ephemeral" });
        return;
    }
}

const fn thinking_name(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
    }
}

fn anthropic_message(turn: &Turn) -> Value {
    let role = if turn.role == Role::Assistant {
        "assistant"
    } else {
        "user"
    };
    let mut content = Vec::new();
    for block in &turn.blocks {
        match block {
            Block::Text { text } => content.push(json!({ "type": "text", "text": text })),
            Block::Thinking {
                content: text,
                signature,
            } => {
                let mut thinking = json!({ "type": "thinking", "thinking": text });
                if let Some(signature) = signature {
                    thinking["signature"] = json!(signature);
                }
                content.push(thinking);
            }
            Block::ToolCall { id, name, args } => content.push(json!({
                "type": "tool_use", "id": id.0, "name": name, "input": args,
            })),
            Block::ToolResult {
                id,
                output,
                is_error,
            } => content.push(json!({
                "type": "tool_result", "tool_use_id": id.0,
                "content": anthropic_tool_output(output), "is_error": is_error,
            })),
            Block::Image { media_type, data } => content.push(anthropic_image(media_type, data)),
            Block::Citation {
                uri,
                title,
                excerpt,
            } => content.push(json!({
                "type": "text", "text": excerpt.as_deref().unwrap_or_default(),
                "citations": [{
                    "type": "web_search_result_location", "url": uri, "title": title,
                }],
            })),
        }
    }
    json!({ "role": role, "content": content })
}

fn anthropic_tool_output(output: &ToolOutput) -> Value {
    match output {
        ToolOutput::Text { text } => json!(text),
        ToolOutput::Structured { value } => json!(value.to_string()),
        ToolOutput::Mixed { parts } => Value::Array(
            parts
                .iter()
                .map(|part| match part {
                    ToolOutputPart::Text { text } => json!({ "type": "text", "text": text }),
                    ToolOutputPart::Structured { value } => {
                        json!({ "type": "text", "text": value.to_string() })
                    }
                    ToolOutputPart::Image { media_type, data } => anthropic_image(media_type, data),
                })
                .collect(),
        ),
    }
}

fn anthropic_image(media_type: &str, data: &ImageRef) -> Value {
    let source = match data {
        ImageRef::InlineBase64 { data } => {
            json!({ "type": "base64", "media_type": media_type, "data": data })
        }
        ImageRef::Url { url } => json!({ "type": "url", "url": url }),
    };
    json!({ "type": "image", "source": source })
}

#[derive(Default)]
struct AnthropicState {
    tools: BTreeMap<u64, ToolState>,
    usage: TokenUsage,
    finished: bool,
    finish_reason: Option<FinishReason>,
}

struct ToolState {
    id: String,
    arguments: String,
}

impl AnthropicState {
    #[allow(clippy::too_many_lines)]
    fn handle(&mut self, event: &SseEvent) -> Result<Vec<ProviderEvent>, ProviderError> {
        if event.event.as_deref() == Some("ping") {
            return Ok(Vec::new());
        }
        let value: Value = serde_json::from_str(&event.data).map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                format!("invalid Anthropic SSE JSON: {error}"),
            )
        })?;
        let kind = event
            .event
            .as_deref()
            .or_else(|| value.get("type").and_then(Value::as_str));
        match kind {
            Some("message_start") => {
                let message = &value["message"];
                self.usage.input_tokens = u64_at(message, &["usage", "input_tokens"]);
                self.usage.cache_read_tokens =
                    u64_at(message, &["usage", "cache_read_input_tokens"]);
                self.usage.cache_write_tokens =
                    u64_at(message, &["usage", "cache_creation_input_tokens"]);
                Ok(vec![
                    ProviderEvent::MessageStart {
                        model: message["model"].as_str().unwrap_or_default().to_owned(),
                    },
                    ProviderEvent::Usage { usage: self.usage },
                ])
            }
            Some("content_block_start") => {
                let index = value["index"].as_u64().ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::Protocol,
                        "tool call start omitted its index",
                    )
                })?;
                let block = &value["content_block"];
                if block["type"] == "tool_use" {
                    let id = block["id"]
                        .as_str()
                        .filter(|id| !id.trim().is_empty())
                        .ok_or_else(|| {
                            ProviderError::new(
                                ProviderErrorKind::Protocol,
                                "tool call start omitted its id",
                            )
                        })?
                        .to_owned();
                    let name = block["name"]
                        .as_str()
                        .filter(|name| !name.trim().is_empty())
                        .ok_or_else(|| {
                            ProviderError::new(
                                ProviderErrorKind::Protocol,
                                "tool call start omitted its name",
                            )
                        })?
                        .to_owned();
                    if self.tools.contains_key(&index) {
                        return Err(ProviderError::new(
                            ProviderErrorKind::Protocol,
                            "tool call start reused an active index",
                        ));
                    }
                    self.tools.insert(
                        index,
                        ToolState {
                            id: id.clone(),
                            arguments: String::new(),
                        },
                    );
                    Ok(vec![ProviderEvent::ToolCallStart { id, name }])
                } else {
                    Ok(Vec::new())
                }
            }
            Some("content_block_delta") => {
                let delta = &value["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => Ok(vec![ProviderEvent::TextDelta {
                        text: delta["text"].as_str().unwrap_or_default().to_owned(),
                    }]),
                    Some("thinking_delta") => Ok(vec![ProviderEvent::ThinkingDelta {
                        content: delta["thinking"].as_str().unwrap_or_default().to_owned(),
                        signature: None,
                    }]),
                    Some("signature_delta") => Ok(vec![ProviderEvent::ThinkingDelta {
                        content: String::new(),
                        signature: delta["signature"].as_str().map(str::to_owned),
                    }]),
                    Some("citations_delta") => {
                        let citation = &delta["citation"];
                        Ok(vec![ProviderEvent::Citation {
                            uri: citation["url"].as_str().unwrap_or_default().to_owned(),
                            title: citation["title"].as_str().map(str::to_owned),
                            start_index: citation["start_char_index"].as_u64(),
                            end_index: citation["end_char_index"].as_u64(),
                        }])
                    }
                    Some("input_json_delta") => {
                        let index = value["index"].as_u64().unwrap_or_default();
                        let fragment = delta["partial_json"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned();
                        let tool = self.tools.get_mut(&index).ok_or_else(|| {
                            ProviderError::new(
                                ProviderErrorKind::Protocol,
                                "tool argument delta arrived before tool start",
                            )
                        })?;
                        if tool.arguments.len().saturating_add(fragment.len())
                            > MAX_TOOL_ARGUMENT_BYTES
                        {
                            return Err(ProviderError::new(
                                ProviderErrorKind::Protocol,
                                "provider tool arguments exceeded the 1 MiB safety limit",
                            ));
                        }
                        tool.arguments.push_str(&fragment);
                        Ok(vec![ProviderEvent::ToolCallArgumentsDelta {
                            id: tool.id.clone(),
                            json_fragment: fragment,
                        }])
                    }
                    _ => Ok(Vec::new()),
                }
            }
            Some("content_block_stop") => {
                let index = value["index"].as_u64().unwrap_or_default();
                let Some(tool) = self.tools.remove(&index) else {
                    return Ok(Vec::new());
                };
                let arguments = parse_arguments(&tool.arguments)?;
                Ok(vec![ProviderEvent::ToolCallEnd {
                    id: tool.id,
                    arguments,
                }])
            }
            Some("message_delta") => {
                self.usage.output_tokens = u64_at(&value, &["usage", "output_tokens"]);
                self.finish_reason = Some(map_finish(value["delta"]["stop_reason"].as_str()));
                Ok(vec![ProviderEvent::Usage { usage: self.usage }])
            }
            Some("message_stop") => {
                self.finished = true;
                Ok(vec![ProviderEvent::Finished {
                    reason: self.finish_reason.unwrap_or(FinishReason::Stop),
                }])
            }
            Some("error") => Err(anthropic_stream_error(&value)),
            _ => Ok(Vec::new()),
        }
    }
}

fn anthropic_stream_error(value: &Value) -> ProviderError {
    let error_type = value["error"]["type"].as_str().unwrap_or("unknown");
    let prompt_too_long = error_type == "invalid_request_error"
        && value["error"]["message"]
            .as_str()
            .is_some_and(is_prompt_too_long);
    let kind = if prompt_too_long {
        ProviderErrorKind::ContextOverflow
    } else {
        match error_type {
            "authentication_error" | "billing_error" | "permission_error" => {
                ProviderErrorKind::Authentication
            }
            "invalid_request_error"
            | "not_found_error"
            | "conflict_error"
            | "request_too_large" => ProviderErrorKind::InvalidRequest,
            "rate_limit_error" => ProviderErrorKind::RateLimited,
            "api_error" | "timeout_error" | "overloaded_error" => ProviderErrorKind::Server,
            _ => ProviderErrorKind::Protocol,
        }
    };
    let message = match kind {
        ProviderErrorKind::ContextOverflow => "Anthropic context window exceeded",
        ProviderErrorKind::Authentication => "Anthropic authentication error",
        ProviderErrorKind::InvalidRequest => "Anthropic invalid request",
        ProviderErrorKind::RateLimited => "Anthropic rate limit exceeded",
        ProviderErrorKind::Server => "Anthropic server error",
        ProviderErrorKind::Timeout => "Anthropic request timed out",
        ProviderErrorKind::Network => "Anthropic network error",
        ProviderErrorKind::NetworkDisabled => "Anthropic network access disabled",
        ProviderErrorKind::Protocol
        | ProviderErrorKind::Unsupported
        | ProviderErrorKind::ReplayMiss
        | ProviderErrorKind::Cancelled => "Anthropic protocol error",
    };
    ProviderError::new(kind, message)
}

fn is_prompt_too_long(message: &str) -> bool {
    let normalized = message.trim().to_ascii_lowercase();
    if normalized == "prompt is too long" {
        return true;
    }
    let Some(suffix) = normalized.strip_prefix("prompt is too long: ") else {
        return false;
    };
    let Some((actual, maximum)) = suffix.split_once(" tokens > ") else {
        return false;
    };
    let Some(maximum) = maximum.strip_suffix(" maximum") else {
        return false;
    };
    !actual.is_empty()
        && actual.bytes().all(|byte| byte.is_ascii_digit())
        && !maximum.is_empty()
        && maximum.bytes().all(|byte| byte.is_ascii_digit())
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

fn u64_at(value: &Value, path: &[&str]) -> u64 {
    path.iter()
        .fold(value, |current, key| &current[*key])
        .as_u64()
        .unwrap_or_default()
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

fn map_finish(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("end_turn" | "stop_sequence") => FinishReason::Stop,
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCalls,
        Some(_) | None => FinishReason::Unknown,
    }
}

pub(crate) fn replay_sse_frames(
    frames: &[RawSseFrame],
) -> Vec<Result<ProviderEvent, ProviderError>> {
    let mut state = AnthropicState::default();
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
            "Anthropic replay ended before message_stop",
        )));
    }
    items
}

#[cfg(test)]
mod tests {
    use rw_types::{Block, Role, Turn, TurnMeta};
    use serde_json::json;

    use crate::{
        CacheHint, ProviderErrorKind, ProviderRequest, ThinkingLevel, ToolChoice, ToolDefinition,
        sse::SseEvent,
    };

    use super::{AnthropicState, AnthropicThinkingStrategy, anthropic_stream_error, build_request};

    fn tool_request(tool_choice: ToolChoice) -> ProviderRequest {
        ProviderRequest {
            model: "claude-fixture".to_owned(),
            turns: vec![Turn {
                role: Role::User,
                blocks: Vec::new(),
                meta: TurnMeta::default(),
            }],
            tools: vec![ToolDefinition {
                name: "live_smoke_ping".to_owned(),
                description: "fixture".to_owned(),
                input_schema: json!({"type": "object"}),
            }],
            tool_choice,
            max_output_tokens: 32,
            temperature: None,
            thinking: ThinkingLevel::Off,
            cache_hint: None,
        }
    }

    #[test]
    fn tool_json_fragments_are_accumulated_at_block_stop() {
        let mut state = AnthropicState::default();
        let frames = [
            (
                "content_block_start",
                json!({"index":0,"content_block":{"type":"tool_use","id":"call-1","name":"read","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
            ),
            (
                "content_block_delta",
                json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}),
            ),
            ("content_block_stop", json!({"index":0})),
        ];
        let mut events = Vec::new();
        for (kind, value) in frames {
            events.extend(
                state
                    .handle(&SseEvent {
                        event: Some(kind.to_owned()),
                        data: value.to_string(),
                    })
                    .unwrap_or_else(|error| panic!("frame must normalize: {error}")),
            );
        }
        assert_eq!(events.len(), 4);
        assert_eq!(
            events[3],
            crate::ProviderEvent::ToolCallEnd {
                id: "call-1".to_owned(),
                arguments: json!({"path":"a.rs"}),
            }
        );
    }

    #[test]
    fn duplicate_tool_start_index_is_rejected() {
        let mut state = AnthropicState::default();
        let frame = |id| SseEvent {
            event: Some("content_block_start".to_owned()),
            data: json!({"index":0,"content_block":{"type":"tool_use","id":id,"name":"read"}})
                .to_string(),
        };
        assert!(state.handle(&frame("call-1")).is_ok());
        let Err(error) = state.handle(&frame("call-2")) else {
            panic!("duplicate index must be rejected");
        };
        assert_eq!(error.kind, ProviderErrorKind::Protocol);
    }

    #[test]
    fn streamed_errors_follow_documented_retry_classes() {
        let fixtures = [
            ("authentication_error", ProviderErrorKind::Authentication),
            ("permission_error", ProviderErrorKind::Authentication),
            ("invalid_request_error", ProviderErrorKind::InvalidRequest),
            ("request_too_large", ProviderErrorKind::InvalidRequest),
            ("rate_limit_error", ProviderErrorKind::RateLimited),
            ("api_error", ProviderErrorKind::Server),
            ("overloaded_error", ProviderErrorKind::Server),
            ("future_error_type", ProviderErrorKind::Protocol),
        ];
        for (error_type, expected) in fixtures {
            let error = anthropic_stream_error(&json!({
                "type": "error",
                "error": {"type": error_type, "message": "sanitized fixture"}
            }));
            assert_eq!(error.kind, expected, "classification for {error_type}");
        }
    }

    #[test]
    fn documented_prompt_too_long_is_typed_without_echoing_provider_text() {
        let overflow = anthropic_stream_error(&json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": "prompt is too long"}
        }));
        assert_eq!(overflow.kind, ProviderErrorKind::ContextOverflow);
        assert_eq!(overflow.message, "Anthropic context window exceeded");

        let near_miss = anthropic_stream_error(&json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "prompt is too long; SECRET_CANARY"
            }
        }));
        assert_eq!(near_miss.kind, ProviderErrorKind::InvalidRequest);
        assert!(!near_miss.message.contains("SECRET_CANARY"));
    }

    #[test]
    fn explicit_cache_hint_marks_stable_system_and_conversation_prefix() {
        let mut request = tool_request(ToolChoice::Auto);
        request.turns[0].blocks.push(Block::Text {
            text: "current user message".to_owned(),
        });
        request.turns.insert(
            0,
            Turn {
                role: Role::System,
                blocks: vec![Block::Text {
                    text: "stable system".to_owned(),
                }],
                meta: TurnMeta::default(),
            },
        );
        request.cache_hint = Some(CacheHint {
            stable_prefix_turns: 1,
            tools_in_prefix: true,
        });
        let body = build_request(&request, None)
            .unwrap_or_else(|error| panic!("cache request failed: {error}"));
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );

        request.turns.truncate(1);
        request.tools.clear();
        request.cache_hint = Some(CacheHint {
            stable_prefix_turns: 1,
            tools_in_prefix: false,
        });
        let body = build_request(&request, None)
            .unwrap_or_else(|error| panic!("system cache request failed: {error}"));
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
    }

    #[test]
    fn explicit_cache_hint_marks_tool_when_no_stable_system_exists() {
        let mut request = tool_request(ToolChoice::Auto);
        request.cache_hint = Some(CacheHint {
            stable_prefix_turns: 0,
            tools_in_prefix: true,
        });
        let body = build_request(&request, None)
            .unwrap_or_else(|error| panic!("cache request failed: {error}"));
        assert_eq!(
            body["tools"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
    }

    #[test]
    fn tool_choice_uses_anthropic_messages_shape() {
        let fixtures = [
            (ToolChoice::Auto, json!({"type":"auto"})),
            (ToolChoice::Required, json!({"type":"any"})),
            (ToolChoice::None, json!({"type":"none"})),
            (
                ToolChoice::Named {
                    name: "live_smoke_ping".to_owned(),
                },
                json!({"type":"tool","name":"live_smoke_ping"}),
            ),
        ];
        for (choice, expected) in fixtures {
            let body = build_request(&tool_request(choice), None)
                .unwrap_or_else(|error| panic!("tool request must build: {error}"));
            assert_eq!(body["tool_choice"], expected);
        }
    }

    #[test]
    fn thinking_rejects_anthropic_forced_tool_choice() {
        let mut request = tool_request(ToolChoice::Required);
        request.thinking = ThinkingLevel::Low;
        let Err(error) = build_request(&request, Some(AnthropicThinkingStrategy::Adaptive)) else {
            panic!("Anthropic thinking cannot force a tool");
        };
        assert_eq!(error.kind, ProviderErrorKind::Unsupported);
    }
}
