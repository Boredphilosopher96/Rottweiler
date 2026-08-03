use std::{collections::BTreeMap, fmt, sync::Arc};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use rw_types::{Block, ImageRef, Role, ToolOutput, ToolOutputPart};
use tokio::sync::OnceCell;
use url::{Host, Url};

use super::{
    GitHubCopilotCatalog, GitHubCopilotEndpoint, GitHubCopilotModel, parse_github_copilot_models,
};
use crate::types::RawSseFrame;
use crate::{
    AnthropicConfig, AnthropicProvider, AnthropicThinkingStrategy, AuthMaterial, BoxEventStream,
    CacheBreakpointSupport, Capabilities, DiscoveredModel, DiscoveredProviderCatalog,
    NetworkPolicy, OpenAiChatRequestProfile, OpenAiCompatibleConfig, OpenAiCompatibleProvider,
    OpenAiWireMode, Provider, ProviderError, ProviderErrorKind, ProviderEvent,
    ProviderModelMetadata, ProviderRequest, ProxyAuthentication, Secret, StaticAuth,
    UsageAccounting, WireFrameSink, WireMode,
    http::{build_client_with_proxy_auth, require_network, response_error, transport_error},
};

/// Fixed public GitHub Copilot inference origin.
pub const GITHUB_COPILOT_BASE_URL: &str = "https://api.githubcopilot.com";
/// GitHub Copilot API revision sent by Rottweiler.
pub const GITHUB_COPILOT_API_VERSION: &str = "2026-06-01";

const MAX_CATALOG_BYTES: u64 = 4 * 1024 * 1024;
const OPENAI_REASONING_PREFIX: &str = "openai.responses.reasoning.v1:";
const OPENAI_CHAT_REASONING_PREFIX: &str = "openai.chat.reasoning.v1:";
const COPILOT_RESPONSES_REASONING_PREFIX: &str = "github-copilot.responses.reasoning.v1:";
const COPILOT_MESSAGES_REASONING_PREFIX: &str = "github-copilot.messages.reasoning.v1:";
const COPILOT_CHAT_REASONING_PREFIX: &str = "github-copilot.chat.reasoning.v1:";

/// Shared token, HTTP client, origin, and one-shot catalog for one logical
/// GitHub Copilot provider.
pub struct GitHubCopilotRuntime {
    token: Secret,
    client: reqwest::Client,
    base_url: Url,
    proxy: Option<Url>,
    proxy_authentication: Option<ProxyAuthentication>,
    network_policy: NetworkPolicy,
    catalog: OnceCell<Arc<GitHubCopilotCatalog>>,
}

impl fmt::Debug for GitHubCopilotRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubCopilotRuntime")
            .field("base_url", &self.base_url)
            .field("network_policy", &self.network_policy)
            .field("catalog_initialized", &self.catalog.initialized())
            .finish_non_exhaustive()
    }
}

