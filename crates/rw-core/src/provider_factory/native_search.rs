use super::*;

/// Alias/model-bound adapter from provider streams to the public web-search
/// boundary. It uses the ordinary provider request path, preserving recorder
/// and replay semantics.
pub struct ProviderNativeWebSearcher {
    provider: Arc<dyn Provider>,
    model: String,
}

pub(super) struct ProviderNativeWebSearchRouter {
    pub(super) candidates: Vec<Arc<dyn WebSearcher>>,
}

#[async_trait]
impl WebSearcher for ProviderNativeWebSearchRouter {
    async fn search(
        &self,
        request: WebSearchRequest,
        cancellation: CancellationToken,
    ) -> Result<WebSearchResponse, ToolError> {
        let mut last_error = None;
        for candidate in &self.candidates {
            match candidate
                .search(request.clone(), cancellation.clone())
                .await
            {
                Ok(response) => return Ok(response),
                Err(ToolError::Cancelled) => return Err(ToolError::Cancelled),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            ToolError::Network("no native web-search candidate is available".to_owned())
        }))
    }
}

impl ProviderNativeWebSearcher {
    #[must_use]
    pub fn new(provider: Arc<dyn Provider>, model: String) -> Option<Self> {
        (provider.native_web_search_capability() == NativeWebSearchCapability::Supported)
            .then_some(Self { provider, model })
    }
}

#[async_trait]
impl WebSearcher for ProviderNativeWebSearcher {
    async fn search(
        &self,
        request: WebSearchRequest,
        cancellation: CancellationToken,
    ) -> Result<WebSearchResponse, ToolError> {
        let max_results = u16::try_from(request.max_results.min(50)).unwrap_or(50);
        let native = NativeWebSearchRequest {
            query: request.query.clone(),
            max_results,
            recency_days: request.recency_days,
            allowed_domains: request.allowed_domains,
        };
        native
            .validate_for(self.provider.native_web_search_capability())
            .map_err(|error| ToolError::Network(error.to_string()))?;
        let mut stream = self
            .provider
            .stream(ProviderRequest {
                model: self.model.clone(),
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
                tool_choice: ToolChoice::Auto,
                max_output_tokens: 2_048,
                temperature: None,
                thinking: ThinkingLevel::Off,
                cache_hint: None,
            })
            .await
            .map_err(|error| ToolError::Network(error.to_string()))?;
        let mut answer = String::new();
        let mut citations = BTreeMap::<String, Option<String>>::new();
        loop {
            let event = tokio::select! {
                event = stream.next() => event,
                () = cancellation.cancelled() => return Err(ToolError::Cancelled),
            };
            let Some(event) = event else {
                break;
            };
            match event.map_err(|error| ToolError::Network(error.to_string()))? {
                rw_providers::ProviderEvent::TextDelta { text } => {
                    let remaining = 4_096usize.saturating_sub(answer.len());
                    let end = text.floor_char_boundary(remaining.min(text.len()));
                    answer.push_str(&text[..end]);
                }
                rw_providers::ProviderEvent::Citation { uri, title, .. }
                    if citations.len() < usize::from(max_results) =>
                {
                    citations.entry(uri).or_insert(title);
                }
                _ => {}
            }
        }
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
