use super::AgentLoopError;
use crate::ProviderRuntime;
use async_trait::async_trait;
use rw_providers::BoxEventStream;
use rw_providers::CacheBreakpointSupport;
use rw_providers::ProviderRequest;
use rw_providers::TokenUsage;
use rw_types::Cost;
use rw_types::config::BudgetConfig;
use rw_types::config::CompactionConfig;
use rw_types::config::ThinkingLevel;

/// Provider-neutral model streaming boundary used by the actor loop.
#[async_trait]
pub trait ModelDriver: Send + Sync {
    /// Drains local provider work retained after an invocation future is dropped.
    /// Implementations must explicitly prove settlement or report an unsettled outcome.
    async fn settle_effects(&self) -> Result<(), AgentLoopError>;

    /// Starts one provider iteration for an already-resolved model alias.
    ///
    /// # Errors
    ///
    /// Returns an error when alias resolution or stream construction fails.
    fn stream(
        &self,
        alias: &str,
        request: ProviderRequest,
        invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError>;

    /// Streams through one exact configured provider when the user selected a
    /// route explicitly. The default rejects provider-specific routing.
    ///
    /// # Errors
    ///
    /// Returns an error when the alias cannot be streamed through the selected
    /// provider.
    fn stream_for_provider(
        &self,
        alias: &str,
        provider: Option<&str>,
        request: ProviderRequest,
        invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        match provider {
            None => self.stream(alias, request, invocation),
            Some(provider) => Err(AgentLoopError::InvalidConfiguration(format!(
                "model alias {alias:?} cannot be routed through provider {provider:?}"
            ))),
        }
    }

    /// Binds an optional native search capability to the active turn's accounting owner.
    fn native_web_searcher(
        &self,
        _alias: &str,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Option<std::sync::Arc<dyn rw_tools::WebSearcher>> {
        None
    }

    /// Context/cache metadata known without a network call. Unknown context
    /// windows conservatively disable estimate-triggered auto-compaction.
    fn context_metadata(&self, _alias: &str) -> ModelContextMetadata {
        ModelContextMetadata::default()
    }

    /// Whether an alias is configured without making a provider request.
    fn has_model_alias(&self, alias: &str) -> bool {
        !alias.trim().is_empty()
    }

    /// Small, inexpensive alias used for non-blocking session titles. Drivers
    /// return `None` unless they can route this background request safely.
    fn title_model_alias(&self) -> Option<String> {
        None
    }

    /// Resolves a concrete live-catalog model before an idle session commits
    /// the selection. Static/replay drivers keep their synchronous behavior.
    async fn prepare_model(&self, alias: &str) -> Result<(), AgentLoopError> {
        if self.has_model_alias(alias) {
            Ok(())
        } else {
            Err(AgentLoopError::InvalidConfiguration(format!(
                "model {alias:?} is unavailable"
            )))
        }
    }

    /// Commits runtime state staged by [`Self::prepare_model`] after the
    /// corresponding `ModelChanged` event has been persisted successfully.
    fn commit_prepared_model(&self, _alias: &str) {}

    /// Discards runtime state staged by [`Self::prepare_model`] when command
    /// validation or durable persistence fails.
    fn discard_prepared_model(&self, _alias: &str) {}

    /// Activates a provider whose credentials became available after this
    /// session runtime was assembled.
    async fn activate_provider(
        &self,
        provider: &str,
        _selected_model: Option<&str>,
    ) -> Result<(), AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(format!(
            "provider {provider:?} cannot be activated by this model runtime"
        )))
    }

    /// Resolves the session thinking effort for a newly selected model.
    fn thinking_for_model(&self, _model: &str, fallback: ThinkingLevel) -> ThinkingLevel {
        fallback
    }

    /// Whether one exact provider route is configured for an alias.
    fn has_provider_for_alias(&self, _alias: &str, _provider: &str) -> bool {
        false
    }

    /// Whether an alias accepts provider-neutral image blocks.
    fn supports_vision(&self, _alias: &str) -> bool {
        false
    }

    /// Validated compaction settings associated with this model runtime.
    fn compaction_config(&self) -> CompactionConfig {
        CompactionConfig::default()
    }

    /// Validated spend guardrails associated with this model runtime.
    fn budget_config(&self) -> BudgetConfig {
        BudgetConfig::default()
    }

    /// Billing disposition for normalized usage.
    fn cost(&self, _alias: &str, _usage: TokenUsage) -> Cost {
        Cost::Unavailable {
            reason: "provider accounting is unavailable".to_owned(),
        }
    }

    /// Billing disposition when the normalized stream reported the concrete
    /// model that served a failover-capable alias.
    fn cost_for_reported_model(
        &self,
        alias: &str,
        _reported_model: Option<&str>,
        usage: TokenUsage,
    ) -> Cost {
        self.cost(alias, usage)
    }

    /// Billing disposition keyed by an opaque router-owned candidate identity.
    fn cost_for_route(
        &self,
        alias: &str,
        _route: Option<&str>,
        reported_model: Option<&str>,
        usage: TokenUsage,
    ) -> Cost {
        self.cost_for_reported_model(alias, reported_model, usage)
    }

