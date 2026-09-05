use async_trait::async_trait;
use rw_core::AgentLoopError;
use rw_core::Config;
use rw_core::ModelDriver;
use rw_providers::BoxEventStream;
use rw_providers::ProviderRequest;
use rw_tools::CancellationToken;
use rw_tools::ToolError;
use rw_tools::WebSearchRequest;
use rw_tools::WebSearchResponse;
use rw_tools::WebSearcher;
use rw_types::config::ThinkingLevel;
use std::sync::Arc;
use std::sync::RwLock;
use url::Url;

pub(super) fn provider_native_search_available(config: &Config) -> bool {
    config.models.aliases.values().flatten().any(|candidate| {
        let Some((provider, _model)) = candidate.split_once('/') else {
            return false;
        };
        let Some(provider) = config.providers.get(provider) else {
            return false;
        };
        match provider.kind.as_str() {
            "openai" => provider
                .base_url
                .as_deref()
                .is_none_or(openai_native_endpoint),
            "openai_compatible_responses" => provider
                .base_url
                .as_deref()
                .is_some_and(openai_native_endpoint),
            _ => false,
        }
    })
}

pub(super) fn provider_model_for_alias(
    config: &Config,
    alias: &str,
    expected_provider: &str,
) -> Option<String> {
    config
        .models
        .aliases
        .get(alias)?
        .iter()
        .find_map(|candidate| {
            let (provider, model) = candidate.split_once('/')?;
            (provider == expected_provider).then(|| model.to_owned())
        })
}

pub(super) fn openai_native_endpoint(endpoint: &str) -> bool {
    Url::parse(endpoint)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .as_deref()
        == Some("api.openai.com")
}

pub(super) type NativeWebSearchResolver =
    dyn Fn(&str) -> Option<Arc<dyn WebSearcher>> + Send + Sync + 'static;

pub(super) struct RuntimeWebSearcher {
    pub(super) native: RwLock<Option<Arc<NativeWebSearchResolver>>>,
    pub(super) configured: Option<Arc<dyn WebSearcher>>,
}

impl RuntimeWebSearcher {
    pub(super) fn new(configured: Option<Arc<dyn WebSearcher>>) -> Self {
        Self {
            native: RwLock::new(None),
            configured,
        }
    }

    pub(super) fn bind_native_resolver(&self, native: Option<Arc<NativeWebSearchResolver>>) {
        *self
            .native
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = native;
    }

    pub(super) fn native_resolver(&self) -> Option<Arc<NativeWebSearchResolver>> {
        self.native
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(super) fn is_available_for_alias(&self, alias: &str) -> bool {
        self.configured.is_some()
            || self
                .native_resolver()
                .and_then(|resolve| resolve(alias))
                .is_some()
    }
}

pub(super) struct AliasAwareWebSearchModel {
    pub(super) inner: Arc<dyn ModelDriver>,
    pub(super) searcher: Arc<RuntimeWebSearcher>,
}

impl AliasAwareWebSearchModel {
    pub(super) fn wrap(
        inner: Arc<dyn ModelDriver>,
        searcher: Option<&Arc<RuntimeWebSearcher>>,
    ) -> Arc<dyn ModelDriver> {
        match searcher {
            Some(searcher) => Arc::new(Self {
                inner,
                searcher: Arc::clone(searcher),
            }),
            None => inner,
        }
    }
}

#[async_trait]
impl ModelDriver for AliasAwareWebSearchModel {
    async fn settle_effects(&self) -> std::result::Result<(), rw_core::AgentLoopError> {
        self.inner.settle_effects().await
    }

    fn stream(
        &self,
        alias: &str,
        mut request: ProviderRequest,
        invocation: rw_core::provider_admission::ProviderInvocation,
    ) -> std::result::Result<BoxEventStream, AgentLoopError> {
        if !self.searcher.is_available_for_alias(alias) {
            request.tools.retain(|tool| tool.name != "websearch");
            request.cache_hint = request.cache_hint.and_then(|mut hint| {
                hint.tools_in_prefix = !request.tools.is_empty();
                (hint.stable_prefix_turns > 0 || hint.tools_in_prefix).then_some(hint)
            });
        }
        self.inner.stream(alias, request, invocation)
    }

