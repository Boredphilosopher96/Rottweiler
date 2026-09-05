use async_trait::async_trait;
use miette::IntoDiagnostic;
use miette::Result;
use rw_providers::BoxEventStream;
use rw_providers::CacheBreakpointSupport;
use rw_providers::Capabilities;
use rw_providers::Provider;
use rw_providers::ProviderError;
use rw_providers::ProviderErrorKind;
use rw_providers::ProviderEvent;
use rw_providers::ProviderRequest;
use rw_providers::WireMode;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

pub(super) fn load_provider_script(path: &Path) -> Result<Vec<Vec<ProviderEvent>>> {
    serde_json::from_slice(&std::fs::read(path).into_diagnostic()?).into_diagnostic()
}

pub(super) struct ScriptProvider {
    pub(super) name: String,
    pub(super) scripts: Mutex<VecDeque<Vec<ProviderEvent>>>,
    pub(super) event_delay: std::time::Duration,
    pub(super) model_metadata: Option<rw_core::ProviderModelMetadata>,
    pub(super) cache_support: CacheBreakpointSupport,
}

impl ScriptProvider {
    pub(super) fn new(name: String, scripts: Vec<Vec<ProviderEvent>>, event_delay_ms: u64) -> Self {
        Self {
            name,
            scripts: Mutex::new(scripts.into()),
            event_delay: std::time::Duration::from_millis(event_delay_ms),
            model_metadata: None,
            cache_support: CacheBreakpointSupport::None,
        }
    }

    pub(super) fn with_cache_support(mut self, cache_support: CacheBreakpointSupport) -> Self {
        self.cache_support = cache_support;
        self
    }

    #[cfg(test)]
    pub(super) fn with_model_metadata(mut self, metadata: rw_core::ProviderModelMetadata) -> Self {
        self.model_metadata = Some(metadata);
        self
    }
}

#[async_trait]
impl Provider for ScriptProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calling: true,
            vision: false,
            thinking: true,
            cache_breakpoints: self.cache_support,
            max_context_tokens: Some(128_000),
            max_output_tokens: Some(16_384),
            wire_mode: WireMode::NormalizedReplay,
        }
    }

    async fn model_metadata(
        &self,
    ) -> std::result::Result<Option<rw_core::ProviderModelMetadata>, ProviderError> {
        Ok(self.model_metadata.clone())
    }

    fn cached_model_metadata(&self) -> Option<rw_core::ProviderModelMetadata> {
        self.model_metadata.clone()
    }

    async fn stream(
        &self,
        _request: ProviderRequest,
    ) -> std::result::Result<BoxEventStream, ProviderError> {
        let events = self
            .scripts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::ReplayMiss,
                    "scripted provider sequence is exhausted",
                )
            })?;
        let delay = self.event_delay;
        Ok(Box::pin(async_stream::stream! {
            for event in events {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                yield Ok(event);
            }
        }))
    }
}
