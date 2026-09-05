use super::command_execution::CommandFixtureMode;
use super::credential_resolution::DeferredToolProxy;
use super::credential_resolution::DeferredWebSearchHeaders;
use super::web_fetch::PolicyWebFetcher;
use super::websearch_recording::RecordingConfiguredWebSearcher;
use super::websearch_recording::ReplayingConfiguredWebSearcher;
use async_trait::async_trait;
use miette::Result;
use miette::miette;
use rw_tools::CancellationToken;
use rw_tools::ConfiguredSearchApi;
use rw_tools::FetchRequest;
use rw_tools::FetchResponse;
use rw_tools::ToolError;
use rw_tools::ToolLimits;
use rw_tools::WebFetcher;
use rw_tools::WebSearchRequest;
use rw_tools::WebSearchResponse;
use rw_tools::WebSearcher;
use rw_types::config::WebSearchConfig;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::OnceCell;
use url::Url;

pub(super) struct DeferredPolicyWebFetcher {
    pub(super) global_proxy: DeferredToolProxy,
    pub(super) inner: OnceCell<Arc<dyn WebFetcher>>,
}

impl DeferredPolicyWebFetcher {
    pub(super) fn new(global_proxy: DeferredToolProxy) -> Self {
        Self {
            global_proxy,
            inner: OnceCell::new(),
        }
    }
}

#[async_trait]
impl WebFetcher for DeferredPolicyWebFetcher {
    async fn fetch(
        &self,
        request: FetchRequest,
        cancellation: CancellationToken,
    ) -> std::result::Result<FetchResponse, ToolError> {
        let inner = self
            .inner
            .get_or_try_init(|| async {
                let proxy = self
                    .global_proxy
                    .resolve()
                    .await
                    .map_err(ToolError::Network)?;
                Ok::<Arc<dyn WebFetcher>, ToolError>(Arc::new(PolicyWebFetcher::new(
                    false,
                    Some(proxy),
                )))
            })
            .await?;
        inner.fetch(request, cancellation).await
    }
}

pub(super) struct DeferredConfiguredWebSearcher {
    pub(super) config: WebSearchConfig,
    pub(super) headers: DeferredWebSearchHeaders,
    pub(super) web_fetcher: Arc<dyn WebFetcher>,
    pub(super) limits: ToolLimits,
    pub(super) fixture_mode: CommandFixtureMode,
    pub(super) inner: OnceCell<Arc<dyn WebSearcher>>,
}

impl DeferredConfiguredWebSearcher {
    pub(super) fn new(
        config: WebSearchConfig,
        headers: DeferredWebSearchHeaders,
        web_fetcher: Arc<dyn WebFetcher>,
        limits: ToolLimits,
        fixture_mode: CommandFixtureMode,
    ) -> Result<Self> {
        let endpoint = config
            .endpoint
            .as_deref()
            .ok_or_else(|| miette!("deferred web-search credentials require an endpoint"))?;
        let endpoint = Url::parse(endpoint)
            .map_err(|error| miette!("configured web-search endpoint is invalid: {error}"))?;
        ConfiguredSearchApi::new(
            Arc::clone(&web_fetcher),
            endpoint,
            config.query_parameter.clone(),
            BTreeMap::new(),
            limits.max_web_bytes,
        )
        .map_err(|error| miette!("configured web-search API could not start: {error}"))?;
        Ok(Self {
            config,
            headers,
            web_fetcher,
            limits,
            fixture_mode,
            inner: OnceCell::new(),
        })
    }
}

#[async_trait]
impl WebSearcher for DeferredConfiguredWebSearcher {
    async fn search(
        &self,
        request: WebSearchRequest,
        cancellation: CancellationToken,
    ) -> std::result::Result<WebSearchResponse, ToolError> {
        let inner = self
            .inner
            .get_or_try_init(|| async {
                let headers = self.headers.resolve().await.map_err(ToolError::Network)?;
                let config = self.config.clone();
                let web_fetcher = Arc::clone(&self.web_fetcher);
                let limits = self.limits;
                let fixture_mode = self.fixture_mode.clone();
                tokio::task::spawn_blocking(move || {
                    configured_web_searcher(
                        false,
                        &config,
                        &headers,
                        &web_fetcher,
                        limits,
                        &fixture_mode,
                    )
                    .map_err(|error| ToolError::Network(error.to_string()))?
                    .ok_or_else(|| {
                        ToolError::Network(
                            "configured web-search endpoint is unavailable".to_owned(),
                        )
                    })
                })
                .await
                .map_err(|error| {
                    ToolError::Network(format!("web-search startup worker failed: {error}"))
                })?
            })
            .await?;
        inner.search(request, cancellation).await
    }
}

pub(super) fn configured_web_searcher(
    offline: bool,
    config: &WebSearchConfig,
    headers: &BTreeMap<String, String>,
    web_fetcher: &Arc<dyn WebFetcher>,
    limits: ToolLimits,
    fixture_mode: &CommandFixtureMode,
) -> Result<Option<Arc<dyn WebSearcher>>> {
    if let CommandFixtureMode::Replay { directory } = fixture_mode {
        return ReplayingConfiguredWebSearcher::load(directory)
            .map(|searcher| searcher.map(|value| Arc::new(value) as Arc<dyn WebSearcher>));
    }
    if offline {
        return Ok(None);
    }
    let searcher = config
        .endpoint
        .as_ref()
        .map(|endpoint| {
            let endpoint = Url::parse(endpoint)
                .map_err(|error| miette!("configured web-search endpoint is invalid: {error}"))?;
            ConfiguredSearchApi::new(
                Arc::clone(web_fetcher),
                endpoint,
                config.query_parameter.clone(),
                headers.clone(),
                limits.max_web_bytes,
            )
            .map(|searcher| Arc::new(searcher) as Arc<dyn WebSearcher>)
            .map_err(|error| miette!("configured web-search API could not start: {error}"))
        })
        .transpose()?;
    match (searcher, fixture_mode) {
        (
            Some(searcher),
            CommandFixtureMode::Record {
                directory,
                redactor,
            },
        ) => RecordingConfiguredWebSearcher::new(searcher, directory, redactor.clone())
            .map(|value| Some(Arc::new(value) as Arc<dyn WebSearcher>)),
        (searcher, _) => Ok(searcher),
    }
}