impl GitHubCopilotRuntime {
    /// Builds a shared runtime fixed to `https://api.githubcopilot.com`.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty token or invalid proxy configuration.
    pub fn new(
        token: Secret,
        proxy: Option<&Url>,
        proxy_authentication: Option<&ProxyAuthentication>,
        network_policy: NetworkPolicy,
    ) -> Result<Self, ProviderError> {
        Self::build(
            token,
            Url::parse(GITHUB_COPILOT_BASE_URL).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "built-in GitHub Copilot origin is invalid",
                )
            })?,
            proxy,
            proxy_authentication,
            network_policy,
        )
    }

    /// Builds a deterministic loopback-only runtime for acceptance tests.
    /// Production integrations must use [`Self::new`].
    ///
    /// # Errors
    ///
    /// Rejects every non-loopback origin.
    #[doc(hidden)]
    pub fn with_test_origin(
        token: Secret,
        base_url: Url,
        network_policy: NetworkPolicy,
    ) -> Result<Self, ProviderError> {
        if !is_loopback_origin(&base_url) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "GitHub Copilot test origin must be an HTTP loopback URL",
            ));
        }
        Self::build(token, base_url, None, None, network_policy)
    }

    fn build(
        token: Secret,
        base_url: Url,
        proxy: Option<&Url>,
        proxy_authentication: Option<&ProxyAuthentication>,
        network_policy: NetworkPolicy,
    ) -> Result<Self, ProviderError> {
        if token.expose_secret().trim().is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                "GitHub Copilot credential is empty",
            ));
        }
        let client = build_client_with_proxy_auth(proxy, proxy_authentication)?;
        Ok(Self {
            token,
            client,
            base_url,
            proxy: proxy.cloned(),
            proxy_authentication: proxy_authentication.cloned(),
            network_policy,
            catalog: OnceCell::new(),
        })
    }

    /// Returns the lazily fetched authenticated catalog. A successful snapshot
    /// is shared; transient failures remain retryable.
    ///
    /// # Errors
    ///
    /// Returns a sanitized network, authentication, or catalog error.
    pub async fn catalog(&self) -> Result<Arc<GitHubCopilotCatalog>, ProviderError> {
        self.catalog
            .get_or_try_init(|| async { self.fetch_catalog().await.map(Arc::new) })
            .await
            .cloned()
    }

    async fn fetch_catalog(&self) -> Result<GitHubCopilotCatalog, ProviderError> {
        require_network(self.network_policy)?;
        let endpoint = self.endpoint("models")?;
        let response = self
            .client
            .get(endpoint)
            .headers(discovery_headers(&self.token)?)
            .send()
            .await
            .map_err(transport_error)?;
        if let Some(error) = response_error(&response) {
            return Err(error);
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_CATALOG_BYTES)
        {
            return Err(catalog_too_large());
        }
        let bytes = response.bytes().await.map_err(transport_error)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CATALOG_BYTES {
            return Err(catalog_too_large());
        }
        parse_github_copilot_models(&bytes)
    }

    fn endpoint(&self, path: &str) -> Result<Url, ProviderError> {
        self.base_url.join(path).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                "could not construct fixed GitHub Copilot endpoint",
            )
        })
    }
}

/// Synchronous model binding around a shared lazy Copilot runtime.
#[derive(Clone, Debug)]
pub struct GitHubCopilotProviderConfig {
    /// Router provider key.
    pub name: String,
    /// Exact provider-local model id to validate against `/models`.
    pub model_id: String,
    /// Shared provider runtime/catalog cache.
    pub runtime: Arc<GitHubCopilotRuntime>,
}

/// Model-bound GitHub Copilot provider with lazy authenticated discovery.
pub struct GitHubCopilotProvider {
    config: GitHubCopilotProviderConfig,
    resolved: OnceCell<Arc<ResolvedModel>>,
}

impl fmt::Debug for GitHubCopilotProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubCopilotProvider")
            .field("name", &self.config.name)
            .field("model_id", &self.config.model_id)
            .field("resolved", &self.resolved.initialized())
            .finish_non_exhaustive()
    }
}

impl GitHubCopilotProvider {
    /// Creates a synchronous model binding. Discovery happens once on first
    /// live stream, so wrapping this provider in [`crate::ReplayProvider`]
    /// opens no discovery or inference sockets.
    ///
    /// # Errors
    ///
    /// Returns an error for empty provider/model identifiers.
    pub fn new(config: GitHubCopilotProviderConfig) -> Result<Self, ProviderError> {
        if config.name.trim().is_empty() || config.model_id.trim().is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "GitHub Copilot provider and model ids must not be empty",
            ));
        }
        Ok(Self {
            config,
            resolved: OnceCell::new(),
        })
    }

    async fn resolved(&self) -> Result<Arc<ResolvedModel>, ProviderError> {
        self.resolved
            .get_or_try_init(|| async {
                let catalog = self.config.runtime.catalog().await?;
                let model = catalog.get(&self.config.model_id).cloned().ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::Unsupported,
                        "requested GitHub Copilot model is unavailable for this account",
                    )
                })?;
                build_resolved(&self.config, model).map(Arc::new)
            })
            .await
            .cloned()
    }

    async fn stream_impl(
        &self,
        request: ProviderRequest,
        wire_sink: Option<Arc<dyn WireFrameSink>>,
    ) -> Result<BoxEventStream, ProviderError> {
        if request.model != self.config.model_id {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "GitHub Copilot model binding does not match the routed request",
            ));
        }
        let resolved = self.resolved().await?;
        if u64::from(request.max_output_tokens) > resolved.model.max_output_tokens {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "requested output limit exceeds the discovered GitHub Copilot model limit",
            ));
        }
        if !resolved.model.supports_tools && !request.tools.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::Unsupported,
                "discovered GitHub Copilot model does not support function tools",
            ));
        }
        let initiator = request
            .turns
            .last()
            .is_none_or(|turn| turn.role != Role::User || turn.meta.synthetic);
        let vision = request_has_image(&request);
        if vision && !resolved.model.supports_vision {
            return Err(ProviderError::new(
                ProviderErrorKind::Unsupported,
                "discovered GitHub Copilot model does not support image input",
            ));
        }
        let request = rewrite_request_signatures(request, resolved.model.endpoint)?;
        let delegate = resolved.delegate(initiator, vision);
        let stream = match wire_sink {
            Some(sink) => delegate.stream_with_wire_sink(request, sink).await?,
            None => delegate.stream(request).await?,
        };
        let endpoint = resolved.model.endpoint;
        Ok(Box::pin(stream.map(move |item| {
            item.and_then(|event| rewrite_event_signature(event, endpoint))
        })))
    }
}