    fn stream_for_provider(
        &self,
        alias: &str,
        provider: Option<&str>,
        mut request: ProviderRequest,
        invocation: rw_core::provider_admission::ProviderInvocation,
    ) -> std::result::Result<BoxEventStream, AgentLoopError> {
        if !self.searcher.is_available_for_alias(alias) {
            request.tools.retain(|tool| tool.name != "websearch");
            request.cache_hint = request.cache_hint.and_then(|mut hint| {
                hint.tools_in_prefix = !request.tools.is_empty();
                (hint.stable_prefix_turns > 0 || hint.tools_in_prefix).then_some(hint)
            });
        }
        self.inner
            .stream_for_provider(alias, provider, request, invocation)
    }

    fn context_metadata(&self, alias: &str) -> rw_core::ModelContextMetadata {
        self.inner.context_metadata(alias)
    }

    fn has_model_alias(&self, alias: &str) -> bool {
        self.inner.has_model_alias(alias)
    }

    fn title_model_alias(&self) -> Option<String> {
        self.inner.title_model_alias()
    }

    async fn prepare_model(&self, alias: &str) -> std::result::Result<(), AgentLoopError> {
        self.inner.prepare_model(alias).await
    }

    fn commit_prepared_model(&self, alias: &str) {
        self.inner.commit_prepared_model(alias);
    }

    fn discard_prepared_model(&self, alias: &str) {
        self.inner.discard_prepared_model(alias);
    }

    async fn activate_provider(
        &self,
        provider: &str,
        selected_model: Option<&str>,
    ) -> std::result::Result<(), AgentLoopError> {
        self.inner.activate_provider(provider, selected_model).await
    }

    fn thinking_for_model(&self, model: &str, fallback: ThinkingLevel) -> ThinkingLevel {
        self.inner.thinking_for_model(model, fallback)
    }

    fn has_provider_for_alias(&self, alias: &str, provider: &str) -> bool {
        self.inner.has_provider_for_alias(alias, provider)
    }

    fn supports_vision(&self, alias: &str) -> bool {
        self.inner.supports_vision(alias)
    }

    fn compaction_config(&self) -> rw_core::CompactionConfig {
        self.inner.compaction_config()
    }

    fn budget_config(&self) -> rw_core::BudgetConfig {
        self.inner.budget_config()
    }

    fn cost(&self, alias: &str, usage: rw_core::ModelTokenUsage) -> rw_core::Cost {
        self.inner.cost(alias, usage)
    }

    fn cost_for_reported_model(
        &self,
        alias: &str,
        reported_model: Option<&str>,
        usage: rw_core::ModelTokenUsage,
    ) -> rw_core::Cost {
        self.inner
            .cost_for_reported_model(alias, reported_model, usage)
    }

    fn cost_for_route(
        &self,
        alias: &str,
        route: Option<&str>,
        reported_model: Option<&str>,
        usage: rw_core::ModelTokenUsage,
    ) -> rw_core::Cost {
        self.inner
            .cost_for_route(alias, route, reported_model, usage)
    }
}

#[async_trait]
impl WebSearcher for RuntimeWebSearcher {
    async fn search(
        &self,
        request: WebSearchRequest,
        cancellation: CancellationToken,
    ) -> std::result::Result<WebSearchResponse, ToolError> {
        let native = self
            .native
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let (Some(resolve), Some(alias)) = (native, request.model_alias.as_deref())
            && let Some(searcher) = resolve(alias)
        {
            match searcher.search(request.clone(), cancellation.clone()).await {
                Ok(response) => return Ok(response),
                Err(ToolError::Cancelled) => return Err(ToolError::Cancelled),
                Err(_) if self.configured.is_some() => {}
                Err(error) => return Err(error),
            }
        }
        if let Some(configured) = &self.configured {
            return configured.search(request, cancellation).await;
        }
        Err(ToolError::Network(
            "selected model did not provide native web search and no configured API is available"
                .to_owned(),
        ))
    }
}