    /// Provider-qualified concrete model that served an iteration, when the
    /// router can resolve its opaque route identity.
    fn qualified_model_for_route(
        &self,
        _alias: &str,
        _route: Option<&str>,
        reported_model: Option<&str>,
    ) -> Option<String> {
        reported_model.map(str::to_owned)
    }
}

/// Synchronous context metadata consumed by the provider-neutral assembler.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModelContextMetadata {
    pub max_context_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub cache_breakpoints: Option<CacheBreakpointSupport>,
}

#[async_trait]
impl ModelDriver for ProviderRuntime {
    fn native_web_searcher(
        &self,
        alias: &str,
        invocation: crate::provider_admission::ProviderInvocation,
    ) -> Option<std::sync::Arc<dyn rw_tools::WebSearcher>> {
        self.native_web_search_factory(alias)
            .map(|factory| factory.bind(invocation))
    }

    async fn settle_effects(&self) -> std::result::Result<(), crate::AgentLoopError> {
        self.settle_provider_effects()
            .await
            .map_err(|error| AgentLoopError::EffectsUnsettled(error.to_string()))
    }

    fn stream(
        &self,
        alias: &str,
        request: ProviderRequest,
        invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        self.stream_alias(alias, request, invocation)
            .map_err(|error| AgentLoopError::Provider(error.to_string()))
    }

    fn stream_for_provider(
        &self,
        alias: &str,
        provider: Option<&str>,
        request: ProviderRequest,
        invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        match provider {
            None => self.stream_alias(alias, request, invocation),
            Some(provider) => self.stream_alias_provider(alias, provider, request, invocation),
        }
        .map_err(|error| AgentLoopError::Provider(error.to_string()))
    }

    fn context_metadata(&self, alias: &str) -> ModelContextMetadata {
        self.resolved_alias_capabilities(alias).map_or_else(
            ModelContextMetadata::default,
            |capabilities| ModelContextMetadata {
                max_context_tokens: capabilities.max_context_tokens,
                max_output_tokens: capabilities.max_output_tokens,
                cache_breakpoints: Some(capabilities.cache_breakpoints),
            },
        )
    }

    fn title_model_alias(&self) -> Option<String> {
        ["title", "fast"]
            .into_iter()
            .find(|alias| self.has_model_alias(alias))
            .map(str::to_owned)
    }

    fn has_model_alias(&self, alias: &str) -> bool {
        self.resolved_alias_capabilities(alias).is_some()
    }

    async fn prepare_model(&self, alias: &str) -> Result<(), AgentLoopError> {
        self.prepare_model_selection(alias)
            .await
            .map_err(|error| AgentLoopError::Provider(error.to_string()))
    }

    async fn activate_provider(
        &self,
        provider: &str,
        selected_model: Option<&str>,
    ) -> Result<(), AgentLoopError> {
        ProviderRuntime::activate_provider(self, provider)
            .map_err(|error| AgentLoopError::Provider(error.to_string()))?;
        if let Some(model) = selected_model.filter(|model| {
            model
                .split_once('/')
                .is_some_and(|(owner, _)| owner == provider)
        }) {
            self.refresh_concrete_model(model)
                .await
                .map_err(|error| AgentLoopError::Provider(error.to_string()))?;
        }
        Ok(())
    }

    fn thinking_for_model(&self, model: &str, fallback: ThinkingLevel) -> ThinkingLevel {
        self.thinking_for_model(model).unwrap_or(fallback)
    }

    fn has_provider_for_alias(&self, alias: &str, provider: &str) -> bool {
        ProviderRuntime::has_provider_for_alias(self, alias, provider)
    }

    fn supports_vision(&self, alias: &str) -> bool {
        self.resolved_alias_capabilities(alias)
            .is_some_and(|capabilities| capabilities.vision)
    }

    fn compaction_config(&self) -> CompactionConfig {
        ProviderRuntime::compaction_config(self).clone()
    }

    fn budget_config(&self) -> BudgetConfig {
        ProviderRuntime::budget_config(self).clone()
    }

    fn cost(&self, alias: &str, usage: TokenUsage) -> Cost {
        self.accounting_for_alias(alias, usage)
    }

    fn cost_for_reported_model(
        &self,
        alias: &str,
        reported_model: Option<&str>,
        usage: TokenUsage,
    ) -> Cost {
        self.accounting_for_reported_model(alias, reported_model, usage)
    }

    fn cost_for_route(
        &self,
        _alias: &str,
        route: Option<&str>,
        _reported_model: Option<&str>,
        usage: TokenUsage,
    ) -> Cost {
        self.accounting_for_route(route, usage)
    }

    fn qualified_model_for_route(
        &self,
        alias: &str,
        route: Option<&str>,
        reported_model: Option<&str>,
    ) -> Option<String> {
        if self.resolved_model(alias).is_some()
            || self
                .resolved_alias_capabilities(alias)
                .is_some_and(|_| alias.contains('/'))
        {
            return Some(alias.to_owned());
        }
        route
            .and_then(|route| self.route_candidate(route).map(str::to_owned))
            .or_else(|| reported_model.map(str::to_owned))
    }
}