#[async_trait]
impl Provider for GitHubCopilotProvider {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Discovery is asynchronous while this trait method is synchronous.
            // Keep this manifest stable across the first recorded stream; the
            // discovered model remains the fail-closed request gate.
            tool_calling: true,
            vision: false,
            thinking: false,
            cache_breakpoints: CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: WireMode::GitHubCopilot,
        }
    }

    async fn model_metadata(&self) -> Result<Option<ProviderModelMetadata>, ProviderError> {
        let resolved = self.resolved().await?;
        Ok(Some(ProviderModelMetadata {
            capabilities: discovered_capabilities(&resolved.model),
            pricing: resolved.model.model_pricing()?,
            accounting: UsageAccounting::AiCredits {
                // GitHub defines one AI Credit as one US cent.
                micros_usd_per_credit: 10_000,
            },
        }))
    }

    fn cached_model_metadata(&self) -> Option<ProviderModelMetadata> {
        let resolved = self.resolved.get()?;
        Some(ProviderModelMetadata {
            capabilities: discovered_capabilities(&resolved.model),
            pricing: resolved.model.model_pricing().ok()?,
            accounting: UsageAccounting::AiCredits {
                micros_usd_per_credit: 10_000,
            },
        })
    }

    async fn discover_models(&self) -> Result<Option<DiscoveredProviderCatalog>, ProviderError> {
        let catalog = self.config.runtime.catalog().await?;
        // `model_picker_enabled` is a presentation hint, not an availability
        // guarantee. Some authenticated catalogs mark every otherwise usable
        // model false. Prefer GitHub's picker subset when it exists, but never
        // turn a successfully validated live catalog into an empty picker.
        let has_picker_models = catalog.iter().any(|(_, model)| model.picker_enabled);
        let mut models = Vec::new();
        for (_, model) in catalog
            .iter()
            .filter(|(_, model)| !has_picker_models || model.picker_enabled)
        {
            models.push(DiscoveredModel {
                id: model.id.clone(),
                display_name: Some(model.name.clone()),
                description: None,
                capabilities: Some(discovered_capabilities(model)),
                pricing: model.model_pricing()?,
            });
        }
        Ok(Some(DiscoveredProviderCatalog {
            provider: self.config.name.clone(),
            models,
        }))
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

fn discovered_capabilities(model: &GitHubCopilotModel) -> Capabilities {
    Capabilities {
        tool_calling: model.supports_tools,
        vision: model.supports_vision,
        thinking: !model.reasoning_efforts.is_empty(),
        cache_breakpoints: CacheBreakpointSupport::None,
        max_context_tokens: Some(model.max_context_tokens),
        max_output_tokens: Some(model.max_output_tokens),
        wire_mode: match model.endpoint {
            GitHubCopilotEndpoint::Messages => WireMode::GitHubCopilotMessages,
            GitHubCopilotEndpoint::Responses => WireMode::GitHubCopilotResponses,
            GitHubCopilotEndpoint::ChatCompletions => WireMode::GitHubCopilotChatCompletions,
        },
    }
}

struct ResolvedModel {
    model: GitHubCopilotModel,
    user: Arc<dyn Provider>,
    user_vision: Arc<dyn Provider>,
    agent: Arc<dyn Provider>,
    agent_vision: Arc<dyn Provider>,
}

impl ResolvedModel {
    fn delegate(&self, agent: bool, vision: bool) -> Arc<dyn Provider> {
        match (agent, vision) {
            (false, false) => Arc::clone(&self.user),
            (false, true) => Arc::clone(&self.user_vision),
            (true, false) => Arc::clone(&self.agent),
            (true, true) => Arc::clone(&self.agent_vision),
        }
    }
}

fn build_resolved(
    config: &GitHubCopilotProviderConfig,
    model: GitHubCopilotModel,
) -> Result<ResolvedModel, ProviderError> {
    let build = |initiator: &'static str, vision| build_delegate(config, &model, initiator, vision);
    Ok(ResolvedModel {
        user: build("user", false)?,
        user_vision: build("user", true)?,
        agent: build("agent", false)?,
        agent_vision: build("agent", true)?,
        model,
    })
}

fn build_delegate(
    config: &GitHubCopilotProviderConfig,
    model: &GitHubCopilotModel,
    initiator: &str,
    vision: bool,
) -> Result<Arc<dyn Provider>, ProviderError> {
    let auth = Arc::new(StaticAuth::new(AuthMaterial::GitHubCopilot {
        access_token: config.runtime.token.clone(),
        user_agent: format!("rottweiler/{}", env!("CARGO_PKG_VERSION")),
        initiator: initiator.to_owned(),
        vision,
        omit_max_output_tokens: model.family.to_ascii_lowercase().starts_with("gpt"),
    }));
    let endpoint = match model.endpoint {
        GitHubCopilotEndpoint::Messages => config.runtime.endpoint("v1/messages")?,
        GitHubCopilotEndpoint::Responses => config.runtime.endpoint("responses")?,
        GitHubCopilotEndpoint::ChatCompletions => config.runtime.endpoint("chat/completions")?,
    };
    let provider: Arc<dyn Provider> = match model.endpoint {
        GitHubCopilotEndpoint::Messages => Arc::new(AnthropicProvider::new(AnthropicConfig {
            name: config.name.clone(),
            endpoint,
            auth,
            proxy: config.runtime.proxy.clone(),
            proxy_authentication: config.runtime.proxy_authentication.clone(),
            network_policy: config.runtime.network_policy,
            thinking_strategy: anthropic_thinking(model),
            max_context_tokens: Some(model.max_context_tokens),
            max_output_tokens: Some(model.max_output_tokens),
        })?),
        GitHubCopilotEndpoint::Responses | GitHubCopilotEndpoint::ChatCompletions => {
            Arc::new(OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
                name: config.name.clone(),
                endpoint,
                auth,
                proxy: config.runtime.proxy.clone(),
                proxy_authentication: config.runtime.proxy_authentication.clone(),
                network_policy: config.runtime.network_policy,
                wire_mode: if model.endpoint == GitHubCopilotEndpoint::Responses {
                    OpenAiWireMode::Responses
                } else {
                    OpenAiWireMode::ChatCompletions
                },
                chat_request_profile: OpenAiChatRequestProfile::OpenAi,
                tool_calling: model.supports_tools,
                cache_breakpoints: CacheBreakpointSupport::None,
                supported_reasoning_efforts: model.reasoning_efforts.clone(),
                supports_vision: model.supports_vision,
                max_context_tokens: Some(model.max_context_tokens),
                max_output_tokens: Some(model.max_output_tokens),
                headers: BTreeMap::new(),
                header_credentials: BTreeMap::new(),
                extra_body: BTreeMap::new(),
                model_ids: BTreeMap::new(),
                path_template: None,
            })?)
        }
    };
    Ok(provider)
}

