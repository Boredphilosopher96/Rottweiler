use super::prompt_shapes::PromptShapeJournal;
use super::prompt_shapes::PromptShapeProfile;
use async_trait::async_trait;
use miette::Result;
use miette::miette;
use rw_core::AgentLoopError;
use rw_core::ModelDriver;
use rw_providers::BoxEventStream;
use rw_providers::CacheBreakpointSupport;
use rw_providers::ProviderRequest;
use rw_tools::CapabilityManifest;
use rw_tools::Tool;
use rw_tools::ToolContext;
use rw_tools::ToolDescriptor;
use rw_tools::ToolError;
use rw_tools::ToolRegistry;
use rw_tools::ToolResult;
use rw_types::config::ThinkingLevel;
use std::sync::Arc;

pub(super) struct PromptRecordingModel {
    pub(super) inner: Arc<dyn ModelDriver>,
    pub(super) journal: Arc<PromptShapeJournal>,
}

#[async_trait]
impl ModelDriver for PromptRecordingModel {
    fn native_web_searcher(
        &self,
        alias: &str,
        invocation: rw_core::provider_admission::ProviderInvocation,
    ) -> Option<Arc<dyn rw_tools::WebSearcher>> {
        self.inner.native_web_searcher(alias, invocation)
    }

    async fn settle_effects(&self) -> std::result::Result<(), rw_core::AgentLoopError> {
        self.inner.settle_effects().await
    }

    fn stream(
        &self,
        alias: &str,
        request: ProviderRequest,
        invocation: rw_core::provider_admission::ProviderInvocation,
    ) -> std::result::Result<BoxEventStream, AgentLoopError> {
        let cache_support = self
            .inner
            .context_metadata(alias)
            .cache_breakpoints
            .unwrap_or(CacheBreakpointSupport::None);
        self.journal
            .record_request(alias, &request, cache_support)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        self.inner.stream(alias, request, invocation)
    }

    fn stream_for_provider(
        &self,
        alias: &str,
        provider: Option<&str>,
        request: ProviderRequest,
        invocation: rw_core::provider_admission::ProviderInvocation,
    ) -> std::result::Result<BoxEventStream, AgentLoopError> {
        let cache_support = self
            .inner
            .context_metadata(alias)
            .cache_breakpoints
            .unwrap_or(CacheBreakpointSupport::None);
        self.journal
            .record_request(alias, &request, cache_support)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
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

pub(super) struct HistoricalPromptTool(pub(super) ToolDescriptor);

#[async_trait]
impl Tool for HistoricalPromptTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        self.0.clone()
    }

    async fn execute(
        &self,
        _context: &ToolContext,
        _input: serde_json::Value,
    ) -> std::result::Result<ToolResult, ToolError> {
        Err(ToolError::InvalidInput(
            "historical prompt tools cannot execute".to_owned(),
        ))
    }
}

pub(super) fn historical_tool_registry(profile: &PromptShapeProfile) -> Result<Arc<ToolRegistry>> {
    let mut registry = ToolRegistry::new();
    for tool in &profile.tools {
        registry
            .register(Arc::new(HistoricalPromptTool(ToolDescriptor {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
                capabilities: CapabilityManifest::default(),
            })))
            .map_err(|error| miette!("historical prompt tool could not register: {error}"))?;
    }
    Ok(Arc::new(registry))
}
