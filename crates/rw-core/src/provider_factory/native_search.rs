//! Native search uses the same admitted, accounted attempt owner as model calls.
use std::{collections::BTreeMap, sync::Arc};

use crate::provider_admission::{ProviderInputBudget, ProviderInvocation, gate::InvocationGate};
use async_trait::async_trait;
use futures_util::StreamExt as _;
use rw_context::LocalTokenEstimator;
use rw_providers::{
    ModelCandidate, NativeWebSearchCapability, NativeWebSearchRequest, Provider, ProviderEvent,
    ProviderModelMetadata, ProviderRequest, ProviderRouter, RetryPolicy, RouterError, ToolChoice,
};
use rw_tools::{
    CancellationToken, ToolError, WebSearchRequest, WebSearchResponse, WebSearchResult,
    WebSearchSource, WebSearcher,
};
use rw_types::config::ThinkingLevel;

/// Inert route configuration. A caller must bind session accounting before search.
#[derive(Clone)]
pub struct ProviderNativeWebSearchFactory {
    pub(super) router: Arc<ProviderRouter>,
    pub(super) alias: String,
    pub(super) candidates: Vec<ModelCandidate>,
    pub(super) metadata: BTreeMap<ModelCandidate, ProviderModelMetadata>,
}
impl ProviderNativeWebSearchFactory {
    /// Creates a single-provider route, including recording and replay adapters.
    /// Unsupported providers have no native search capability.
    ///
    /// # Errors
    /// Returns invalid route configuration.
    pub fn single(provider: Arc<dyn Provider>, model: String) -> Result<Option<Self>, RouterError> {
        if provider.native_web_search_capability() != NativeWebSearchCapability::Supported {
            return Ok(None);
        }
        let candidate = ModelCandidate {
            provider: provider.name().to_owned(),
            model,
        };
        let metadata = provider
            .cached_model_metadata_for(&candidate.model)
            .map(|metadata| BTreeMap::from([(candidate.clone(), metadata)]))
            .unwrap_or_default();
        let alias = "native-search".to_owned();
        let router = ProviderRouter::with_registry(
            BTreeMap::from([(
                alias.clone(),
                vec![format!("{}/{}", candidate.provider, candidate.model)],
            )]),
            [(candidate.provider.clone(), provider)],
            RetryPolicy::default(),
        )?;
        Ok(Some(Self {
            router: Arc::new(router),
            alias,
            candidates: vec![candidate],
            metadata,
        }))
    }

    /// Binds the immutable session, family budget, and durable accounting owner.
    #[must_use]
    pub fn bind(&self, invocation: ProviderInvocation) -> Arc<dyn WebSearcher> {
        Arc::new(ProviderNativeWebSearcher {
            route: self.clone(),
            invocation,
        })
    }
}
struct ProviderNativeWebSearcher {
    route: ProviderNativeWebSearchFactory,
    invocation: ProviderInvocation,
}
#[async_trait]
impl WebSearcher for ProviderNativeWebSearcher {
    async fn settle_effects(&self) -> Result<(), ToolError> {
        self.route
            .router
            .settle_effects()
            .await
            .map_err(|error| ToolError::EffectsUnsettled(error.to_string()))
    }

    async fn search(
        &self,
        request: WebSearchRequest,
        cancellation: CancellationToken,
    ) -> Result<WebSearchResponse, ToolError> {
        let (request, max_results) = provider_request(request)?;
        let mut invocation = self.invocation.clone();
        let mut id = [0_u8; 16];
        getrandom::fill(&mut id).map_err(|error| ToolError::Network(error.to_string()))?;
        invocation.call_id = u128::from_be_bytes(id).to_string();
        invocation.input = ProviderInputBudget::Estimated(
            request
                .turns
                .iter()
                .fold(LocalTokenEstimator::tools(&request.tools), |total, turn| {
                    total.saturating_add(LocalTokenEstimator::turn(turn))
                }),
        );
        let gate = Arc::new(InvocationGate {
            invocation,
            metadata: self.route.metadata.clone(),
        });
        let mut stream = self
            .route
            .router
            .stream_candidates(
                &self.route.alias,
                self.route.candidates.clone(),
                request,
                gate,
            )
            .map_err(|error| ToolError::Network(error.to_string()))?;
        let mut answer = String::new();
        let mut citations = BTreeMap::<String, Option<String>>::new();
        let result = loop {
            let event = tokio::select! {
                event = stream.next() => event,
                () = cancellation.cancelled() => break Err(ToolError::Cancelled),
            };
            match event {
                Some(Ok(ProviderEvent::TextDelta { text })) => {
                    let remaining = 4_096usize.saturating_sub(answer.len());
                    answer.push_str(&text[..text.floor_char_boundary(remaining.min(text.len()))]);
                }
                Some(Ok(ProviderEvent::Citation { uri, title, .. }))
                    if citations.len() < usize::from(max_results) && uri.len() <= 8_192 =>
                {
                    let title = title.map(|mut title| {
                        title.truncate(title.floor_char_boundary(title.len().min(1_024)));
                        title
                    });
                    citations.entry(uri).or_insert(title);
                }
                Some(Ok(ProviderEvent::Finished { .. })) => break Ok(()),
                Some(Ok(_)) => {}
                Some(Err(error)) => break Err(ToolError::Network(error.to_string())),
                None => {
                    break Err(ToolError::Network(
                        "native search ended without an accounted terminal".into(),
                    ));
                }
            }
        };
        // Dropping the receiver transfers its concrete attempt to the router owner.
        drop(stream);
        self.settle_effects().await?;
        result?;
        Ok(WebSearchResponse {
            source: WebSearchSource::ProviderNative,
            results: citations
                .into_iter()
                .map(|(url, title)| WebSearchResult {
                    title: title.unwrap_or_else(|| url.clone()),
                    url,
                    snippet: answer.clone(),
                })
                .collect(),
        })
    }
}

fn provider_request(request: WebSearchRequest) -> Result<(ProviderRequest, u16), ToolError> {
    let max_results = u16::try_from(request.max_results.min(50)).unwrap_or(50);
    let native = NativeWebSearchRequest {
        query: request.query.clone(),
        max_results,
        recency_days: request.recency_days,
        allowed_domains: request.allowed_domains,
    };
    native
        .validate_for(NativeWebSearchCapability::Supported)
        .map_err(|error| ToolError::Network(error.to_string()))?;
    let request = ProviderRequest {
        model: String::new(),
        turns: vec![rw_types::Turn {
            role: rw_types::Role::User,
            blocks: vec![rw_types::Block::Text {
                text: request.query,
            }],
            meta: rw_types::TurnMeta::default(),
        }],
        tools: vec![
            native
                .tool_definition()
                .map_err(|error| ToolError::Network(error.to_string()))?,
        ],
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 2_048,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    };
    Ok((request, max_results))
}