fn anthropic_thinking(model: &GitHubCopilotModel) -> Option<AnthropicThinkingStrategy> {
    if model.adaptive_thinking {
        return Some(AnthropicThinkingStrategy::Adaptive);
    }
    let max = model.max_thinking_budget?;
    if max < 3 {
        return None;
    }
    let low = model.min_thinking_budget.unwrap_or(1).clamp(1, max - 2);
    Some(AnthropicThinkingStrategy::FixedBudgets {
        low,
        medium: (max / 2).clamp(low, max - 1),
        high: max - 1,
    })
}

fn discovery_headers(token: &Secret) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    let mut authorization = HeaderValue::from_str(&format!("Bearer {}", token.expose_secret()))
        .map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "GitHub Copilot credential contains invalid header bytes",
            )
        })?;
    authorization.set_sensitive(true);
    headers.insert(AUTHORIZATION, authorization);
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(concat!("rottweiler/", env!("CARGO_PKG_VERSION"))),
    );
    headers.insert(
        HeaderName::from_static("x-github-api-version"),
        HeaderValue::from_static(GITHUB_COPILOT_API_VERSION),
    );
    Ok(headers)
}

fn is_loopback_origin(url: &Url) -> bool {
    url.scheme() == "http"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && match url.host() {
            Some(Host::Ipv4(address)) => address.is_loopback(),
            Some(Host::Ipv6(address)) => address.is_loopback(),
            Some(Host::Domain("localhost")) => true,
            Some(Host::Domain(_)) | None => false,
        }
}

