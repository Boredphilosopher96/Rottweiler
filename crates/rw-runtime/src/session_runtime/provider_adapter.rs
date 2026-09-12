use async_trait::async_trait;
use rw_core::AgentLoopError;
use rw_core::Config;
use rw_core::ModelDriver;
use rw_providers::BoxEventStream;
use rw_providers::Provider;
use rw_providers::ProviderRequest;
use rw_types::config::ThinkingLevel;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(super) fn configured_session_thinking(config: &Config, model: &str) -> ThinkingLevel {
    config
        .models
        .thinking
        .get(model)
        .or_else(|| config.models.thinking.get(&config.models.default))
        .copied()
        .unwrap_or_default()
}

pub(super) struct ProviderModel {
    pub(super) operations: rw_providers::ProviderRouter,
    pub(super) provider: Arc<dyn Provider>,
    native_search: BTreeMap<String, rw_core::ProviderNativeWebSearchFactory>,
    pub(super) model_metadata: Option<rw_core::ProviderModelMetadata>,
    pub(super) compaction: rw_core::CompactionConfig,
    pub(super) budget: rw_core::BudgetConfig,
}

pub(super) struct UnavailableHostedModel {
    pub(super) alias: String,
    pub(super) reason: String,
    pub(super) compaction: rw_core::CompactionConfig,
    pub(super) budget: rw_core::BudgetConfig,
}

#[async_trait::async_trait]
impl ModelDriver for UnavailableHostedModel {
    async fn settle_effects(&self) -> std::result::Result<(), rw_core::AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        _alias: &str,
        _request: ProviderRequest,
        __invocation: rw_core::provider_admission::ProviderInvocation,
    ) -> std::result::Result<BoxEventStream, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(format!(
            "the interactive engine is ready, but its provider is unavailable: {}",
            self.reason
        )))
    }

    fn has_model_alias(&self, alias: &str) -> bool {
        alias == self.alias
    }

    fn compaction_config(&self) -> rw_core::CompactionConfig {
        self.compaction.clone()
    }

    fn budget_config(&self) -> rw_core::BudgetConfig {
        self.budget.clone()
    }
}

impl ProviderModel {
    pub(super) fn new(
        provider: Arc<dyn Provider>,
        compaction: rw_core::CompactionConfig,
        budget: rw_core::BudgetConfig,
    ) -> std::result::Result<Self, AgentLoopError> {
        let model_metadata = provider.cached_model_metadata();
        Ok(Self {
            operations: rw_providers::ProviderRouter::new(
                BTreeMap::new(),
                Vec::<Arc<dyn Provider>>::new(),
                rw_providers::RetryPolicy::default(),
            )
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?,
            provider,
            model_metadata,
            native_search: BTreeMap::new(),
            compaction,
            budget,
        })
    }
    pub(super) fn with_native_search_routes(
        mut self,
        config: &Config,
    ) -> Result<Self, AgentLoopError> {
        for alias in config.models.aliases.keys() {
            let Some(model) =
                super::native_search::provider_model_for_alias(config, alias, self.provider.name())
            else {
                continue;
            };
            if let Some(factory) =
                rw_core::ProviderNativeWebSearchFactory::single(Arc::clone(&self.provider), model)
                    .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?
            {
                self.native_search.insert(alias.clone(), factory);
            }
        }
        Ok(self)
    }
}

#[async_trait]
impl ModelDriver for ProviderModel {
    fn native_web_searcher(
        &self,
        alias: &str,
        invocation: rw_core::provider_admission::ProviderInvocation,
    ) -> Option<Arc<dyn rw_tools::WebSearcher>> {
        self.native_search
            .get(alias)
            .map(|factory| factory.bind(invocation))
    }

    fn stream(
        &self,
        _alias: &str,
        request: ProviderRequest,
        invocation: rw_core::provider_admission::ProviderInvocation,
    ) -> std::result::Result<BoxEventStream, AgentLoopError> {
        let candidate = rw_providers::ModelCandidate {
            provider: self.provider.name().to_owned(),
            model: request.model.clone(),
        };
        let gate = rw_core::provider_admission::concrete_attempt_gate(
            invocation,
            candidate.clone(),
            self.model_metadata.clone(),
        );
        self.operations
            .stream_provider(candidate, Arc::clone(&self.provider), request, gate)
            .map_err(|error| AgentLoopError::Provider(error.to_string()))
    }

    async fn settle_effects(&self) -> std::result::Result<(), rw_core::AgentLoopError> {
        self.operations
            .settle_effects()
            .await
            .map_err(|error| AgentLoopError::EffectsUnsettled(error.to_string()))
    }

    fn context_metadata(&self, _alias: &str) -> rw_core::ModelContextMetadata {
        let capabilities = self.model_metadata.as_ref().map_or_else(
            || self.provider.capabilities(),
            |metadata| metadata.capabilities.clone(),
        );
        rw_core::ModelContextMetadata {
            max_context_tokens: capabilities.max_context_tokens,
            max_output_tokens: capabilities.max_output_tokens,
            cache_breakpoints: Some(capabilities.cache_breakpoints),
        }
    }

    fn compaction_config(&self) -> rw_core::CompactionConfig {
        self.compaction.clone()
    }

    fn budget_config(&self) -> rw_core::BudgetConfig {
        self.budget.clone()
    }

    fn cost(&self, _alias: &str, usage: rw_core::ModelTokenUsage) -> rw_core::Cost {
        self.model_metadata.as_ref().map_or_else(
            || rw_core::Cost::Unavailable {
                reason: "recorded provider accounting is unavailable".to_owned(),
            },
            |metadata| rw_core::cost_from_model_metadata(metadata, usage),
        )
    }
}