fn request_has_image(request: &ProviderRequest) -> bool {
    request.turns.iter().any(|turn| {
        turn.blocks.iter().any(|block| match block {
            Block::Image { .. } => true,
            Block::ToolResult { output, .. } => tool_output_has_image(output),
            Block::Text { .. }
            | Block::Thinking { .. }
            | Block::ToolCall { .. }
            | Block::Citation { .. } => false,
        })
    })
}

fn tool_output_has_image(output: &ToolOutput) -> bool {
    match output {
        ToolOutput::Mixed { parts } => parts.iter().any(|part| {
            matches!(
                part,
                ToolOutputPart::Image {
                    data: ImageRef::InlineBase64 { .. } | ImageRef::Url { .. },
                    ..
                }
            )
        }),
        ToolOutput::Text { .. } | ToolOutput::Structured { .. } => false,
    }
}

fn rewrite_request_signatures(
    mut request: ProviderRequest,
    endpoint: GitHubCopilotEndpoint,
) -> Result<ProviderRequest, ProviderError> {
    for turn in &mut request.turns {
        for block in &mut turn.blocks {
            if let Block::Thinking {
                signature: Some(signature),
                ..
            } = block
            {
                *signature = decode_signature(signature, endpoint)?;
            }
        }
    }
    Ok(request)
}

fn decode_signature(
    signature: &str,
    endpoint: GitHubCopilotEndpoint,
) -> Result<String, ProviderError> {
    match endpoint {
        GitHubCopilotEndpoint::Responses => signature
            .strip_prefix(COPILOT_RESPONSES_REASONING_PREFIX)
            .map(|payload| format!("{OPENAI_REASONING_PREFIX}{payload}"))
            .ok_or_else(wrong_signature_namespace),
        GitHubCopilotEndpoint::Messages => {
            let payload = signature
                .strip_prefix(COPILOT_MESSAGES_REASONING_PREFIX)
                .ok_or_else(wrong_signature_namespace)?;
            let bytes = URL_SAFE_NO_PAD
                .decode(payload)
                .map_err(|_| wrong_signature_namespace())?;
            String::from_utf8(bytes).map_err(|_| wrong_signature_namespace())
        }
        GitHubCopilotEndpoint::ChatCompletions => signature
            .strip_prefix(COPILOT_CHAT_REASONING_PREFIX)
            .map(|payload| format!("{OPENAI_CHAT_REASONING_PREFIX}{payload}"))
            .ok_or_else(wrong_signature_namespace),
    }
}

fn rewrite_event_signature(
    mut event: ProviderEvent,
    endpoint: GitHubCopilotEndpoint,
) -> Result<ProviderEvent, ProviderError> {
    if let ProviderEvent::ThinkingDelta {
        signature: Some(signature),
        ..
    } = &mut event
    {
        *signature = match endpoint {
            GitHubCopilotEndpoint::Responses => {
                let payload = signature
                    .strip_prefix(OPENAI_REASONING_PREFIX)
                    .ok_or_else(wrong_signature_namespace)?;
                format!("{COPILOT_RESPONSES_REASONING_PREFIX}{payload}")
            }
            GitHubCopilotEndpoint::Messages => format!(
                "{COPILOT_MESSAGES_REASONING_PREFIX}{}",
                URL_SAFE_NO_PAD.encode(signature.as_bytes())
            ),
            GitHubCopilotEndpoint::ChatCompletions => {
                let payload = signature
                    .strip_prefix(OPENAI_CHAT_REASONING_PREFIX)
                    .ok_or_else(wrong_signature_namespace)?;
                format!("{COPILOT_CHAT_REASONING_PREFIX}{payload}")
            }
        };
    }
    Ok(event)
}

pub(crate) fn replay_sse_frames(
    endpoint: GitHubCopilotEndpoint,
    frames: &[RawSseFrame],
) -> Vec<Result<ProviderEvent, ProviderError>> {
    let parsed = match endpoint {
        GitHubCopilotEndpoint::Messages => crate::anthropic::replay_sse_frames(frames),
        GitHubCopilotEndpoint::Responses => {
            crate::openai::replay_sse_frames(OpenAiWireMode::Responses, frames)
        }
        GitHubCopilotEndpoint::ChatCompletions => {
            crate::openai::replay_sse_frames(OpenAiWireMode::ChatCompletions, frames)
        }
    };
    parsed
        .into_iter()
        .map(|item| item.and_then(|event| rewrite_event_signature(event, endpoint)))
        .collect()
}

fn wrong_signature_namespace() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::InvalidRequest,
        "reasoning signature does not belong to this GitHub Copilot wire dialect",
    )
}

fn catalog_too_large() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Protocol,
        "GitHub Copilot model discovery response exceeded the size limit",
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use reqwest::header::HeaderMap;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };
    use url::Url;

    use crate::{
        AuthMaterial, NetworkPolicy, Provider, ProviderErrorKind, ProviderEvent, Secret,
        UsageAccounting,
    };

    use super::{
        COPILOT_CHAT_REASONING_PREFIX, GitHubCopilotEndpoint, GitHubCopilotProvider,
        GitHubCopilotProviderConfig, GitHubCopilotRuntime, RawSseFrame, decode_signature,
        replay_sse_frames,
    };

    #[test]
    fn copilot_headers_are_exact_and_never_compete_with_x_api_key() {
        let material = AuthMaterial::GitHubCopilot {
            access_token: Secret::new("PRIVATE-COPILOT-TOKEN"),
            user_agent: "rottweiler/fixture".to_owned(),
            initiator: "agent".to_owned(),
            vision: true,
            omit_max_output_tokens: false,
        };
        let mut openai = HeaderMap::new();
        material
            .apply_openai(&mut openai)
            .unwrap_or_else(|error| panic!("headers must build: {error}"));
        assert_eq!(openai["authorization"], "Bearer PRIVATE-COPILOT-TOKEN");
        assert!(openai["authorization"].is_sensitive());
        assert_eq!(openai["user-agent"], "rottweiler/fixture");
        assert_eq!(openai["x-github-api-version"], "2026-06-01");
        assert_eq!(openai["openai-intent"], "conversation-edits");
        assert_eq!(openai["x-initiator"], "agent");
        assert_eq!(openai["copilot-vision-request"], "true");
        assert!(!openai.contains_key("x-api-key"));

        let mut anthropic = HeaderMap::new();
        material
            .apply_anthropic(&mut anthropic)
            .unwrap_or_else(|error| panic!("headers must build: {error}"));
        assert_eq!(anthropic["authorization"], "Bearer PRIVATE-COPILOT-TOKEN");
        assert_eq!(
            anthropic["anthropic-beta"],
            "interleaved-thinking-2025-05-14"
        );
        assert!(!anthropic.contains_key("x-api-key"));
    }

    #[test]
    fn chat_replay_and_continuation_use_copilot_signature_namespace() {
        let frames = vec![
            RawSseFrame {
                event: None,
                data: json!({
                    "model": "gpt-fixture",
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "reasoning_content": "considering",
                            "reasoning_opaque": "opaque-fixture"
                        }
                    }]
                })
                .to_string(),
            },
            RawSseFrame {
                event: None,
                data: "[DONE]".to_owned(),
            },
        ];
        let events = replay_sse_frames(GitHubCopilotEndpoint::ChatCompletions, &frames);
        let signature = events.iter().find_map(|event| match event {
            Ok(ProviderEvent::ThinkingDelta {
                signature: Some(signature),
                ..
            }) => Some(signature),
            Ok(_) | Err(_) => None,
        });
        let signature = signature.unwrap_or_else(|| panic!("opaque signature must normalize"));
        assert!(signature.starts_with(COPILOT_CHAT_REASONING_PREFIX));
        let decoded = decode_signature(signature, GitHubCopilotEndpoint::ChatCompletions)
            .unwrap_or_else(|error| panic!("signature must continue: {error}"));
        assert!(decoded.starts_with("openai.chat.reasoning.v1:"));
        assert!(!decoded.contains("opaque-fixture"));
    }

    #[test]
    fn messages_and_responses_tool_calls_replay_through_native_normalizers() {
        let responses = vec![
            frame(
                "response.created",
                &json!({"response":{"model":"gpt-fixture"}}),
            ),
            frame(
                "response.output_item.added",
                &json!({"output_index":0,"item":{"type":"function_call","call_id":"call-r","name":"read"}}),
            ),
            frame(
                "response.function_call_arguments.delta",
                &json!({"output_index":0,"delta":"{\"path\":\"a.rs\"}"}),
            ),
            frame(
                "response.function_call_arguments.done",
                &json!({"output_index":0,"arguments":"{\"path\":\"a.rs\"}"}),
            ),
            frame("response.completed", &json!({"response":{"usage":{}}})),
        ];
        assert_tool_replay(GitHubCopilotEndpoint::Responses, &responses, "call-r");

        let messages = vec![
            frame(
                "message_start",
                &json!({"message":{"model":"claude-fixture","usage":{}}}),
            ),
            frame(
                "content_block_start",
                &json!({"index":0,"content_block":{"type":"tool_use","id":"call-m","name":"read","input":{}}}),
            ),
            frame(
                "content_block_delta",
                &json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.rs\"}"}}),
            ),
            frame("content_block_stop", &json!({"index":0})),
            frame(
                "message_delta",
                &json!({"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}),
            ),
            frame("message_stop", &json!({"type":"message_stop"})),
        ];
        assert_tool_replay(GitHubCopilotEndpoint::Messages, &messages, "call-m");
    }

    #[test]
    fn exact_dialect_replays_error_only_stream_without_frame_guessing() {
        let responses = [frame(
            "error",
            &json!({"type":"error","error":{"type":"server_error"}}),
        )];
        let events = replay_sse_frames(GitHubCopilotEndpoint::Responses, &responses);
        assert!(matches!(
            events.as_slice(),
            [Err(error)] if error.kind == ProviderErrorKind::Server
        ));

        let messages = [frame(
            "error",
            &json!({"type":"error","error":{"type":"overloaded_error"}}),
        )];
        let events = replay_sse_frames(GitHubCopilotEndpoint::Messages, &messages);
        assert!(matches!(
            events.as_slice(),
            [Err(error)] if error.kind == ProviderErrorKind::Server
        ));
    }

    #[tokio::test]
    async fn lazy_catalog_retries_transient_failure_then_caches_success() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("listener must bind: {error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("address must resolve: {error}"));
        let catalog = catalog_fixture();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in [("500 Server Error", ""), ("200 OK", catalog.as_str())] {
                let (mut stream, _) = listener
                    .accept()
                    .await
                    .unwrap_or_else(|error| panic!("request must connect: {error}"));
                requests.push(read_headers(&mut stream).await);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .unwrap_or_else(|error| panic!("response must write: {error}"));
            }
            requests
        });
        let base_url = Url::parse(&format!("http://{address}/"))
            .unwrap_or_else(|error| panic!("loopback URL must parse: {error}"));
        let runtime = Arc::new(
            GitHubCopilotRuntime::with_test_origin(
                Secret::new("PRIVATE-COPILOT-TOKEN"),
                base_url,
                NetworkPolicy::Allow,
            )
            .unwrap_or_else(|error| panic!("runtime must build: {error}")),
        );
        let first = runtime.catalog().await;
        assert_eq!(
            first.err().map(|error| error.kind),
            Some(ProviderErrorKind::Server)
        );
        let second = runtime
            .catalog()
            .await
            .unwrap_or_else(|error| panic!("second discovery must work: {error}"));
        let third = runtime
            .catalog()
            .await
            .unwrap_or_else(|error| panic!("cached discovery must work: {error}"));
        assert!(Arc::ptr_eq(&second, &third));
        assert_eq!(
            second.get("gpt-fixture").map(|model| model.endpoint),
            Some(GitHubCopilotEndpoint::Responses)
        );
        let provider = GitHubCopilotProvider::new(GitHubCopilotProviderConfig {
            name: "github-copilot".to_owned(),
            model_id: "gpt-fixture".to_owned(),
            runtime: Arc::clone(&runtime),
        })
        .unwrap_or_else(|error| panic!("provider must build: {error}"));
        let metadata = provider
            .model_metadata()
            .await
            .unwrap_or_else(|error| panic!("metadata must resolve: {error}"))
            .unwrap_or_else(|| panic!("Copilot must expose dynamic metadata"));
        assert_eq!(
            metadata.accounting,
            UsageAccounting::AiCredits {
                micros_usd_per_credit: 10_000
            }
        );
        assert_eq!(metadata.capabilities.max_output_tokens, Some(16_000));
        assert_eq!(
            metadata
                .pricing
                .map(|pricing| pricing.input_per_million_micros_usd),
            Some(2_500_000)
        );
        assert_discovered_catalog(&provider).await;
        let requests = server
            .await
            .unwrap_or_else(|error| panic!("server task must finish: {error}"));
        assert_eq!(requests.len(), 2);
        for request in requests {
            assert!(request.starts_with("get /models http/1.1"));
            assert!(request.contains("authorization: bearer private-copilot-token"));
            assert!(request.contains("user-agent: rottweiler/"));
            assert!(request.contains("x-github-api-version: 2026-06-01"));
            assert!(!request.contains("PRIVATE-COPILOT-TOKEN\r\nPRIVATE"));
        }
    }

    async fn assert_discovered_catalog(provider: &GitHubCopilotProvider) {
        let discovered = provider
            .discover_models()
            .await
            .unwrap_or_else(|error| panic!("catalog projection must work: {error}"))
            .unwrap_or_else(|| panic!("Copilot must expose model discovery"));
        assert_eq!(discovered.provider, "github-copilot");
        assert_eq!(discovered.models.len(), 1);
        assert_eq!(discovered.models[0].id, "gpt-fixture");
        assert_eq!(
            discovered.models[0]
                .capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.max_context_tokens),
            Some(100_000)
        );
    }

    fn catalog_fixture() -> String {
        json!({
            "data": [{
                "model_picker_enabled": false,
                "id": "gpt-fixture",
                "name": "GPT Fixture",
                "version": "gpt-fixture-2026-07-10",
                "supported_endpoints": ["/chat/completions", "/responses"],
                "policy": { "state": "enabled" },
                "billing": {
                    "token_prices": {
                        "batch_size": 1_000_000,
                        "default": {
                            "cache_price": 25.0,
                            "input_price": 250.0,
                            "output_price": 1500.0
                        }
                    }
                },
                "capabilities": {
                    "family": "gpt-fixture",
                    "limits": {
                        "max_context_window_tokens": 100_000,
                        "max_output_tokens": 16_000,
                        "max_prompt_tokens": 84_000
                    },
                    "supports": { "tool_calls": true, "vision": false }
                }
            }]
        })
        .to_string()
    }

    fn frame(event: &str, data: &serde_json::Value) -> RawSseFrame {
        RawSseFrame {
            event: Some(event.to_owned()),
            data: data.to_string(),
        }
    }

    fn assert_tool_replay(
        endpoint: GitHubCopilotEndpoint,
        frames: &[RawSseFrame],
        expected_id: &str,
    ) {
        let events = replay_sse_frames(endpoint, frames);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                Ok(ProviderEvent::ToolCallEnd { id, arguments })
                    if id == expected_id && arguments == &json!({"path":"a.rs"})
            )
        }));
        assert!(matches!(
            events.last(),
            Some(Ok(ProviderEvent::Finished { .. }))
        ));
    }

    async fn read_headers(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream
                .read(&mut buffer)
                .await
                .unwrap_or_else(|error| panic!("request must read: {error}"));
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(bytes)
            .unwrap_or_else(|error| panic!("headers must be UTF-8: {error}"))
            .to_ascii_lowercase()
    }
}
