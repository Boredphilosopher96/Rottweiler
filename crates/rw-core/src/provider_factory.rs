//! Production composition boundary for provider adapters and model routing.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use futures_util::StreamExt as _;
use rw_plugin_protocol::validate_provider_alias_prefix;
use rw_providers::{
    AnthropicConfig, AnthropicProvider, AnthropicThinkingStrategy, AuthMaterial, AuthProvider,
    BoxEventStream, CacheBreakpointSupport, Capabilities, FixtureRedactor,
    GITHUB_COPILOT_CLIENT_ID, GitHubCopilotProvider, GitHubCopilotProviderConfig,
    GitHubCopilotRuntime, ModelCandidate, ModelPricing, NativeWebSearchCapability,
    NativeWebSearchRequest, NetworkPolicy, OAuthRefreshConfig, OPENAI_SUBSCRIPTION_CLIENT_ID,
    OPENAI_SUBSCRIPTION_RESPONSES_ENDPOINT, OPENAI_SUBSCRIPTION_TOKEN_ENDPOINT,
    OpenAiChatRequestProfile, OpenAiCompatibleConfig, OpenAiCompatibleProvider,
    OpenAiSubscriptionAuth, OpenAiSubscriptionAuthConfig, OpenAiSubscriptionTokenSink,
    OpenAiWireMode, PricingTable, Provider, ProviderError, ProviderErrorKind,
    ProviderModelMetadata, ProviderRequest, ProviderRouter, ProxyAuthentication, ProxyEnvironment,
    ProxySettings, ProxySource, RefreshTokenSink, RefreshingOAuth, RetryPolicy, RouterError,
    Secret as ProviderSecret, StaticAuth, ToolChoice, UsageAccounting, WireFrameSink, WireMode,
};
use rw_store::credentials::{
    CredentialEnvironment, CredentialError, CredentialManager, CredentialReference,
    CredentialStore, NoExternalCredentialStore, Secret as StoredSecret, SystemEnvironment,
};
use rw_tools::{
    CancellationToken, ToolError, WebSearchRequest, WebSearchResponse, WebSearchResult,
    WebSearchSource, WebSearcher,
};
use rw_types::config::ThinkingLevel;
use rw_types::{
    Cost, ModelAlias, ModelAliasDescriptor, ModelCacheBehavior, ModelCapabilities,
    ModelCatalogSnapshot, ModelDescriptor, ProviderAuthKind, ProviderDescriptor,
    ProviderNextAction,
    config::{
        BudgetConfig, CompactionConfig, Config, ProviderAuthScheme, ProviderConfig,
        ProviderModelPricingConfig,
    },
};
use thiserror::Error;
use url::{Host, Url};

use crate::admin::provider_api_key_credential_reference;
use crate::copilot_credentials::{GitHubCopilotCredential, github_copilot_credential_id};
use crate::subscription_credentials::{
    OpenAiSubscriptionCredentialBundle, openai_codex_credential_id,
};
use crate::{ModelCatalogError, ModelCatalogSource};

const ANTHROPIC_MESSAGES_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const OPENAI_RESPONSES_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const OPENAI_CHAT_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";
const GITHUB_COPILOT_ENDPOINT: &str = "https://api.githubcopilot.com";
const MODEL_DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
const MAX_CATALOG_MODELS: usize = 2_048;
const MAX_CATALOG_PROVIDERS: usize = 128;
const MAX_CATALOG_ALIASES: usize = 256;
const MAX_CATALOG_ALIAS_CANDIDATES: usize = 32;
const MAX_CATALOG_WIRE_BYTES: usize = 512 * 1024;
const MAX_CATALOG_ID_BYTES: usize = 512;
const MAX_CATALOG_TEXT_BYTES: usize = 512;

/// Runtime adapter selected by a provider configuration kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterKind {
    Anthropic,
    OpenAiResponses,
    OpenAiChat,
    OpenAiSubscription,
    GitHubCopilot,
    OpenAiCompatibleResponses,
    OpenAiCompatibleChat,
}

/// Canonical identity of a provider exposed by built-in onboarding.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BuiltinProviderId {
    Anthropic,
    OpenAi,
    OpenAiCodex,
    GitHubCopilot,
}

/// Fixed metadata for one provider exposed by built-in onboarding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinProviderProfile {
    id: BuiltinProviderId,
    canonical_id: &'static str,
    adapter_kind: AdapterKind,
    setup_exposed: bool,
    onboarding_auth_kind: ProviderAuthKind,
}

/// Complete built-in onboarding registry in stable display order.
pub const BUILTIN_PROVIDER_PROFILES: [BuiltinProviderProfile; 4] = [
    BuiltinProviderProfile {
        id: BuiltinProviderId::Anthropic,
        canonical_id: "anthropic",
        adapter_kind: AdapterKind::Anthropic,
        setup_exposed: true,
        onboarding_auth_kind: ProviderAuthKind::ApiKey,
    },
    BuiltinProviderProfile {
        id: BuiltinProviderId::OpenAi,
        canonical_id: "openai",
        adapter_kind: AdapterKind::OpenAiResponses,
        setup_exposed: true,
        onboarding_auth_kind: ProviderAuthKind::ApiKey,
    },
    BuiltinProviderProfile {
        id: BuiltinProviderId::OpenAiCodex,
        canonical_id: "openai_codex",
        adapter_kind: AdapterKind::OpenAiSubscription,
        setup_exposed: true,
        onboarding_auth_kind: ProviderAuthKind::Oauth,
    },
    BuiltinProviderProfile {
        id: BuiltinProviderId::GitHubCopilot,
        canonical_id: "github_copilot",
        adapter_kind: AdapterKind::GitHubCopilot,
        setup_exposed: true,
        onboarding_auth_kind: ProviderAuthKind::DeviceFlow,
    },
];

impl BuiltinProviderId {
    /// Parses a canonical built-in provider id. Adapter aliases do not count as
    /// built-in onboarding identities.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        BUILTIN_PROVIDER_PROFILES
            .iter()
            .find(|profile| profile.canonical_id == value)
            .map(|profile| profile.id)
    }

    /// Returns the fixed profile for this identity.
    #[must_use]
    pub const fn profile(self) -> BuiltinProviderProfile {
        BUILTIN_PROVIDER_PROFILES[self as usize]
    }

    /// Resolves only a canonical provider configured with its fixed built-in
    /// adapter kind.
    #[must_use]
    pub fn from_config(provider: &str, kind: &str) -> Option<Self> {
        Self::parse(provider).filter(|id| id.profile().config_kind() == kind)
    }
}

impl BuiltinProviderProfile {
    #[must_use]
    pub const fn id(self) -> BuiltinProviderId {
        self.id
    }

    #[must_use]
    pub const fn canonical_id(self) -> &'static str {
        self.canonical_id
    }

    /// Built-in profiles currently use their canonical id as the fixed config
    /// kind, so the string has one owner.
    #[must_use]
    pub const fn config_kind(self) -> &'static str {
        self.canonical_id
    }

    #[must_use]
    pub const fn adapter_kind(self) -> AdapterKind {
        self.adapter_kind
    }

    #[must_use]
    pub const fn setup_exposed(self) -> bool {
        self.setup_exposed
    }

    #[must_use]
    pub const fn onboarding_auth_kind(self) -> ProviderAuthKind {
        self.onboarding_auth_kind
    }
}

/// Production live-catalog source using the same secure provider composition
/// boundary as inference.
pub struct ProviderModelCatalogSource {
    credentials_path: PathBuf,
    pricing: ModelCatalogPricing,
    config: Config,
}

enum ModelCatalogPricing {
    Loaded(PricingTable),
    Path(PathBuf),
}

impl ProviderModelCatalogSource {
    #[must_use]
    pub fn system(
        credentials_path: impl Into<PathBuf>,
        pricing: PricingTable,
        config: Config,
    ) -> Self {
        Self {
            credentials_path: credentials_path.into(),
            pricing: ModelCatalogPricing::Loaded(pricing),
            config,
        }
    }

    /// Builds a catalog source that defers pricing-table I/O and parsing until
    /// the first live model-catalog discovery.
    #[must_use]
    pub fn system_from_pricing_path(
        credentials_path: impl Into<PathBuf>,
        pricing_path: impl Into<PathBuf>,
        config: Config,
    ) -> Self {
        Self {
            credentials_path: credentials_path.into(),
            pricing: ModelCatalogPricing::Path(pricing_path.into()),
            config,
        }
    }

    async fn factory(&self) -> Result<ProviderFactory, ModelCatalogError> {
        let pricing = match &self.pricing {
            ModelCatalogPricing::Loaded(pricing) => pricing.clone(),
            ModelCatalogPricing::Path(path) if path.is_file() => PricingTable::load(path)
                .await
                .map_err(|error| ModelCatalogError(error.to_string()))?,
            ModelCatalogPricing::Path(_) => PricingTable::default(),
        };
        Ok(ProviderFactory::system(
            self.credentials_path.clone(),
            pricing,
        ))
    }

    /// Builds a provider inventory without resolving credentials or claiming
    /// any concrete model is available. This is the safe first-run seed for a
    /// non-refresh catalog request when no durable cache exists yet.
    #[must_use]
    pub fn placeholder(config: &Config) -> ModelCatalogSnapshot {
        let discoveries = config
            .providers
            .keys()
            .map(|provider| {
                (
                    provider.clone(),
                    discovery_candidate(provider),
                    false,
                    Err("live model catalog has not been loaded".to_owned()),
                )
            })
            .collect();
        project_model_catalog(config, &PricingTable::default(), discoveries)
    }
}

#[async_trait]
impl ModelCatalogSource for ProviderModelCatalogSource {
    async fn discover(&self) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        self.factory()
            .await?
            .discover_model_catalog(&self.config)
            .await
            .map_err(|error| ModelCatalogError(error.to_string()))
    }

    async fn discover_provider(
        &self,
        provider: &str,
    ) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        self.factory()
            .await?
            .discover_provider_model_catalog(&self.config, provider)
            .await
            .map_err(|error| ModelCatalogError(error.to_string()))
    }
}

/// Sanitized provider-composition failure. Secret values are never retained.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("provider composition failed for {provider:?}: {reason}")]
pub struct ProviderFactoryError {
    provider: String,
    reason: String,
}

impl ProviderFactoryError {
    fn new(provider: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            reason: reason.into(),
        }
    }
}

/// Model-specific information resolved at the composition boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedModel {
    candidate: String,
    provider: String,
    model: String,
    catalog_model: Option<String>,
    capabilities: Capabilities,
    pricing: Option<ModelPricing>,
    pricing_source: Option<ModelPricingSource>,
    accounting: UsageAccounting,
}

/// Authority that supplied a model's effective token rates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelPricingSource {
    /// Explicit user-scoped provider configuration.
    UserConfig,
    /// Authenticated metadata returned by the provider.
    ProviderDiscovered,
    /// Local models.dev enrichment snapshot.
    ModelsDev,
}

impl ModelPricingSource {
    /// Stable diagnostic label used by configuration and runtime inspection.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserConfig => "user_config",
            Self::ProviderDiscovered => "provider_discovered",
            Self::ModelsDev => "models_dev",
        }
    }
}

impl ResolvedModel {
    /// User-configured `provider/model` candidate.
    #[must_use]
    pub fn candidate(&self) -> &str {
        &self.candidate
    }

    /// User-scoped provider registry name.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Model identifier sent on the wire.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Canonical model-catalog key, when authoritative metadata was found.
    #[must_use]
    pub fn catalog_model(&self) -> Option<&str> {
        self.catalog_model.as_deref()
    }

    /// Conservative model-specific capabilities enforced before dispatch.
    #[must_use]
    pub const fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Effective pricing record after applying configured source precedence.
    #[must_use]
    pub const fn pricing(&self) -> Option<&ModelPricing> {
        self.pricing.as_ref()
    }

    /// Source of the effective pricing record, when pricing is available.
    #[must_use]
    pub const fn pricing_source(&self) -> Option<ModelPricingSource> {
        self.pricing_source
    }

    /// Accounting unit for usage reported by this provider route.
    #[must_use]
    pub const fn accounting(&self) -> UsageAccounting {
        self.accounting
    }
}

/// Fully composed provider registry and provider-blind model router.
pub struct ProviderRuntime {
    router: ProviderRouter,
    providers: BTreeMap<String, Arc<dyn Provider>>,
    models: BTreeMap<String, ResolvedModel>,
    dynamic_providers: std::sync::RwLock<BTreeMap<String, Arc<dyn Provider>>>,
    dynamic_models: std::sync::RwLock<BTreeMap<String, ResolvedModel>>,
    connections: std::sync::RwLock<BTreeMap<String, ProviderConnection>>,
    discovery_providers: std::sync::RwLock<BTreeMap<String, Arc<dyn Provider>>>,
    extension_providers: BTreeMap<String, Arc<dyn Provider>>,
    provider_activator: Arc<dyn ProviderActivator>,
    network_policy: NetworkPolicy,
    model_discovery_timeout: std::time::Duration,
    alias_thinking: BTreeMap<String, ThinkingLevel>,
    alias_candidates: BTreeMap<String, Vec<String>>,
    validated_alias_routes: std::sync::RwLock<BTreeMap<String, Vec<ModelCandidate>>>,
    validated_concrete_models: std::sync::RwLock<std::collections::BTreeSet<String>>,
    route_candidates: BTreeMap<String, String>,
    default_alias: String,
    redactor: FixtureRedactor,
    warnings: RuntimeWarnings,
    pricing_table: PricingTable,
    config: Config,
    compaction: CompactionConfig,
    budget: BudgetConfig,
}

enum CatalogAuthority {
    Unavailable(String),
    NotExposed,
    Models(BTreeSet<String>),
}

impl fmt::Debug for ProviderRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRuntime")
            .field("models", &self.models.keys().collect::<Vec<_>>())
            .field("default_alias", &self.default_alias)
            .field("warning_count", &self.warnings.snapshot().len())
            .finish_non_exhaustive()
    }
}

impl ProviderRuntime {
    /// Default provider-blind model alias.
    #[must_use]
    pub fn default_alias(&self) -> &str {
        &self.default_alias
    }

    /// Configured thinking effort for a provider-neutral alias. Concrete
    /// selections intentionally inherit the actor's durable session effort.
    #[must_use]
    pub fn thinking_for_model(&self, model: &str) -> Option<ThinkingLevel> {
        self.alias_thinking.get(model).copied()
    }

    /// Model-bound provider suitable for direct recording or a live smoke test.
    #[must_use]
    pub fn provider(&self, candidate: &str) -> Option<Arc<dyn Provider>> {
        self.providers.get(candidate).cloned()
    }

    /// Model-specific capability and pricing metadata.
    #[must_use]
    pub fn resolved_model(&self, candidate: &str) -> Option<&ResolvedModel> {
        self.models.get(candidate)
    }

    /// Resolves an opaque router identity to its provider-qualified candidate.
    #[must_use]
    pub fn route_candidate(&self, route: &str) -> Option<&str> {
        self.route_candidates.get(route).map(String::as_str)
    }

    /// Resolves current provider-neutral capabilities, rates, and billing unit.
    /// Dynamic providers may perform an authenticated catalog lookup.
    ///
    /// # Errors
    ///
    /// Returns a sanitized discovery error or an unknown-candidate error.
    pub async fn model_metadata(
        &self,
        candidate: &str,
    ) -> Result<ProviderModelMetadata, ProviderFactoryError> {
        let provider = self.providers.get(candidate).ok_or_else(|| {
            ProviderFactoryError::new(candidate, "model candidate is not configured")
        })?;
        let discovered = provider
            .model_metadata()
            .await
            .map_err(|error| ProviderFactoryError::new(candidate, error.to_string()))?;
        let model = self.models.get(candidate).ok_or_else(|| {
            ProviderFactoryError::new(candidate, "model metadata is inconsistent")
        })?;
        Ok(effective_model_metadata(model, discovered))
    }

    /// Known-secret redactor for [`rw_providers::Recorder`].
    #[must_use]
    pub fn fixture_redactor(&self) -> FixtureRedactor {
        self.redactor.clone()
    }

    /// Credential persistence warnings that the active UI must surface.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        self.warnings.snapshot()
    }

    /// Synchronous metadata for the first configured candidate of an alias.
    #[must_use]
    pub fn resolved_alias_model(&self, alias: &str) -> Option<&ResolvedModel> {
        self.models.get(self.alias_candidates.get(alias)?.first()?)
    }

    /// Capabilities for the first alias candidate, upgraded with any metadata
    /// cached by a lazily discovered provider.
    #[must_use]
    pub fn resolved_alias_capabilities(&self, alias: &str) -> Option<Capabilities> {
        let candidate = self
            .alias_candidates
            .get(alias)
            .and_then(|candidates| candidates.first())
            .map_or(alias, String::as_str);
        self.providers
            .get(candidate)
            .and_then(|provider| provider.cached_model_metadata())
            .map(|metadata| metadata.capabilities)
            .or_else(|| {
                self.models
                    .get(candidate)
                    .map(|model| model.capabilities.clone())
            })
            .or_else(|| {
                let providers = self
                    .dynamic_providers
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                providers
                    .get(candidate)
                    .and_then(|provider| provider.cached_model_metadata())
                    .map(|metadata| metadata.capabilities)
            })
            .or_else(|| {
                self.dynamic_models
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(candidate)
                    .map(|model| model.capabilities.clone())
            })
    }

    /// Provider-native search bound to the first configured candidate for an
    /// alias. Unsupported/fallback aliases return `None`.
    #[must_use]
    pub fn native_web_searcher(&self, alias: &str) -> Option<Arc<dyn WebSearcher>> {
        let candidates = self
            .alias_candidates
            .get(alias)?
            .iter()
            .filter_map(|candidate| {
                let model = self.models.get(candidate)?;
                let provider = self.providers.get(candidate)?.clone();
                ProviderNativeWebSearcher::new(provider, model.model.clone())
                    .map(|searcher| Arc::new(searcher) as Arc<dyn WebSearcher>)
            })
            .collect::<Vec<_>>();
        (!candidates.is_empty())
            .then(|| Arc::new(ProviderNativeWebSearchRouter { candidates }) as Arc<dyn WebSearcher>)
    }

    /// Runtime compaction settings captured from validated user config.
    #[must_use]
    pub const fn compaction_config(&self) -> &CompactionConfig {
        &self.compaction
    }

    /// Runtime budget settings captured from validated user config.
    #[must_use]
    pub const fn budget_config(&self) -> &BudgetConfig {
        &self.budget
    }

    /// Typed accounting disposition for the first alias candidate.
    #[must_use]
    pub fn accounting_for_alias(&self, alias: &str, usage: rw_providers::TokenUsage) -> Cost {
        if self.models.contains_key(alias)
            || self
                .dynamic_models
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(alias)
        {
            return self.accounting_for_candidate(alias, usage);
        }
        let Some(candidates) = self.alias_candidates.get(alias) else {
            return Cost::Unavailable {
                reason: "model alias accounting is unavailable".to_owned(),
            };
        };
        let [candidate] = candidates.as_slice() else {
            return Cost::Unavailable {
                reason: "actual failover model is not known for accounting".to_owned(),
            };
        };
        self.accounting_for_candidate(candidate, usage)
    }

    /// Prices usage only when the normalized stream model uniquely identifies
    /// the candidate that actually served a failover-capable alias.
    #[must_use]
    pub fn accounting_for_reported_model(
        &self,
        alias: &str,
        reported_model: Option<&str>,
        usage: rw_providers::TokenUsage,
    ) -> Cost {
        let Some(candidates) = self.alias_candidates.get(alias) else {
            return Cost::Unavailable {
                reason: "model alias accounting is unavailable".to_owned(),
            };
        };
        let mut matches = candidates.iter().filter(|candidate| {
            reported_model.is_some_and(|reported| {
                self.models
                    .get(*candidate)
                    .is_some_and(|model| model.model == reported)
            })
        });
        let Some(candidate) = matches.next() else {
            return Cost::Unavailable {
                reason: "actual routed model is unavailable for accounting".to_owned(),
            };
        };
        if matches.next().is_some() {
            return Cost::Unavailable {
                reason: "actual routed model is ambiguous for accounting".to_owned(),
            };
        }
        self.accounting_for_candidate(candidate, usage)
    }

    /// Prices usage using the opaque route identity emitted by the router.
    #[must_use]
    pub fn accounting_for_route(
        &self,
        route: Option<&str>,
        usage: rw_providers::TokenUsage,
    ) -> Cost {
        let Some(candidate) = route.and_then(|route| self.route_candidates.get(route)) else {
            return Cost::Unavailable {
                reason: "actual routed candidate is unavailable for accounting".to_owned(),
            };
        };
        self.accounting_for_candidate(candidate, usage)
    }

    fn accounting_for_candidate(&self, candidate: &str, usage: rw_providers::TokenUsage) -> Cost {
        let dynamic = self
            .dynamic_models
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(model) = self
            .models
            .get(candidate)
            .or_else(|| dynamic.get(candidate))
        else {
            return Cost::Unavailable {
                reason: "model candidate accounting is unavailable".to_owned(),
            };
        };
        let dynamic_providers = self
            .dynamic_providers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cached_metadata = self
            .providers
            .get(candidate)
            .or_else(|| dynamic_providers.get(candidate))
            .and_then(|provider| provider.cached_model_metadata());
        if cached_metadata.is_some() {
            return cost_from_model_metadata(
                &effective_model_metadata(model, cached_metadata),
                usage,
            );
        }
        cost_from_model_metadata(
            &ProviderModelMetadata {
                capabilities: model.capabilities.clone(),
                pricing: model.pricing.clone(),
                accounting: model.accounting,
            },
            usage,
        )
    }

    /// Dispatches through an alias after applying its configured thinking dial.
    ///
    /// # Errors
    ///
    /// Returns an error when the alias is absent or has no candidates.
    pub fn stream_alias(
        &self,
        alias: &str,
        mut request: ProviderRequest,
    ) -> Result<BoxEventStream, RouterError> {
        if self.models.contains_key(alias)
            || self
                .dynamic_models
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(alias)
        {
            return self.stream_concrete(alias, request);
        }
        if let Some(thinking) = self.alias_thinking.get(alias) {
            request.thinking = *thinking;
        }
        if let Some(candidates) = self
            .validated_alias_routes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(alias)
            .cloned()
        {
            return self.router.stream_candidates(alias, candidates, request);
        }
        self.router.stream_alias(alias, request)
    }

    /// Dispatches through exactly one configured provider for an alias.
    /// Routes on other providers are intentionally excluded rather than used
    /// as fallback after an explicit user selection.
    ///
    /// # Errors
    ///
    /// Returns an error when the alias is absent or has no route for the
    /// selected provider.
    pub fn stream_alias_provider(
        &self,
        alias: &str,
        provider: &str,
        mut request: ProviderRequest,
    ) -> Result<BoxEventStream, RouterError> {
        if alias
            .split_once('/')
            .is_some_and(|(candidate_provider, _)| candidate_provider == provider)
            && (self.models.contains_key(alias)
                || self
                    .dynamic_models
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(alias))
        {
            return self.stream_concrete(alias, request);
        }
        if let Some(thinking) = self.alias_thinking.get(alias) {
            request.thinking = *thinking;
        }
        let validated = self
            .validated_alias_routes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(alias)
            .cloned();
        let candidates = validated
            .as_deref()
            .unwrap_or(self.router.resolve(alias)?)
            .iter()
            .filter(|candidate| {
                self.route_candidates
                    .get(&candidate.provider)
                    .and_then(|route| route.split_once('/'))
                    .is_some_and(|(route_provider, _)| route_provider == provider)
            })
            .cloned()
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Err(RouterError::ProviderNotAvailable {
                alias: alias.to_owned(),
                provider: provider.to_owned(),
            });
        }
        self.router.stream_candidates(alias, candidates, request)
    }

    /// Whether an alias has an exact route through a configured provider.
    #[must_use]
    pub fn has_provider_for_alias(&self, alias: &str, provider: &str) -> bool {
        if alias
            .split_once('/')
            .is_some_and(|(candidate_provider, _)| candidate_provider == provider)
            && (self.models.contains_key(alias)
                || self
                    .dynamic_models
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(alias))
        {
            return true;
        }
        if let Some(routes) = self
            .validated_alias_routes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(alias)
        {
            return routes.iter().any(|candidate| {
                self.route_candidates
                    .get(&candidate.provider)
                    .and_then(|route| route.split_once('/'))
                    .is_some_and(|(route_provider, _)| route_provider == provider)
            });
        }
        self.alias_candidates.get(alias).is_some_and(|candidates| {
            candidates.iter().any(|candidate| {
                candidate
                    .split_once('/')
                    .is_some_and(|(route_provider, _)| route_provider == provider)
            })
        })
    }

    /// Resolves every live-catalog-backed route for an alias before its first
    /// inference. A successful provider catalog is authoritative: configured
    /// candidates omitted by it are not retained as router fallbacks.
    ///
    /// # Errors
    ///
    /// Returns a sanitized discovery or routing error when the selection has
    /// no candidate authorized by its provider's current live catalog.
    pub async fn prepare_model_selection(
        &self,
        selection: &str,
    ) -> Result<(), ProviderFactoryError> {
        match self.alias_candidates.get(selection).cloned() {
            Some(configured) => self.prepare_alias_selection(selection, &configured).await,
            None => self.prepare_concrete_selection(selection).await,
        }
    }

    async fn prepare_concrete_selection(
        &self,
        selection: &str,
    ) -> Result<(), ProviderFactoryError> {
        if !self.models.contains_key(selection) {
            return self.prepare_concrete_model(selection).await;
        }
        if self
            .validated_concrete_models
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(selection)
        {
            return Ok(());
        }
        let (provider, model) = parse_candidate(selection)?;
        match self.catalog_authority(provider).await {
            CatalogAuthority::Unavailable(reason) => {
                return Err(ProviderFactoryError::new(provider, reason));
            }
            CatalogAuthority::Models(models) if !models.contains(model) => {
                return Err(ProviderFactoryError::new(
                    provider,
                    "model is not in the live catalog",
                ));
            }
            CatalogAuthority::NotExposed | CatalogAuthority::Models(_) => {}
        }
        self.validated_concrete_models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(selection.to_owned());
        Ok(())
    }

    async fn prepare_alias_selection(
        &self,
        selection: &str,
        configured: &[String],
    ) -> Result<(), ProviderFactoryError> {
        if self
            .validated_alias_routes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(selection)
        {
            return Ok(());
        }
        let mut authorities = BTreeMap::<String, CatalogAuthority>::new();
        for candidate in configured {
            let (provider, _) = parse_candidate(candidate)?;
            if authorities.contains_key(provider) {
                continue;
            }
            authorities.insert(provider.to_owned(), self.catalog_authority(provider).await);
        }
        let mut excluded_reason = None;
        let allowed = configured
            .iter()
            .filter_map(|candidate| {
                let (provider, model) = candidate.split_once('/')?;
                match authorities.get(provider)? {
                    CatalogAuthority::NotExposed => Some(candidate.as_str()),
                    CatalogAuthority::Models(models) if models.contains(model) => {
                        Some(candidate.as_str())
                    }
                    CatalogAuthority::Models(_) => {
                        excluded_reason = Some("model is not in the live catalog".to_owned());
                        None
                    }
                    CatalogAuthority::Unavailable(reason) => {
                        excluded_reason = Some(reason.clone());
                        None
                    }
                }
            })
            .collect::<std::collections::BTreeSet<_>>();
        let routes = self
            .router
            .resolve(selection)
            .map_err(|error| ProviderFactoryError::new("models", error.to_string()))?
            .iter()
            .filter(|route| {
                self.route_candidates
                    .get(&route.provider)
                    .is_some_and(|candidate| allowed.contains(candidate.as_str()))
            })
            .cloned()
            .collect::<Vec<_>>();
        if routes.is_empty() {
            return Err(ProviderFactoryError::new(
                "models",
                excluded_reason.unwrap_or_else(|| {
                    format!("model alias {selection:?} has no live provider route")
                }),
            ));
        }
        self.validated_alias_routes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(selection.to_owned(), routes);
        Ok(())
    }

    async fn catalog_authority(&self, provider: &str) -> CatalogAuthority {
        let discovery = self
            .discovery_providers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
            .cloned();
        let Some(discovery) = discovery else {
            return CatalogAuthority::NotExposed;
        };
        match tokio::time::timeout(self.model_discovery_timeout, discovery.discover_models()).await
        {
            Err(_) => CatalogAuthority::Unavailable("model discovery timed out".to_owned()),
            Ok(Err(error)) => {
                CatalogAuthority::Unavailable(provider_discovery_status(&error).to_owned())
            }
            Ok(Ok(None)) => CatalogAuthority::NotExposed,
            Ok(Ok(Some(catalog))) => {
                CatalogAuthority::Models(catalog.models.into_iter().map(|model| model.id).collect())
            }
        }
    }

    fn stream_concrete(
        &self,
        candidate: &str,
        mut request: ProviderRequest,
    ) -> Result<BoxEventStream, RouterError> {
        let model = self
            .dynamic_models
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(candidate)
            .map(|model| model.model.clone())
            .or_else(|| self.models.get(candidate).map(|model| model.model.clone()))
            .ok_or_else(|| RouterError::AliasNotConfigured(candidate.to_owned()))?;
        let provider = self
            .dynamic_providers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(candidate)
            .cloned()
            .or_else(|| self.providers.get(candidate).cloned());
        let provider =
            provider.ok_or_else(|| RouterError::AliasNotConfigured(candidate.to_owned()))?;
        request.model = model;
        let stream = async_stream::try_stream! {
            let mut inner = provider.stream(request).await?;
            while let Some(event) = inner.next().await {
                yield event?;
            }
        };
        Ok(Box::pin(stream))
    }

    /// Authenticates and binds one concrete live-discovered model so a later
    /// synchronous turn dispatch can use it without trusting a client string.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error when the provider is inactive, discovery
    /// fails or times out, the id is absent, or adapter construction fails.
    pub async fn prepare_concrete_model(
        &self,
        candidate: &str,
    ) -> Result<(), ProviderFactoryError> {
        self.prepare_concrete_model_inner(candidate, false).await
    }

    /// Re-discovers and rebinds an exact concrete model after its provider's
    /// credentials or endpoint have been replaced.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error when discovery or adapter construction fails.
    pub async fn refresh_concrete_model(
        &self,
        candidate: &str,
    ) -> Result<(), ProviderFactoryError> {
        self.prepare_concrete_model_inner(candidate, true).await
    }

    async fn prepare_concrete_model_inner(
        &self,
        candidate: &str,
        force: bool,
    ) -> Result<(), ProviderFactoryError> {
        if !force
            && (self.models.contains_key(candidate)
                || self
                    .dynamic_models
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(candidate))
        {
            return Ok(());
        }
        let (provider_name, model) = parse_candidate(candidate)?;
        let discovery_provider = self
            .discovery_providers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider_name)
            .cloned()
            .ok_or_else(|| {
                ProviderFactoryError::new(provider_name, "provider has no discovery route")
            })?;
        let catalog = tokio::time::timeout(
            self.model_discovery_timeout,
            discovery_provider.discover_models(),
        )
        .await
        .map_err(|_| ProviderFactoryError::new(provider_name, "model discovery timed out"))?
        .map_err(|error| {
            ProviderFactoryError::new(provider_name, provider_discovery_status(&error))
        })?
        .ok_or_else(|| {
            ProviderFactoryError::new(provider_name, "provider has no live model catalog")
        })?;
        let discovered = catalog
            .models
            .into_iter()
            .find(|entry| entry.id == model)
            .ok_or_else(|| {
                ProviderFactoryError::new(provider_name, "model is not in the live catalog")
            })?;
        if let Some(provider) = self.extension_providers.get(provider_name) {
            self.bind_extension_discovered_model(
                candidate,
                provider_name,
                model,
                Arc::clone(provider),
                discovered,
            )?;
            return Ok(());
        }
        let connections = self
            .connections
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let connection = connections.get(provider_name).ok_or_else(|| {
            ProviderFactoryError::new(provider_name, "provider is not active in this session")
        })?;
        self.bind_discovered_model(candidate, provider_name, model, connection, discovered)
    }

    /// Re-composes one configured provider after an in-app authentication flow
    /// stores its credential. No process or session restart is required.
    ///
    /// # Errors
    ///
    /// Returns a sanitized composition error if the provider is unknown or its
    /// newly stored authentication material still cannot be resolved.
    pub fn activate_provider(&self, provider: &str) -> Result<(), ProviderFactoryError> {
        let activated = self.provider_activator.activate(provider)?;
        self.redactor.merge_from(&activated.redactor);
        self.connections
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider.to_owned(), activated.connection);
        self.discovery_providers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider.to_owned(), activated.discovery_provider);
        self.dynamic_providers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|candidate, _| {
                candidate
                    .split_once('/')
                    .is_none_or(|(owner, _)| owner != provider)
            });
        self.dynamic_models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|candidate, _| {
                candidate
                    .split_once('/')
                    .is_none_or(|(owner, _)| owner != provider)
            });
        self.invalidate_catalog_authority(provider);
        Ok(())
    }

    fn invalidate_catalog_authority(&self, provider: &str) {
        self.validated_concrete_models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|candidate| {
                candidate
                    .split_once('/')
                    .is_none_or(|(owner, _)| owner != provider)
            });
        self.validated_alias_routes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|alias, _| {
                self.alias_candidates.get(alias).is_none_or(|candidates| {
                    candidates.iter().all(|candidate| {
                        candidate
                            .split_once('/')
                            .is_none_or(|(owner, _)| owner != provider)
                    })
                })
            });
    }

    fn bind_extension_discovered_model(
        &self,
        candidate: &str,
        provider_name: &str,
        model: &str,
        inner: Arc<dyn Provider>,
        discovered: rw_providers::DiscoveredModel,
    ) -> Result<(), ProviderFactoryError> {
        let fallback = inner.capabilities();
        let mut capabilities = discovered.capabilities.unwrap_or(fallback.clone());
        capabilities.max_context_tokens = capabilities
            .max_context_tokens
            .or(fallback.max_context_tokens);
        capabilities.max_output_tokens = capabilities
            .max_output_tokens
            .or(fallback.max_output_tokens);
        let supported_thinking = if capabilities.thinking {
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ]
        } else {
            Vec::new()
        };
        let configured_pricing = declared_pricing(&self.config, provider_name, model);
        let metadata_accounting = inner
            .cached_model_metadata_for(model)
            .map_or(UsageAccounting::UnpricedApi, |value| value.accounting);
        if configured_pricing.is_some()
            && matches!(
                metadata_accounting,
                UsageAccounting::SubscriptionQuota | UsageAccounting::AiCredits { .. }
            )
        {
            return Err(ProviderFactoryError::new(
                provider_name,
                "extension uses subscription or credit accounting and cannot declare API pricing",
            ));
        }
        let (_, catalog_pricing) = find_pricing(&self.pricing_table, provider_name, model, None);
        let (pricing, pricing_source) =
            effective_pricing(configured_pricing, discovered.pricing, catalog_pricing);
        let accounting = match metadata_accounting {
            UsageAccounting::SubscriptionQuota | UsageAccounting::AiCredits { .. } => {
                metadata_accounting
            }
            _ if pricing.is_some() => UsageAccounting::ApiDollars,
            _ => UsageAccounting::UnpricedApi,
        };
        let bounded: Arc<dyn Provider> = Arc::new(ModelBoundProvider {
            inner,
            name: candidate.to_owned(),
            expected_model: model.to_owned(),
            capabilities: capabilities.clone(),
            supported_thinking,
            defer_capabilities: false,
        });
        self.dynamic_providers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(candidate.to_owned(), bounded);
        self.dynamic_models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                candidate.to_owned(),
                ResolvedModel {
                    candidate: candidate.to_owned(),
                    provider: provider_name.to_owned(),
                    model: model.to_owned(),
                    catalog_model: None,
                    capabilities,
                    accounting,
                    pricing,
                    pricing_source,
                },
            );
        Ok(())
    }

    fn bind_discovered_model(
        &self,
        candidate: &str,
        provider_name: &str,
        model: &str,
        connection: &ProviderConnection,
        discovered: rw_providers::DiscoveredModel,
    ) -> Result<(), ProviderFactoryError> {
        let (_, catalog_pricing) = find_pricing(
            &self.pricing_table,
            provider_name,
            model,
            connection.kind.catalog_namespace(),
        );
        let fallback = match connection.kind {
            AdapterKind::OpenAiSubscription => {
                subscription_model_capabilities(catalog_pricing.as_ref())
            }
            AdapterKind::GitHubCopilot => github_copilot_capabilities(catalog_pricing.as_ref()),
            kind => model_capabilities(kind, catalog_pricing.as_ref()),
        };
        let defer_capabilities = connection.kind == AdapterKind::GitHubCopilot
            || (discovered.capabilities.is_none() && catalog_pricing.is_none());
        let mut capabilities = discovered.capabilities.unwrap_or(fallback.clone());
        capabilities.max_context_tokens = capabilities
            .max_context_tokens
            .or(fallback.max_context_tokens);
        capabilities.max_output_tokens = capabilities
            .max_output_tokens
            .or(fallback.max_output_tokens);
        let supported_thinking = if capabilities.thinking {
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ]
        } else {
            Vec::new()
        };
        let inner = construct_adapter(
            candidate,
            connection,
            self.network_policy,
            &capabilities,
            &supported_thinking,
            defer_capabilities,
        )?;
        let bounded: Arc<dyn Provider> = Arc::new(ModelBoundProvider {
            inner,
            name: candidate.to_owned(),
            expected_model: model.to_owned(),
            capabilities: capabilities.clone(),
            supported_thinking,
            defer_capabilities,
        });
        let (pricing, pricing_source) = effective_pricing(
            declared_pricing(&self.config, provider_name, model),
            discovered.pricing,
            catalog_pricing,
        );
        let accounting = match connection.kind {
            AdapterKind::OpenAiSubscription => UsageAccounting::SubscriptionQuota,
            AdapterKind::GitHubCopilot => UsageAccounting::AiCredits {
                micros_usd_per_credit: 10_000,
            },
            _ if pricing.is_some() => UsageAccounting::ApiDollars,
            _ => UsageAccounting::UnpricedApi,
        };
        self.dynamic_providers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(candidate.to_owned(), bounded);
        self.dynamic_models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                candidate.to_owned(),
                ResolvedModel {
                    candidate: candidate.to_owned(),
                    provider: provider_name.to_owned(),
                    model: model.to_owned(),
                    catalog_model: None,
                    capabilities,
                    pricing,
                    pricing_source,
                    accounting,
                },
            );
        Ok(())
    }
}

async fn discover_runtime_provider(
    provider_name: String,
    provider: Arc<dyn Provider>,
    discovery_timeout: std::time::Duration,
) -> (
    String,
    String,
    bool,
    Result<rw_providers::DiscoveredProviderCatalog, String>,
) {
    let candidate = discovery_candidate(&provider_name);
    let discovery = tokio::time::timeout(discovery_timeout, provider.discover_models())
        .await
        .map_err(|_| "model discovery timed out".to_owned())
        .and_then(|result| result.map_err(|error| provider_discovery_status(&error).to_owned()))
        .and_then(|catalog| {
            catalog.ok_or_else(|| "provider does not expose live model discovery".to_owned())
        });
    (provider_name, candidate, true, discovery)
}

#[async_trait]
impl ModelCatalogSource for ProviderRuntime {
    async fn discover(&self) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        let providers = self
            .discovery_providers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(name, provider)| (name.clone(), Arc::clone(provider)))
            .collect::<Vec<_>>();
        let discovery_timeout = self.model_discovery_timeout;
        let pending = providers
            .into_iter()
            .map(|(provider_name, provider)| {
                discover_runtime_provider(provider_name, provider, discovery_timeout)
            })
            .collect::<Vec<_>>();
        let discoveries = futures_util::stream::iter(pending)
            .buffer_unordered(4)
            .collect::<Vec<_>>()
            .await;
        Ok(project_model_catalog(
            &self.config,
            &self.pricing_table,
            discoveries,
        ))
    }

    async fn discover_provider(
        &self,
        provider: &str,
    ) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        let discovery_provider = self
            .discovery_providers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
            .cloned()
            .ok_or_else(|| ModelCatalogError(format!("provider {provider:?} is unavailable")))?;
        let discovery = discover_runtime_provider(
            provider.to_owned(),
            discovery_provider,
            self.model_discovery_timeout,
        )
        .await;
        Ok(project_model_catalog(
            &self.config,
            &self.pricing_table,
            vec![discovery],
        ))
    }
}

/// Alias/model-bound adapter from provider streams to the public web-search
/// boundary. It uses the ordinary provider request path, preserving recorder
/// and replay semantics.
pub struct ProviderNativeWebSearcher {
    provider: Arc<dyn Provider>,
    model: String,
}

struct ProviderNativeWebSearchRouter {
    candidates: Vec<Arc<dyn WebSearcher>>,
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

/// Injectable production provider-composition boundary.
pub struct ProviderFactory<E = SystemEnvironment, K = NoExternalCredentialStore> {
    credentials: Arc<CredentialManager<E, K>>,
    proxy_environment: ProxyEnvironment,
    network_policy: NetworkPolicy,
    pricing: PricingTable,
    retry: RetryPolicy,
    github_copilot_test_origins: BTreeMap<String, GitHubCopilotTestOrigin>,
    extension_providers: Vec<(String, Arc<dyn Provider>)>,
    model_discovery_timeout: std::time::Duration,
}

impl<E, K> Clone for ProviderFactory<E, K> {
    fn clone(&self) -> Self {
        Self {
            credentials: Arc::clone(&self.credentials),
            proxy_environment: self.proxy_environment.clone(),
            network_policy: self.network_policy,
            pricing: self.pricing.clone(),
            retry: self.retry.clone(),
            github_copilot_test_origins: self.github_copilot_test_origins.clone(),
            extension_providers: self.extension_providers.clone(),
            model_discovery_timeout: self.model_discovery_timeout,
        }
    }
}

#[derive(Clone)]
struct GitHubCopilotTestOrigin {
    origin: Url,
    oauth_client_id: String,
}

impl ProviderFactory<SystemEnvironment, NoExternalCredentialStore> {
    /// Creates a production factory using process environment and the
    /// owner-private credential file. No operating-system credential store is used.
    #[must_use]
    pub fn system(credentials_path: impl Into<PathBuf>, pricing: PricingTable) -> Self {
        Self::with_backends(
            Arc::new(CredentialManager::system(credentials_path)),
            ProxyEnvironment::capture(),
            NetworkPolicy::Allow,
            pricing,
        )
    }
}

impl<E, K> ProviderFactory<E, K>
where
    E: CredentialEnvironment + Send + Sync + 'static,
    K: CredentialStore + Send + Sync + 'static,
{
    /// Creates a deterministic factory with injected credential/network boundaries.
    #[must_use]
    pub fn with_backends(
        credentials: Arc<CredentialManager<E, K>>,
        proxy_environment: ProxyEnvironment,
        network_policy: NetworkPolicy,
        pricing: PricingTable,
    ) -> Self {
        Self {
            credentials,
            proxy_environment,
            network_policy,
            pricing,
            retry: RetryPolicy::default(),
            github_copilot_test_origins: BTreeMap::new(),
            extension_providers: Vec::new(),
            model_discovery_timeout: MODEL_DISCOVERY_TIMEOUT,
        }
    }

    /// Replaces the bounded router retry policy.
    #[must_use]
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Replaces the bounded live model-discovery deadline.
    #[doc(hidden)]
    #[must_use]
    pub fn with_model_discovery_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.model_discovery_timeout = timeout;
        self
    }

    /// Adds already-approved extension providers under their declared alias prefixes.
    ///
    /// Each prefix must be a canonical provider name followed by `/`, for
    /// example `acme/`. Prefixes are validated together with built-in provider
    /// names during [`Self::build`], before the immutable router is created.
    /// The adapter's own name is intentionally not used for routing or exposed
    /// as model metadata.
    #[must_use]
    pub fn with_extension_providers<I, S>(mut self, providers: I) -> Self
    where
        I: IntoIterator<Item = (S, Arc<dyn Provider>)>,
        S: Into<String>,
    {
        self.extension_providers.extend(
            providers
                .into_iter()
                .map(|(prefix, provider)| (prefix.into(), provider)),
        );
        self
    }

    /// Injects a loopback-only Copilot origin for deterministic acceptance tests.
    /// Production composition must use the fixed public origin.
    #[doc(hidden)]
    #[must_use]
    pub fn with_github_copilot_test_origin(
        mut self,
        provider: impl Into<String>,
        origin: Url,
        oauth_client_id: impl Into<String>,
    ) -> Self {
        self.github_copilot_test_origins.insert(
            provider.into(),
            GitHubCopilotTestOrigin {
                origin,
                oauth_client_id: oauth_client_id.into(),
            },
        );
        self
    }

    /// Resolves credentials/proxies and constructs all model-bound adapters.
    ///
    /// # Errors
    ///
    /// Fails closed on unsafe endpoints, ambiguous authentication, missing
    /// credentials, unsupported adapter kinds, or malformed model aliases.
    #[allow(clippy::too_many_lines)]
    pub fn build(&self, config: &Config) -> Result<ProviderRuntime, ProviderFactoryError> {
        config
            .compaction
            .validate()
            .map_err(|error| ProviderFactoryError::new("compaction", error))?;
        config
            .budget
            .validate()
            .map_err(|error| ProviderFactoryError::new("budget", error))?;
        if config.models.aliases.is_empty() {
            return Err(ProviderFactoryError::new(
                "models",
                "at least one model alias must be configured",
            ));
        }
        if !config.models.aliases.contains_key(&config.models.default) {
            return Err(ProviderFactoryError::new(
                "models",
                "models.default must name a configured alias",
            ));
        }
        if let Some(alias) = config
            .models
            .thinking
            .keys()
            .find(|alias| !config.models.aliases.contains_key(*alias))
        {
            return Err(ProviderFactoryError::new(
                "models",
                format!("thinking configuration references unknown alias {alias:?}"),
            ));
        }
        if let Some(alias) = config
            .models
            .aliases
            .iter()
            .find_map(|(alias, candidates)| {
                (alias.trim().is_empty() || candidates.is_empty()).then_some(alias)
            })
        {
            return Err(ProviderFactoryError::new(
                "models",
                format!("model alias {alias:?} must be non-empty and have candidates"),
            ));
        }
        let extension_providers =
            validate_extension_providers(&self.extension_providers, &config.providers)?;
        validate_proxy_auth_fields(
            "network",
            config.network.proxy.as_deref(),
            config.network.proxy_username.as_deref(),
            config.network.proxy_password_credential.as_deref(),
        )?;
        for (name, provider) in &config.providers {
            provider
                .validate_gateway_options()
                .map_err(|error| ProviderFactoryError::new(name, error))?;
            provider
                .validate_pricing()
                .map_err(|error| ProviderFactoryError::new(name, error))?;
            validate_proxy_auth_fields(
                name,
                provider.proxy.as_deref(),
                provider.proxy_username.as_deref(),
                provider.proxy_password_credential.as_deref(),
            )?;
        }
        let global_proxy = parse_optional_proxy("network", config.network.proxy.as_deref())?;
        let mut per_provider = BTreeMap::new();
        for (name, provider) in &config.providers {
            if let Some(proxy) = parse_optional_proxy(name, provider.proxy.as_deref())? {
                per_provider.insert(name.clone(), proxy);
            }
        }
        let proxies = ProxySettings {
            global: global_proxy,
            per_provider,
            environment: self.proxy_environment.clone(),
        };

        let mut unique_candidates = BTreeMap::new();
        for candidates in config.models.aliases.values() {
            for candidate in candidates {
                let (provider, model) = parse_candidate(candidate)?;
                unique_candidates
                    .entry(candidate.clone())
                    .or_insert_with(|| (provider.to_owned(), model.to_owned()));
            }
        }

        let mut registry = Vec::new();
        let mut router_aliases = BTreeMap::new();
        let mut providers = BTreeMap::new();
        let mut models = BTreeMap::new();
        let redactor = FixtureRedactor::default();
        let warnings = RuntimeWarnings::default();
        let mut registration_keys = BTreeMap::new();

        // Authentication and proxy state is endpoint-scoped, not model-scoped.
        // Sharing one refresh source prevents concurrent model adapters from
        // racing refresh-token rotation or duplicating token exchanges. Each
        // provider composes independently so a broken first candidate cannot
        // suppress a later healthy route in the same alias chain.
        let mut connections = BTreeMap::new();
        for (provider_name, provider_config) in &config.providers {
            if extension_providers.contains_key(&format!("{provider_name}/")) {
                continue;
            }
            let resolved: Result<ProviderConnection, ProviderFactoryError> = (|| {
                let kind = AdapterKind::parse(provider_name, &provider_config.kind)?;
                let endpoint = resolve_endpoint(provider_name, provider_config, kind)?;
                let proxy = proxies.resolve(provider_name, &endpoint);
                let proxy_authentication = self.resolve_proxy_authentication(
                    provider_name,
                    provider_config,
                    &config.network,
                    proxy.as_ref().map(|resolution| resolution.source),
                    &redactor,
                    &warnings,
                )?;
                let auth = if kind == AdapterKind::OpenAiSubscription {
                    self.resolve_openai_subscription_auth(
                        provider_name,
                        provider_config,
                        proxy.as_ref().map(|value| &value.url),
                        proxy_authentication.as_ref(),
                        &redactor,
                        &warnings,
                    )?
                } else if kind == AdapterKind::GitHubCopilot {
                    Arc::new(StaticAuth::new(AuthMaterial::None)) as Arc<dyn AuthProvider>
                } else {
                    self.resolve_auth(
                        provider_name,
                        provider_config,
                        kind,
                        &endpoint,
                        proxy.as_ref().map(|value| &value.url),
                        proxy_authentication.as_ref(),
                        &redactor,
                        &warnings,
                    )?
                };
                let copilot_runtime = if kind == AdapterKind::GitHubCopilot {
                    Some(self.resolve_github_copilot_runtime(
                        provider_name,
                        provider_config,
                        proxy.as_ref().map(|value| &value.url),
                        proxy_authentication.as_ref(),
                        &redactor,
                        &warnings,
                    )?)
                } else {
                    None
                };
                let header_credentials = self.resolve_header_credentials(
                    provider_name,
                    provider_config,
                    &redactor,
                    &warnings,
                )?;
                Ok(ProviderConnection {
                    kind,
                    endpoint,
                    auth,
                    copilot_runtime,
                    proxy: proxy.map(|value| value.url),
                    proxy_authentication,
                    headers: provider_config.headers.clone(),
                    header_credentials,
                    extra_body: provider_config.extra_body.clone(),
                    model_ids: provider_config.model_ids.clone(),
                    path_template: provider_config.path_template.clone(),
                })
            })();
            match resolved {
                Ok(connection) => {
                    connections.insert(provider_name.clone(), connection);
                }
                Err(error) => warnings.extend([format!(
                    "provider {provider_name:?} is unavailable for live discovery: {}",
                    error.reason
                )]),
            }
        }

        let mut route_candidates = BTreeMap::new();
        for (index, (candidate, (provider_name, model))) in unique_candidates.iter().enumerate() {
            if let Some(inner) = extension_providers.get(&format!("{provider_name}/")) {
                let metadata = inner.cached_model_metadata_for(model);
                let capabilities = metadata
                    .as_ref()
                    .map_or_else(|| inner.capabilities(), |value| value.capabilities.clone());
                let configured_pricing = declared_pricing(config, provider_name, model);
                let discovered_pricing = metadata.as_ref().and_then(|value| value.pricing.clone());
                let (_, catalog_pricing) = find_pricing(&self.pricing, provider_name, model, None);
                let discovered_accounting = metadata
                    .as_ref()
                    .map_or(UsageAccounting::UnpricedApi, |value| value.accounting);
                if configured_pricing.is_some()
                    && matches!(
                        discovered_accounting,
                        UsageAccounting::SubscriptionQuota | UsageAccounting::AiCredits { .. }
                    )
                {
                    return Err(ProviderFactoryError::new(
                        provider_name,
                        "extension uses subscription or credit accounting and cannot declare API pricing",
                    ));
                }
                let (pricing, pricing_source) =
                    effective_pricing(configured_pricing, discovered_pricing, catalog_pricing);
                let accounting = match discovered_accounting {
                    UsageAccounting::SubscriptionQuota | UsageAccounting::AiCredits { .. } => {
                        discovered_accounting
                    }
                    _ if pricing.is_some() => UsageAccounting::ApiDollars,
                    _ => UsageAccounting::UnpricedApi,
                };
                let supported_thinking = if capabilities.thinking {
                    vec![
                        ThinkingLevel::Low,
                        ThinkingLevel::Medium,
                        ThinkingLevel::High,
                    ]
                } else {
                    Vec::new()
                };
                let bounded: Arc<dyn Provider> = Arc::new(ModelBoundProvider {
                    inner: Arc::clone(inner),
                    name: candidate.clone(),
                    expected_model: model.clone(),
                    capabilities: capabilities.clone(),
                    supported_thinking,
                    defer_capabilities: false,
                });
                let registration_key = format!("__model_{index:08}");
                registration_keys.insert(candidate.clone(), registration_key.clone());
                route_candidates.insert(registration_key.clone(), candidate.clone());
                registry.push((registration_key, Arc::clone(&bounded)));
                providers.insert(candidate.clone(), bounded);
                models.insert(
                    candidate.clone(),
                    ResolvedModel {
                        candidate: candidate.clone(),
                        provider: provider_name.clone(),
                        model: model.clone(),
                        catalog_model: None,
                        capabilities,
                        pricing,
                        pricing_source,
                        accounting,
                    },
                );
                continue;
            }
            let Some(connection) = connections.get(provider_name) else {
                warnings.extend([format!(
                    "model candidate {candidate:?} is unavailable because provider {provider_name:?} could not be composed"
                )]);
                continue;
            };
            let kind = connection.kind;
            let (catalog_model, catalog_pricing) = if kind == AdapterKind::GitHubCopilot {
                (None, None)
            } else {
                find_pricing(
                    &self.pricing,
                    provider_name,
                    model,
                    kind.catalog_namespace(),
                )
            };
            let supported_thinking = if kind == AdapterKind::OpenAiSubscription {
                vec![
                    ThinkingLevel::Off,
                    ThinkingLevel::Low,
                    ThinkingLevel::Medium,
                    ThinkingLevel::High,
                ]
            } else {
                catalog_pricing
                    .as_ref()
                    .map_or_else(Vec::new, |value| value.reasoning_efforts.clone())
            };
            let capability_pricing = if kind == AdapterKind::GitHubCopilot {
                find_pricing(&self.pricing, provider_name, model, Some("github-copilot")).1
            } else {
                catalog_pricing.clone()
            };
            let capabilities = match kind {
                AdapterKind::OpenAiSubscription => {
                    subscription_model_capabilities(capability_pricing.as_ref())
                }
                AdapterKind::GitHubCopilot => {
                    github_copilot_capabilities(capability_pricing.as_ref())
                }
                _ => model_capabilities(kind, catalog_pricing.as_ref()),
            };
            // A configured route remains usable when its provider does not
            // expose model discovery and no cached metadata describes the
            // model. In that case capability support is unknown, not false:
            // let the protocol endpoint decide instead of rejecting the turn
            // locally. Live-discovered and cached capabilities remain strict.
            let defer_capabilities = kind == AdapterKind::GitHubCopilot
                || (!matches!(kind, AdapterKind::OpenAiSubscription)
                    && capability_pricing.is_none());
            let (pricing, pricing_source) = effective_pricing(
                declared_pricing(config, provider_name, model),
                None,
                catalog_pricing,
            );
            let accounting = match kind {
                AdapterKind::OpenAiSubscription => UsageAccounting::SubscriptionQuota,
                AdapterKind::GitHubCopilot => UsageAccounting::AiCredits {
                    micros_usd_per_credit: 10_000,
                },
                _ if pricing.is_some() => UsageAccounting::ApiDollars,
                _ => UsageAccounting::UnpricedApi,
            };
            let inner = match construct_adapter(
                candidate,
                connection,
                self.network_policy,
                &capabilities,
                &supported_thinking,
                defer_capabilities,
            ) {
                Ok(inner) => inner,
                Err(error) => {
                    warnings.extend([format!(
                        "model candidate {candidate:?} could not be composed: {}",
                        error.reason
                    )]);
                    continue;
                }
            };
            let bounded: Arc<dyn Provider> = Arc::new(ModelBoundProvider {
                inner,
                name: candidate.clone(),
                expected_model: model.clone(),
                capabilities: capabilities.clone(),
                supported_thinking,
                defer_capabilities,
            });
            let registration_key = format!("__model_{index:08}");
            registration_keys.insert(candidate.clone(), registration_key.clone());
            route_candidates.insert(registration_key.clone(), candidate.clone());
            registry.push((registration_key, Arc::clone(&bounded)));
            providers.insert(candidate.clone(), bounded);
            models.insert(
                candidate.clone(),
                ResolvedModel {
                    candidate: candidate.clone(),
                    provider: provider_name.clone(),
                    model: model.clone(),
                    catalog_model,
                    capabilities,
                    pricing: if matches!(
                        kind,
                        AdapterKind::OpenAiSubscription | AdapterKind::GitHubCopilot
                    ) {
                        None
                    } else {
                        pricing
                    },
                    pricing_source: if matches!(
                        kind,
                        AdapterKind::OpenAiSubscription | AdapterKind::GitHubCopilot
                    ) {
                        None
                    } else {
                        pricing_source
                    },
                    accounting,
                },
            );
        }

        for (alias, candidates) in &config.models.aliases {
            let routed = candidates
                .iter()
                .filter_map(|candidate| {
                    let registration = registration_keys.get(candidate)?;
                    let model = &unique_candidates.get(candidate)?.1;
                    Some(format!("{registration}/{model}"))
                })
                .collect::<Vec<_>>();
            if routed.is_empty() {
                if let Some((provider_name, _)) = candidates
                    .iter()
                    .filter_map(|candidate| unique_candidates.get(candidate))
                    .find(|(provider_name, _)| {
                        !config.providers.contains_key(provider_name)
                            && !extension_providers.contains_key(&format!("{provider_name}/"))
                    })
                {
                    return Err(ProviderFactoryError::new(
                        provider_name,
                        "model candidate references an unconfigured provider",
                    ));
                }
                return Err(ProviderFactoryError::new(
                    "models",
                    format!("model alias {alias:?} has no usable provider route"),
                ));
            }
            router_aliases.insert(alias.clone(), routed);
        }
        let router = ProviderRouter::with_registry(router_aliases, registry, self.retry.clone())
            .map_err(|error| ProviderFactoryError::new("models", error.to_string()))?;
        let alias_thinking = config
            .models
            .thinking
            .iter()
            .map(|(alias, level)| (alias.clone(), *level))
            .collect();
        let alias_candidates = config.models.aliases.clone();
        let mut discovery_providers: BTreeMap<String, Arc<dyn Provider>> = connections
            .iter()
            .filter_map(|(provider_name, connection)| {
                let candidate = discovery_candidate(provider_name);
                let capabilities = match connection.kind {
                    AdapterKind::OpenAiSubscription => subscription_model_capabilities(None),
                    AdapterKind::GitHubCopilot => github_copilot_capabilities(None),
                    kind => model_capabilities(kind, None),
                };
                construct_adapter(
                    &candidate,
                    connection,
                    self.network_policy,
                    &capabilities,
                    &[],
                    false,
                )
                .ok()
                .map(|provider| (provider_name.clone(), provider))
            })
            .collect();
        let extension_providers = extension_providers
            .into_iter()
            .map(|(prefix, provider)| (prefix.trim_end_matches('/').to_owned(), provider))
            .collect::<BTreeMap<_, _>>();
        discovery_providers.extend(
            extension_providers
                .iter()
                .map(|(name, provider)| (name.clone(), Arc::clone(provider))),
        );
        Ok(ProviderRuntime {
            router,
            providers,
            models,
            dynamic_providers: std::sync::RwLock::new(BTreeMap::new()),
            dynamic_models: std::sync::RwLock::new(BTreeMap::new()),
            connections: std::sync::RwLock::new(connections),
            discovery_providers: std::sync::RwLock::new(discovery_providers),
            extension_providers,
            provider_activator: Arc::new(FactoryProviderActivator {
                factory: self.clone(),
                config: config.clone(),
            }),
            network_policy: self.network_policy,
            model_discovery_timeout: self.model_discovery_timeout,
            alias_thinking,
            alias_candidates,
            validated_alias_routes: std::sync::RwLock::new(BTreeMap::new()),
            validated_concrete_models: std::sync::RwLock::new(std::collections::BTreeSet::new()),
            route_candidates,
            default_alias: config.models.default.clone(),
            redactor,
            warnings,
            pricing_table: self.pricing.clone(),
            config: config.clone(),
            compaction: config.compaction.clone(),
            budget: config.budget.clone(),
        })
    }

    /// Discovers every configured provider independently and projects a
    /// provider-neutral concrete model catalog. Live provider ids are the
    /// availability source; models.dev is enrichment only.
    ///
    /// # Errors
    ///
    /// Returns only configuration-wide failures. Per-provider auth, network,
    /// and protocol failures are retained as visible provider status rows.
    pub async fn discover_model_catalog(
        &self,
        config: &Config,
    ) -> Result<ModelCatalogSnapshot, ProviderFactoryError> {
        let provider_names = config.providers.keys().cloned().collect::<Vec<_>>();
        let discoveries =
            futures_util::stream::iter(provider_names.into_iter().map(|provider_name| {
                let discovery_factory = self.clone();
                async move {
                    let candidate = discovery_candidate(&provider_name);
                    let mut isolated = config.clone();
                    isolated.providers.retain(|name, _| name == &provider_name);
                    isolated.models.aliases =
                        BTreeMap::from([("__catalog".to_owned(), vec![candidate.clone()])]);
                    "__catalog".clone_into(&mut isolated.models.default);
                    isolated.models.thinking.clear();
                    let runtime = match discovery_factory.build(&isolated) {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            return (provider_name, candidate, false, Err(error.to_string()));
                        }
                    };
                    let Some(provider) = runtime.provider(&candidate) else {
                        return (
                            provider_name,
                            candidate,
                            true,
                            Err("catalog provider was not composed".to_owned()),
                        );
                    };
                    let discovered = tokio::time::timeout(
                        self.model_discovery_timeout,
                        provider.discover_models(),
                    )
                    .await
                    .map_err(|_| "model discovery timed out".to_owned())
                    .and_then(|result| {
                        result.map_err(|error| provider_discovery_status(&error).to_owned())
                    })
                    .and_then(|catalog| {
                        catalog.ok_or_else(|| {
                            "provider does not expose live model discovery".to_owned()
                        })
                    });
                    (provider_name, candidate, true, discovered)
                }
            }))
            .buffer_unordered(4)
            .collect::<Vec<_>>()
            .await;

        Ok(project_model_catalog(config, &self.pricing, discoveries))
    }

    /// Discovers one configured provider in isolation. This is the catalog
    /// counterpart to provider activation: credential resolution and network
    /// access are bounded to the selected provider.
    ///
    /// # Errors
    ///
    /// Returns a sanitized configuration error when the provider is unknown;
    /// provider authentication and discovery failures remain visible in the
    /// returned provider row.
    pub async fn discover_provider_model_catalog(
        &self,
        config: &Config,
        provider: &str,
    ) -> Result<ModelCatalogSnapshot, ProviderFactoryError> {
        let provider_config = config
            .providers
            .get(provider)
            .cloned()
            .ok_or_else(|| ProviderFactoryError::new(provider, "provider is not configured"))?;
        let mut isolated = config.clone();
        isolated.providers = BTreeMap::from([(provider.to_owned(), provider_config)]);
        isolated.models.aliases.retain(|_, candidates| {
            candidates.retain(|candidate| {
                candidate
                    .split_once('/')
                    .is_some_and(|(owner, model)| owner == provider && !model.is_empty())
            });
            !candidates.is_empty()
        });
        if isolated.models.aliases.is_empty() {
            let candidate = discovery_candidate(provider);
            isolated.models.aliases = BTreeMap::from([("__catalog".to_owned(), vec![candidate])]);
            "__catalog".clone_into(&mut isolated.models.default);
            isolated.models.thinking.clear();
        } else {
            if !isolated
                .models
                .aliases
                .contains_key(&isolated.models.default)
                && let Some(first) = isolated.models.aliases.keys().next()
            {
                isolated.models.default.clone_from(first);
            }
            isolated
                .models
                .thinking
                .retain(|alias, _| isolated.models.aliases.contains_key(alias));
        }
        self.discover_model_catalog(&isolated).await
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_auth(
        &self,
        provider_name: &str,
        provider: &ProviderConfig,
        kind: AdapterKind,
        endpoint: &Url,
        proxy: Option<&Url>,
        proxy_auth: Option<&ProxyAuthentication>,
        redactor: &FixtureRedactor,
        warnings: &RuntimeWarnings,
    ) -> Result<Arc<dyn AuthProvider>, ProviderFactoryError> {
        let explicit_api = provider.api_key_env.is_some() || provider.api_key_credential.is_some();
        let oauth_configured = provider.oauth_token_env.is_some()
            || provider.oauth_authorization_endpoint.is_some()
            || provider.oauth_token_endpoint.is_some()
            || provider.oauth_client_id.is_some()
            || provider.oauth_access_token_credential.is_some()
            || provider.oauth_refresh_token_credential.is_some()
            || !provider.oauth_scopes.is_empty();
        if matches!(provider.auth_scheme, Some(ProviderAuthScheme::None)) {
            if explicit_api || oauth_configured {
                return Err(ProviderFactoryError::new(
                    provider_name,
                    "auth_scheme none cannot be combined with a primary API or OAuth credential",
                ));
            }
            if is_loopback(endpoint) || !provider.header_credentials.is_empty() {
                return Ok(Arc::new(StaticAuth::new(AuthMaterial::None)));
            }
            return Err(ProviderFactoryError::new(
                provider_name,
                "unauthenticated providers require an explicit loopback endpoint",
            ));
        }
        if explicit_api && oauth_configured {
            return Err(ProviderFactoryError::new(
                provider_name,
                "API-key and OAuth authentication are both configured",
            ));
        }
        if oauth_configured {
            if matches!(
                provider.auth_scheme,
                Some(ProviderAuthScheme::Header { .. })
            ) {
                return Err(ProviderFactoryError::new(
                    provider_name,
                    "OAuth authentication cannot use a custom primary credential header",
                ));
            }
            return self.resolve_oauth(
                provider_name,
                provider,
                proxy,
                proxy_auth,
                redactor,
                warnings,
            );
        }
        if explicit_api || (kind.has_official_default() && !is_loopback(endpoint)) {
            let mut effective = provider.clone();
            if effective.api_key_env.is_none() {
                effective.api_key_env = kind.default_api_key_environment().map(str::to_owned);
            }
            let reference = provider_api_key_credential_reference(provider_name, &effective)
                .map_err(|error| ProviderFactoryError::new(provider_name, error.to_string()))?;
            let resolved = self.resolve_required(provider_name, &reference)?;
            warnings.extend(resolved.warnings().iter().map(ToString::to_string));
            let secret = resolved.secret().expose_secret().clone();
            let secret = ProviderSecret::new(secret);
            redactor.register_secret(&secret);
            let material = match &provider.auth_scheme {
                Some(ProviderAuthScheme::Bearer) => AuthMaterial::Bearer(secret),
                Some(ProviderAuthScheme::Header { name, value_prefix }) => AuthMaterial::Header {
                    name: name.clone(),
                    value_prefix: value_prefix.clone(),
                    secret,
                },
                Some(ProviderAuthScheme::None) => unreachable!("handled above"),
                None => AuthMaterial::ApiKey(secret),
            };
            return Ok(Arc::new(StaticAuth::new(material)));
        }
        if (provider.base_url.is_some() && is_loopback(endpoint))
            || !provider.header_credentials.is_empty()
        {
            return Ok(Arc::new(StaticAuth::new(AuthMaterial::None)));
        }
        Err(ProviderFactoryError::new(
            provider_name,
            "unauthenticated providers require an explicit loopback endpoint",
        ))
    }

    fn resolve_header_credentials(
        &self,
        provider_name: &str,
        provider: &ProviderConfig,
        redactor: &FixtureRedactor,
        warnings: &RuntimeWarnings,
    ) -> Result<BTreeMap<String, ProviderSecret>, ProviderFactoryError> {
        provider
            .header_credentials
            .iter()
            .map(|(name, credential)| {
                let resolved = self.resolve_required(
                    provider_name,
                    &CredentialReference::new(credential.clone()),
                )?;
                warnings.extend(resolved.warnings().iter().map(ToString::to_string));
                let secret = ProviderSecret::new(resolved.secret().expose_secret().clone());
                redactor.register_secret(&secret);
                Ok((name.clone(), secret))
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_oauth(
        &self,
        provider_name: &str,
        provider: &ProviderConfig,
        proxy: Option<&Url>,
        proxy_auth: Option<&ProxyAuthentication>,
        redactor: &FixtureRedactor,
        warnings: &RuntimeWarnings,
    ) -> Result<Arc<dyn AuthProvider>, ProviderFactoryError> {
        let access_id = provider
            .oauth_access_token_credential
            .clone()
            .unwrap_or_else(|| format!("providers.{provider_name}.oauth.access_token"));
        let refresh_id = provider
            .oauth_refresh_token_credential
            .clone()
            .unwrap_or_else(|| format!("providers.{provider_name}.oauth.refresh_token"));
        let access_reference = if let Some(variable) = &provider.oauth_token_env {
            CredentialReference::new(access_id).with_environment(variable)
        } else {
            CredentialReference::new(access_id)
        };
        let access = self.resolve_optional(provider_name, &access_reference)?;
        if let Some(resolved) = access.as_ref()
            && matches!(
                resolved.source(),
                rw_store::credentials::CredentialSource::Environment(_)
            )
        {
            warnings.extend(resolved.warnings().iter().map(ToString::to_string));
            let secret = resolved.secret().expose_secret().clone();
            let secret = ProviderSecret::new(secret);
            redactor.register_secret(&secret);
            return Ok(Arc::new(StaticAuth::new(AuthMaterial::Bearer(secret))));
        }

        let endpoint = provider.oauth_token_endpoint.as_deref();
        let client_id = provider.oauth_client_id.as_deref();
        if endpoint.is_some() != client_id.is_some() {
            return Err(ProviderFactoryError::new(
                provider_name,
                "OAuth refresh requires both oauth_token_endpoint and oauth_client_id",
            ));
        }
        let refresh_reference = CredentialReference::new(refresh_id.clone());
        let refresh = self.resolve_optional(provider_name, &refresh_reference)?;
        if provider.oauth_refresh_token_credential.is_some()
            && (endpoint.is_none() || client_id.is_none())
        {
            return Err(ProviderFactoryError::new(
                provider_name,
                "oauth_refresh_token_credential requires a token endpoint and client id",
            ));
        }
        if let (Some(endpoint), Some(client_id), Some(refresh)) = (endpoint, client_id, refresh) {
            let token_endpoint = parse_remote_or_loopback_endpoint(provider_name, endpoint)?;
            warnings.extend(refresh.warnings().iter().map(ToString::to_string));
            let refresh_token = refresh.secret().expose_secret().clone();
            let refresh_token = ProviderSecret::new(refresh_token);
            redactor.register_secret(&refresh_token);
            let sink: Arc<dyn RefreshTokenSink> = Arc::new(CredentialRefreshSink {
                manager: Arc::clone(&self.credentials),
                reference: refresh_reference,
                provider: provider_name.to_owned(),
                warnings: warnings.clone(),
            });
            let auth = RefreshingOAuth::with_proxy_and_sink(
                OAuthRefreshConfig {
                    token_endpoint,
                    client_id: client_id.to_owned(),
                    client_secret: None,
                    refresh_token,
                    scope: (!provider.oauth_scopes.is_empty())
                        .then(|| provider.oauth_scopes.join(" ")),
                },
                proxy,
                proxy_auth,
                sink,
            )
            .map_err(|error| ProviderFactoryError::new(provider_name, error.to_string()))?
            .with_secret_registrar(Arc::new(redactor.clone()));
            return Ok(Arc::new(auth));
        }
        if let Some(resolved) = access {
            warnings.extend(resolved.warnings().iter().map(ToString::to_string));
            let secret = resolved.secret().expose_secret().clone();
            let secret = ProviderSecret::new(secret);
            redactor.register_secret(&secret);
            return Ok(Arc::new(StaticAuth::new(AuthMaterial::Bearer(secret))));
        }
        Err(ProviderFactoryError::new(
            provider_name,
            "configured OAuth credentials were not found",
        ))
    }

    fn resolve_proxy_authentication(
        &self,
        provider_name: &str,
        provider: &ProviderConfig,
        global: &rw_types::config::NetworkConfig,
        source: Option<ProxySource>,
        redactor: &FixtureRedactor,
        warnings: &RuntimeWarnings,
    ) -> Result<Option<ProxyAuthentication>, ProviderFactoryError> {
        let (username, credential) = match source {
            Some(ProxySource::Provider) => (
                provider.proxy_username.as_deref(),
                provider.proxy_password_credential.as_deref(),
            ),
            Some(ProxySource::Global) => (
                global.proxy_username.as_deref(),
                global.proxy_password_credential.as_deref(),
            ),
            Some(ProxySource::Environment) | None => (None, None),
        };
        match (username, credential) {
            (None, None) => Ok(None),
            (Some(username), Some(credential)) => {
                let resolved =
                    self.resolve_required(provider_name, &CredentialReference::new(credential))?;
                warnings.extend(resolved.warnings().iter().map(ToString::to_string));
                let password = resolved.secret().expose_secret().clone();
                let password = ProviderSecret::new(password);
                redactor.register_secret(&password);
                Ok(Some(ProxyAuthentication::new(username, password)))
            }
            _ => Err(ProviderFactoryError::new(
                provider_name,
                "proxy authentication requires both username and password credential reference",
            )),
        }
    }

    fn resolve_openai_subscription_auth(
        &self,
        provider_name: &str,
        provider: &ProviderConfig,
        proxy: Option<&Url>,
        proxy_authentication: Option<&ProxyAuthentication>,
        redactor: &FixtureRedactor,
        warnings: &RuntimeWarnings,
    ) -> Result<Arc<dyn AuthProvider>, ProviderFactoryError> {
        if provider.api_key_env.is_some()
            || provider.api_key_credential.is_some()
            || provider.oauth_token_env.is_some()
            || provider.oauth_authorization_endpoint.is_some()
            || provider.oauth_token_endpoint.is_some()
            || provider.oauth_client_id.is_some()
            || !provider.oauth_scopes.is_empty()
            || provider.oauth_access_token_credential.is_some()
            || provider.oauth_refresh_token_credential.is_some()
        {
            return Err(ProviderFactoryError::new(
                provider_name,
                "openai_codex uses only its built-in ChatGPT subscription OAuth credential bundle",
            ));
        }
        let reference = CredentialReference::new(openai_codex_credential_id(provider_name));
        let resolved = self
            .resolve_credential(&reference)
            .map_err(|error| ProviderFactoryError::new(provider_name, error.to_string()))?;
        warnings.extend(resolved.warnings().iter().map(ToString::to_string));
        let bundle =
            OpenAiSubscriptionCredentialBundle::parse(resolved.secret().expose_secret())
                .map_err(|error| ProviderFactoryError::new(provider_name, error.to_string()))?;
        let token_endpoint = Url::parse(OPENAI_SUBSCRIPTION_TOKEN_ENDPOINT).map_err(|_| {
            ProviderFactoryError::new(provider_name, "invalid built-in ChatGPT token endpoint")
        })?;
        let sink: Arc<dyn OpenAiSubscriptionTokenSink> = Arc::new(CredentialSubscriptionSink {
            manager: Arc::clone(&self.credentials),
            reference,
            provider: provider_name.to_owned(),
            refresh_token: std::sync::Mutex::new(bundle.refresh_token().to_owned()),
            warnings: warnings.clone(),
        });
        let auth = OpenAiSubscriptionAuth::with_proxy(
            OpenAiSubscriptionAuthConfig {
                token_endpoint,
                client_id: OPENAI_SUBSCRIPTION_CLIENT_ID.to_owned(),
                access_token: Some(ProviderSecret::new(bundle.access_token())),
                refresh_token: ProviderSecret::new(bundle.refresh_token()),
                account_id: Some(ProviderSecret::new(bundle.account_id())),
                originator: "rottweiler".to_owned(),
                user_agent: format!("rottweiler/{}", env!("CARGO_PKG_VERSION")),
                session_id: random_subscription_session_id(provider_name)?,
            },
            proxy,
            proxy_authentication,
            sink,
            Arc::new(redactor.clone()),
        )
        .map_err(|error| ProviderFactoryError::new(provider_name, error.to_string()))?;
        Ok(Arc::new(auth))
    }

    fn resolve_github_copilot_runtime(
        &self,
        provider_name: &str,
        provider: &ProviderConfig,
        proxy: Option<&Url>,
        proxy_authentication: Option<&ProxyAuthentication>,
        redactor: &FixtureRedactor,
        warnings: &RuntimeWarnings,
    ) -> Result<Arc<GitHubCopilotRuntime>, ProviderFactoryError> {
        if provider.api_key_env.is_some()
            || provider.api_key_credential.is_some()
            || provider.oauth_token_env.is_some()
            || provider.oauth_authorization_endpoint.is_some()
            || provider.oauth_token_endpoint.is_some()
            || provider.oauth_client_id.is_some()
            || !provider.oauth_scopes.is_empty()
            || provider.oauth_access_token_credential.is_some()
            || provider.oauth_refresh_token_credential.is_some()
        {
            return Err(ProviderFactoryError::new(
                provider_name,
                "github_copilot uses only its built-in device-flow credential",
            ));
        }
        let reference = CredentialReference::new(github_copilot_credential_id(provider_name));
        let resolved = self.resolve_required(provider_name, &reference)?;
        warnings.extend(resolved.warnings().iter().map(ToString::to_string));
        let credential = GitHubCopilotCredential::parse(resolved.secret().expose_secret())
            .map_err(|error| ProviderFactoryError::new(provider_name, error.to_string()))?;
        let test_origin = self.github_copilot_test_origins.get(provider_name);
        let expected_client_id = if let Some(test_origin) = test_origin {
            test_origin.oauth_client_id.as_str()
        } else {
            GITHUB_COPILOT_CLIENT_ID
        };
        if credential.oauth_client_id() != expected_client_id {
            return Err(ProviderFactoryError::new(
                provider_name,
                "stored GitHub Copilot credential belongs to a different OAuth client identity",
            ));
        }
        let token = ProviderSecret::new(credential.access_token().to_owned());
        redactor.register_secret(&token);
        let runtime = if let Some(test_origin) = test_origin {
            GitHubCopilotRuntime::with_test_origin(
                token,
                test_origin.origin.clone(),
                self.network_policy,
            )
        } else {
            GitHubCopilotRuntime::new(token, proxy, proxy_authentication, self.network_policy)
        };
        runtime
            .map(Arc::new)
            .map_err(|error| ProviderFactoryError::new(provider_name, error.to_string()))
    }

    fn resolve_required(
        &self,
        provider: &str,
        reference: &CredentialReference,
    ) -> Result<rw_store::credentials::ResolvedCredential, ProviderFactoryError> {
        self.resolve_credential(reference)
            .map_err(|error| ProviderFactoryError::new(provider, error.to_string()))
    }

    fn resolve_optional(
        &self,
        provider: &str,
        reference: &CredentialReference,
    ) -> Result<Option<rw_store::credentials::ResolvedCredential>, ProviderFactoryError> {
        match self.resolve_credential(reference) {
            Ok(value) => Ok(Some(value)),
            Err(
                CredentialError::NotFound { .. }
                | CredentialError::CredentialStoreUnavailable { .. },
            ) => Ok(None),
            Err(error) => Err(ProviderFactoryError::new(provider, error.to_string())),
        }
    }

    fn resolve_credential(
        &self,
        reference: &CredentialReference,
    ) -> Result<rw_store::credentials::ResolvedCredential, CredentialError> {
        self.credentials.resolve_authorized(reference)
    }
}

struct CredentialRefreshSink<E, K> {
    manager: Arc<CredentialManager<E, K>>,
    reference: CredentialReference,
    provider: String,
    warnings: RuntimeWarnings,
}

struct CredentialSubscriptionSink<E, K> {
    manager: Arc<CredentialManager<E, K>>,
    reference: CredentialReference,
    provider: String,
    refresh_token: std::sync::Mutex<String>,
    warnings: RuntimeWarnings,
}

impl<E, K> fmt::Debug for CredentialSubscriptionSink<E, K> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialSubscriptionSink")
            .field("provider", &self.provider)
            .field("credential_reference", &self.reference.identifier())
            .field("refresh_token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl<E, K> OpenAiSubscriptionTokenSink for CredentialSubscriptionSink<E, K>
where
    E: CredentialEnvironment + Send + Sync + 'static,
    K: CredentialStore + Send + Sync + 'static,
{
    async fn persist(
        &self,
        access_token: &ProviderSecret,
        rotated_refresh_token: Option<&ProviderSecret>,
        account_id: &ProviderSecret,
    ) -> Result<(), ProviderError> {
        let current_refresh = self
            .refresh_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let refresh = rotated_refresh_token.map_or_else(
            || current_refresh.clone(),
            |token| token.expose_secret().to_owned(),
        );
        let bundle = OpenAiSubscriptionCredentialBundle::new(
            access_token.expose_secret().to_owned(),
            refresh.clone(),
            account_id.expose_secret().to_owned(),
        );
        let encoded = bundle.encode().map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "could not encode refreshed ChatGPT subscription credentials",
            )
        })?;
        let stored = self
            .manager
            .store(&self.reference, &StoredSecret::new(encoded))
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "could not persist refreshed ChatGPT subscription credentials",
                )
            })?;
        self.warnings
            .extend(stored.warnings().iter().map(ToString::to_string));
        if rotated_refresh_token.is_some() {
            *self
                .refresh_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = refresh;
        }
        Ok(())
    }
}

impl<E, K> fmt::Debug for CredentialRefreshSink<E, K> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialRefreshSink")
            .field("provider", &self.provider)
            .field("credential_reference", &self.reference.identifier())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl<E, K> RefreshTokenSink for CredentialRefreshSink<E, K>
where
    E: CredentialEnvironment + Send + Sync + 'static,
    K: CredentialStore + Send + Sync + 'static,
{
    async fn persist(&self, refresh_token: &ProviderSecret) -> Result<(), ProviderError> {
        let stored = self
            .manager
            .store(
                &self.reference,
                &StoredSecret::new(refresh_token.expose_secret().to_owned()),
            )
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "could not persist the rotated OAuth refresh token",
                )
            })?;
        self.warnings
            .extend(stored.warnings().iter().map(ToString::to_string));
        Ok(())
    }
}

#[derive(Clone, Default)]
struct RuntimeWarnings(Arc<std::sync::RwLock<Vec<String>>>);

impl RuntimeWarnings {
    fn extend(&self, values: impl IntoIterator<Item = String>) {
        let mut warnings = match self.0.write() {
            Ok(warnings) => warnings,
            Err(poisoned) => poisoned.into_inner(),
        };
        for warning in values {
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
        }
    }

    fn snapshot(&self) -> Vec<String> {
        match self.0.read() {
            Ok(warnings) => warnings.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

struct ModelBoundProvider {
    inner: Arc<dyn Provider>,
    name: String,
    expected_model: String,
    capabilities: Capabilities,
    supported_thinking: Vec<ThinkingLevel>,
    defer_capabilities: bool,
}

struct ProviderConnection {
    kind: AdapterKind,
    endpoint: Url,
    auth: Arc<dyn AuthProvider>,
    copilot_runtime: Option<Arc<GitHubCopilotRuntime>>,
    proxy: Option<Url>,
    proxy_authentication: Option<ProxyAuthentication>,
    headers: BTreeMap<String, String>,
    header_credentials: BTreeMap<String, ProviderSecret>,
    extra_body: BTreeMap<String, serde_json::Value>,
    model_ids: BTreeMap<String, String>,
    path_template: Option<String>,
}

struct ActivatedProvider {
    connection: ProviderConnection,
    discovery_provider: Arc<dyn Provider>,
    redactor: FixtureRedactor,
}

trait ProviderActivator: Send + Sync {
    fn activate(&self, provider: &str) -> Result<ActivatedProvider, ProviderFactoryError>;
}

struct FactoryProviderActivator<E, K> {
    factory: ProviderFactory<E, K>,
    config: Config,
}

impl<E, K> ProviderActivator for FactoryProviderActivator<E, K>
where
    E: CredentialEnvironment + Send + Sync + 'static,
    K: CredentialStore + Send + Sync + 'static,
{
    fn activate(&self, provider: &str) -> Result<ActivatedProvider, ProviderFactoryError> {
        if !self.config.providers.contains_key(provider) {
            return Err(ProviderFactoryError::new(
                provider,
                "provider is not configured",
            ));
        }
        let candidate = discovery_candidate(provider);
        let mut isolated = self.config.clone();
        isolated.providers.retain(|name, _| name == provider);
        isolated.models.aliases =
            BTreeMap::from([("__activation".to_owned(), vec![candidate.clone()])]);
        "__activation".clone_into(&mut isolated.models.default);
        isolated.models.thinking.clear();
        let runtime = self.factory.build(&isolated)?;
        let redactor = runtime.redactor.clone();
        let mut connections = runtime
            .connections
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let connection = connections.remove(provider).ok_or_else(|| {
            ProviderFactoryError::new(provider, "provider activation did not create a connection")
        })?;
        let mut discovery_providers = runtime
            .discovery_providers
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let discovery_provider = discovery_providers.remove(provider).ok_or_else(|| {
            ProviderFactoryError::new(
                provider,
                "provider activation did not create a discovery route",
            )
        })?;
        Ok(ActivatedProvider {
            connection,
            discovery_provider,
            redactor,
        })
    }
}

impl fmt::Debug for ModelBoundProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelBoundProvider")
            .field("name", &self.name)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl ModelBoundProvider {
    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        if request.model != self.expected_model {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "request model does not match the model-bound provider",
            ));
        }
        request.validate_tool_choice()?;
        if self.defer_capabilities {
            return Ok(());
        }
        if !self.capabilities.tool_calling && !request.tools.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::Unsupported,
                "selected model is not catalogued as supporting function tools",
            ));
        }
        if !self.capabilities.vision
            && request
                .turns
                .iter()
                .flat_map(|turn| &turn.blocks)
                .any(block_contains_image)
        {
            return Err(ProviderError::new(
                ProviderErrorKind::Unsupported,
                "selected model is not catalogued as supporting image input",
            ));
        }
        if request.thinking != ThinkingLevel::Off
            && !self.supported_thinking.contains(&request.thinking)
        {
            return Err(ProviderError::new(
                ProviderErrorKind::Unsupported,
                "selected model does not advertise the requested reasoning effort",
            ));
        }
        if self
            .capabilities
            .max_output_tokens
            .is_some_and(|limit| u64::from(request.max_output_tokens) > limit)
        {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "requested output tokens exceed the selected model limit",
            ));
        }
        Ok(())
    }
}

fn block_contains_image(block: &rw_types::Block) -> bool {
    match block {
        rw_types::Block::Image { .. } => true,
        rw_types::Block::ToolResult {
            output: rw_types::ToolOutput::Mixed { parts },
            ..
        } => parts
            .iter()
            .any(|part| matches!(part, rw_types::ToolOutputPart::Image { .. })),
        rw_types::Block::Text { .. }
        | rw_types::Block::Thinking { .. }
        | rw_types::Block::ToolCall { .. }
        | rw_types::Block::ToolResult { .. }
        | rw_types::Block::Citation { .. } => false,
    }
}

#[async_trait]
impl Provider for ModelBoundProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn native_web_search_capability(&self) -> NativeWebSearchCapability {
        self.inner.native_web_search_capability()
    }

    async fn model_metadata(&self) -> Result<Option<ProviderModelMetadata>, ProviderError> {
        let discovered = self.inner.model_metadata().await?;
        Ok(self
            .inner
            .cached_model_metadata_for(&self.expected_model)
            .or(discovered))
    }

    async fn discover_models(
        &self,
    ) -> Result<Option<rw_providers::DiscoveredProviderCatalog>, ProviderError> {
        self.inner.discover_models().await
    }

    fn cached_model_metadata(&self) -> Option<ProviderModelMetadata> {
        self.inner.cached_model_metadata_for(&self.expected_model)
    }

    fn cached_model_metadata_for(&self, model: &str) -> Option<ProviderModelMetadata> {
        (model == self.expected_model)
            .then(|| self.inner.cached_model_metadata_for(model))
            .flatten()
    }

    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        self.validate(&request)?;
        let stream = self.inner.stream(request).await?;
        Ok(qualify_bound_message_start(stream, self.name.clone()))
    }

    async fn stream_with_wire_sink(
        &self,
        request: ProviderRequest,
        sink: Arc<dyn WireFrameSink>,
    ) -> Result<BoxEventStream, ProviderError> {
        self.validate(&request)?;
        let stream = self.inner.stream_with_wire_sink(request, sink).await?;
        Ok(qualify_bound_message_start(stream, self.name.clone()))
    }
}

fn qualify_bound_message_start(mut stream: BoxEventStream, candidate: String) -> BoxEventStream {
    Box::pin(async_stream::try_stream! {
        while let Some(event) = stream.next().await {
            match event? {
                rw_providers::ProviderEvent::MessageStart { .. } => {
                    yield rw_providers::ProviderEvent::MessageStart {
                        model: candidate.clone(),
                    };
                }
                event => yield event,
            }
        }
    })
}

impl AdapterKind {
    /// Parses the complete configuration grammar.
    #[must_use]
    pub fn from_config_kind(value: &str) -> Option<Self> {
        if let Some(profile) = BUILTIN_PROVIDER_PROFILES
            .iter()
            .find(|profile| profile.config_kind() == value)
        {
            return Some(profile.adapter_kind());
        }
        match value {
            "openai_chat" => Some(Self::OpenAiChat),
            "openai_compatible_responses" => Some(Self::OpenAiCompatibleResponses),
            "openai_compatible" => Some(Self::OpenAiCompatibleChat),
            _ => None,
        }
    }

    fn parse(provider: &str, value: &str) -> Result<Self, ProviderFactoryError> {
        Self::from_config_kind(value)
            .ok_or_else(|| ProviderFactoryError::new(provider, "unsupported provider adapter kind"))
    }

    #[must_use]
    pub const fn has_official_default(self) -> bool {
        matches!(
            self,
            Self::Anthropic | Self::OpenAiResponses | Self::OpenAiChat
        )
    }

    const fn catalog_namespace(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("anthropic"),
            Self::OpenAiResponses | Self::OpenAiChat | Self::OpenAiSubscription => Some("openai"),
            Self::GitHubCopilot | Self::OpenAiCompatibleResponses | Self::OpenAiCompatibleChat => {
                None
            }
        }
    }

    #[must_use]
    pub const fn default_api_key_environment(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::OpenAiResponses | Self::OpenAiChat => Some("OPENAI_API_KEY"),
            Self::OpenAiSubscription
            | Self::GitHubCopilot
            | Self::OpenAiCompatibleResponses
            | Self::OpenAiCompatibleChat => None,
        }
    }

    #[must_use]
    pub const fn default_endpoint(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some(ANTHROPIC_MESSAGES_ENDPOINT),
            Self::OpenAiResponses => Some(OPENAI_RESPONSES_ENDPOINT),
            Self::OpenAiChat => Some(OPENAI_CHAT_ENDPOINT),
            Self::OpenAiSubscription => Some(OPENAI_SUBSCRIPTION_RESPONSES_ENDPOINT),
            Self::GitHubCopilot => Some(GITHUB_COPILOT_ENDPOINT),
            Self::OpenAiCompatibleResponses | Self::OpenAiCompatibleChat => None,
        }
    }

    #[must_use]
    pub const fn auth_kind(self, oauth_configured: bool) -> ProviderAuthKind {
        match self {
            Self::OpenAiSubscription => ProviderAuthKind::Oauth,
            Self::GitHubCopilot => ProviderAuthKind::DeviceFlow,
            _ if oauth_configured => ProviderAuthKind::Oauth,
            _ => ProviderAuthKind::ApiKey,
        }
    }
}

fn validate_extension_providers(
    registrations: &[(String, Arc<dyn Provider>)],
    built_in: &BTreeMap<String, ProviderConfig>,
) -> Result<BTreeMap<String, Arc<dyn Provider>>, ProviderFactoryError> {
    let mut providers = BTreeMap::<String, Arc<dyn Provider>>::new();
    for (prefix, provider) in registrations {
        if validate_provider_alias_prefix(prefix).is_err() {
            return Err(ProviderFactoryError::new(
                "extensions",
                "extension alias prefixes must be bounded canonical names ending in '/'",
            ));
        }
        if let Some(config) = built_in
            .iter()
            .find_map(|(name, config)| (prefix == &format!("{name}/")).then_some(config))
        {
            let mut pricing_only = config.clone();
            pricing_only.kind.clear();
            pricing_only.pricing.clear();
            if config.kind != "extension" || pricing_only != ProviderConfig::default() {
                return Err(ProviderFactoryError::new(
                    "extensions",
                    format!(
                        "extension alias prefix {prefix:?} collides with a configured provider; pricing-only extension configuration requires kind = \"extension\""
                    ),
                ));
            }
        }
        if let Some(existing) = providers
            .keys()
            .find(|existing| prefix.starts_with(existing.as_str()) || existing.starts_with(prefix))
        {
            return Err(ProviderFactoryError::new(
                "extensions",
                format!(
                    "extension alias prefix {prefix:?} overlaps registered prefix {existing:?}"
                ),
            ));
        }
        providers.insert(prefix.clone(), Arc::clone(provider));
    }
    Ok(providers)
}

fn parse_candidate(candidate: &str) -> Result<(&str, &str), ProviderFactoryError> {
    let Some((provider, model)) = candidate.split_once('/') else {
        return Err(ProviderFactoryError::new(
            "models",
            "model candidates must use provider/model form",
        ));
    };
    if provider.is_empty() || model.is_empty() {
        return Err(ProviderFactoryError::new(
            "models",
            "model candidates must have non-empty provider and model names",
        ));
    }
    if !provider
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        return Err(ProviderFactoryError::new(
            "models",
            "provider names may contain only ASCII letters, digits, '.', '-', and '_'",
        ));
    }
    Ok((provider, model))
}

fn resolve_endpoint(
    provider: &str,
    config: &ProviderConfig,
    kind: AdapterKind,
) -> Result<Url, ProviderFactoryError> {
    let value = config.base_url.as_deref().or(kind.default_endpoint());
    let value = value.ok_or_else(|| {
        ProviderFactoryError::new(
            provider,
            "openai-compatible adapters require an explicit endpoint",
        )
    })?;
    if matches!(
        kind,
        AdapterKind::OpenAiSubscription | AdapterKind::GitHubCopilot
    ) && config.base_url.is_some()
    {
        return Err(ProviderFactoryError::new(
            provider,
            "subscription provider endpoint is fixed and cannot be overridden",
        ));
    }
    let mut endpoint = parse_remote_or_loopback_endpoint(provider, value)?;
    if !config.extra_query.is_empty() {
        let mut query = endpoint.query_pairs_mut();
        for (name, value) in &config.extra_query {
            query.append_pair(name, value);
        }
    }
    Ok(endpoint)
}

fn parse_remote_or_loopback_endpoint(
    provider: &str,
    value: &str,
) -> Result<Url, ProviderFactoryError> {
    let url = Url::parse(value)
        .map_err(|_| ProviderFactoryError::new(provider, "endpoint is not a valid absolute URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (url.scheme() == "http" && !is_loopback(&url))
    {
        return Err(ProviderFactoryError::new(
            provider,
            "endpoint must use HTTPS without credentials, query, or fragment; HTTP is loopback-only",
        ));
    }
    Ok(url)
}

fn parse_optional_proxy(
    provider: &str,
    value: Option<&str>,
) -> Result<Option<Url>, ProviderFactoryError> {
    value
        .map(|value| {
            let url = Url::parse(value).map_err(|_| {
                ProviderFactoryError::new(provider, "proxy is not a valid absolute URL")
            })?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(ProviderFactoryError::new(
                    provider,
                    "proxy must be an HTTP(S) URL without inline credentials, query, or fragment",
                ));
            }
            Ok(url)
        })
        .transpose()
}

fn validate_proxy_auth_fields(
    provider: &str,
    proxy: Option<&str>,
    username: Option<&str>,
    credential: Option<&str>,
) -> Result<(), ProviderFactoryError> {
    match (proxy, username, credential) {
        (None | Some(_), None, None) | (Some(_), Some(_), Some(_)) => Ok(()),
        _ => Err(ProviderFactoryError::new(
            provider,
            "proxy authentication requires an explicit proxy, username, and password credential reference",
        )),
    }
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

fn find_pricing(
    table: &PricingTable,
    provider: &str,
    model: &str,
    namespace: Option<&str>,
) -> (Option<String>, Option<ModelPricing>) {
    if let Some(namespace) = namespace {
        let canonical = format!("{namespace}/{model}");
        return table
            .models
            .get(&canonical)
            .map_or((None, None), |pricing| {
                (Some(canonical), Some(pricing.clone()))
            });
    }
    let local = format!("{provider}/{model}");
    if let Some(pricing) = table.models.get(&local) {
        return (Some(local), Some(pricing.clone()));
    }
    (None, None)
}

fn declared_pricing(config: &Config, provider: &str, model: &str) -> Option<ModelPricing> {
    let pricing = config.providers.get(provider)?.pricing.get(model)?;
    configured_model_pricing(model, pricing)
}

fn configured_model_pricing(
    model: &str,
    pricing: &ProviderModelPricingConfig,
) -> Option<ModelPricing> {
    let rate = |value: &serde_json::Number| {
        // Configuration validation bounds every rate before composition.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        value
            .as_f64()
            .map(|rate| (rate * 1_000_000.0).round() as u64)
    };
    Some(ModelPricing {
        display_name: model.to_owned(),
        max_context_tokens: None,
        max_output_tokens: None,
        supports_tools: false,
        supports_thinking: false,
        supports_vision: false,
        reasoning_efforts: Vec::new(),
        input_per_million_micros_usd: rate(pricing.input_per_million.as_ref()?)?,
        output_per_million_micros_usd: rate(pricing.output_per_million.as_ref()?)?,
        cache_read_per_million_micros_usd: pricing.cache_read_per_million.as_ref().and_then(rate),
        cache_write_per_million_micros_usd: pricing.cache_write_per_million.as_ref().and_then(rate),
        reasoning_per_million_micros_usd: None,
    })
}

fn effective_pricing(
    configured: Option<ModelPricing>,
    discovered: Option<ModelPricing>,
    catalog: Option<ModelPricing>,
) -> (Option<ModelPricing>, Option<ModelPricingSource>) {
    if let Some(pricing) = configured {
        return (Some(pricing), Some(ModelPricingSource::UserConfig));
    }
    if let Some(pricing) = discovered {
        return (Some(pricing), Some(ModelPricingSource::ProviderDiscovered));
    }
    catalog.map_or((None, None), |pricing| {
        (Some(pricing), Some(ModelPricingSource::ModelsDev))
    })
}

fn discovery_candidate(provider: &str) -> String {
    // This is an internal, never-presented binding used only to construct the
    // provider adapter before it calls the live catalog endpoint. It must not
    // seed discovery from configured aliases, a bundled list, or a model file.
    format!("{provider}/catalog-discovery")
}

#[allow(clippy::too_many_lines)]
fn project_model_catalog(
    config: &Config,
    pricing: &PricingTable,
    mut discoveries: Vec<(
        String,
        String,
        bool,
        Result<rw_providers::DiscoveredProviderCatalog, String>,
    )>,
) -> ModelCatalogSnapshot {
    discoveries.sort_by(|left, right| left.0.cmp(&right.0));
    let reverse_aliases = config
        .models
        .aliases
        .iter()
        .flat_map(|(alias, candidates)| {
            candidates
                .iter()
                .map(move |candidate| (candidate.clone(), ModelAlias(alias.clone())))
        })
        .fold(
            BTreeMap::<String, Vec<ModelAlias>>::new(),
            |mut map, (candidate, alias)| {
                map.entry(candidate).or_default().push(alias);
                map
            },
        );
    let current_candidate = config
        .models
        .aliases
        .get(&config.models.default)
        .and_then(|candidates| candidates.first());
    let mut models = BTreeMap::new();
    let mut providers = Vec::new();
    for (provider_name, _candidate, authenticated, discovery) in discoveries {
        let provider = match discovery {
            Ok(catalog) => project_available_provider(
                config,
                pricing,
                &provider_name,
                catalog,
                &reverse_aliases,
                current_candidate,
                &mut models,
            ),
            Err(error) => {
                project_unavailable_provider(config, &provider_name, authenticated, error)
            }
        };
        providers.push(provider);
    }
    for profile in BUILTIN_PROVIDER_PROFILES
        .iter()
        .copied()
        .filter(|profile| profile.setup_exposed())
    {
        let name = profile.canonical_id();
        if !providers.iter().any(|provider| provider.name == name) {
            providers.push(ProviderDescriptor {
                name: name.to_owned(),
                auth_kind: profile.onboarding_auth_kind(),
                next_action: ProviderNextAction::Configure,
                configured: false,
                authenticated: false,
                reachable: false,
                model_count: 0,
                status: Some("provider setup required before authentication".to_owned()),
            });
        }
    }
    providers.sort_by(|left, right| left.name.cmp(&right.name));
    let aliases = config
        .models
        .aliases
        .iter()
        .map(|(alias, candidates)| ModelAliasDescriptor {
            alias: ModelAlias(alias.clone()),
            candidates: candidates.clone(),
            current: alias == &config.models.default,
        })
        .collect();
    bound_catalog_snapshot(ModelCatalogSnapshot {
        aliases,
        models: models.into_values().collect(),
        providers,
        cached: false,
        truncated: false,
    })
}

fn project_available_provider(
    config: &Config,
    pricing: &PricingTable,
    provider_name: &str,
    catalog: rw_providers::DiscoveredProviderCatalog,
    reverse_aliases: &BTreeMap<String, Vec<ModelAlias>>,
    current_candidate: Option<&String>,
    models: &mut BTreeMap<String, ModelDescriptor>,
) -> ProviderDescriptor {
    let count = u32::try_from(catalog.models.len()).unwrap_or(u32::MAX);
    for discovered in catalog.models {
        let id = format!("{provider_name}/{}", discovered.id);
        if id.len() > MAX_CATALOG_ID_BYTES || id.chars().any(char::is_control) {
            continue;
        }
        let enriched = enrich_discovered_capabilities(
            config,
            pricing,
            provider_name,
            &discovered.id,
            discovered.capabilities,
        );
        models.insert(
            id.clone(),
            ModelDescriptor {
                id: id.clone(),
                display_name: bounded_catalog_text(
                    discovered
                        .display_name
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or(discovered.id),
                ),
                provider: provider_name.to_owned(),
                aliases: reverse_aliases.get(&id).cloned().unwrap_or_default(),
                current: current_candidate == Some(&id),
                available: true,
                status: None,
                capabilities: protocol_capabilities(&enriched),
            },
        );
    }
    let auth_kind = provider_auth_kind(config, provider_name);
    ProviderDescriptor {
        name: provider_name.to_owned(),
        auth_kind,
        next_action: ProviderNextAction::SelectModels,
        configured: true,
        authenticated: true,
        reachable: true,
        model_count: count,
        status: None,
    }
}

fn bound_catalog_snapshot(mut snapshot: ModelCatalogSnapshot) -> ModelCatalogSnapshot {
    if snapshot.models.len() > MAX_CATALOG_MODELS {
        snapshot.models.truncate(MAX_CATALOG_MODELS);
        snapshot.truncated = true;
    }
    for provider in &mut snapshot.providers {
        provider.name = bounded_catalog_text(std::mem::take(&mut provider.name));
        if let Some(status) = provider.status.take() {
            provider.status = Some(bounded_catalog_text(status));
        }
    }
    if snapshot.providers.len() > MAX_CATALOG_PROVIDERS {
        snapshot.providers.truncate(MAX_CATALOG_PROVIDERS);
        snapshot.truncated = true;
    }
    if snapshot.aliases.len() > MAX_CATALOG_ALIASES {
        snapshot.aliases.truncate(MAX_CATALOG_ALIASES);
        snapshot.truncated = true;
    }
    for alias in &mut snapshot.aliases {
        alias.alias.0 = bounded_catalog_text(std::mem::take(&mut alias.alias.0));
        if alias.candidates.len() > MAX_CATALOG_ALIAS_CANDIDATES {
            alias.candidates.truncate(MAX_CATALOG_ALIAS_CANDIDATES);
            snapshot.truncated = true;
        }
        for candidate in &mut alias.candidates {
            *candidate = bounded_catalog_text(std::mem::take(candidate));
        }
    }
    loop {
        let encoded_len = serde_json::to_vec(&snapshot).map_or(0, |encoded| encoded.len());
        if encoded_len <= MAX_CATALOG_WIRE_BYTES {
            break;
        }
        let excess = encoded_len.saturating_sub(MAX_CATALOG_WIRE_BYTES);
        if !snapshot.models.is_empty() {
            let remove = snapshot
                .models
                .len()
                .saturating_mul(excess)
                .div_ceil(encoded_len)
                .max(1);
            snapshot
                .models
                .truncate(snapshot.models.len().saturating_sub(remove));
        } else if snapshot.aliases.pop().is_none() && snapshot.providers.pop().is_none() {
            break;
        }
        snapshot.truncated = true;
    }
    snapshot
}

fn bounded_catalog_text(mut value: String) -> String {
    let end = value.floor_char_boundary(MAX_CATALOG_TEXT_BYTES.min(value.len()));
    value.truncate(end);
    value
}

fn project_unavailable_provider(
    config: &Config,
    provider_name: &str,
    authenticated: bool,
    error: String,
) -> ProviderDescriptor {
    ProviderDescriptor {
        auth_kind: provider_auth_kind(config, provider_name),
        next_action: provider_next_action(provider_auth_kind(config, provider_name), authenticated),
        name: provider_name.to_owned(),
        configured: true,
        authenticated,
        reachable: false,
        model_count: 0,
        status: Some(error),
    }
}

fn provider_auth_kind(config: &Config, provider: &str) -> ProviderAuthKind {
    let Some(entry) = config.providers.get(provider) else {
        return ProviderAuthKind::None;
    };
    let oauth_configured = entry.oauth_token_env.is_some()
        || entry.oauth_authorization_endpoint.is_some()
        || entry.oauth_access_token_credential.is_some()
        || entry.oauth_refresh_token_credential.is_some();
    match AdapterKind::parse(provider, &entry.kind) {
        Ok(kind) => kind.auth_kind(oauth_configured),
        Err(_) => ProviderAuthKind::None,
    }
}

fn provider_next_action(auth_kind: ProviderAuthKind, authenticated: bool) -> ProviderNextAction {
    if authenticated {
        ProviderNextAction::SelectModels
    } else {
        match auth_kind {
            ProviderAuthKind::Oauth | ProviderAuthKind::DeviceFlow => {
                ProviderNextAction::Authenticate
            }
            ProviderAuthKind::ApiKey => ProviderNextAction::ApiKeyCli,
            ProviderAuthKind::None => ProviderNextAction::None,
        }
    }
}

fn enrich_discovered_capabilities(
    config: &Config,
    pricing: &PricingTable,
    provider: &str,
    model: &str,
    discovered: Option<Capabilities>,
) -> Capabilities {
    let kind = config
        .providers
        .get(provider)
        .and_then(|entry| AdapterKind::parse(provider, &entry.kind).ok());
    let catalog_pricing =
        kind.and_then(|kind| find_pricing(pricing, provider, model, kind.catalog_namespace()).1);
    let fallback = kind.map_or_else(
        || model_capabilities(AdapterKind::OpenAiCompatibleChat, catalog_pricing.as_ref()),
        |kind| match kind {
            AdapterKind::OpenAiSubscription => {
                subscription_model_capabilities(catalog_pricing.as_ref())
            }
            AdapterKind::GitHubCopilot => github_copilot_capabilities(catalog_pricing.as_ref()),
            _ => model_capabilities(kind, catalog_pricing.as_ref()),
        },
    );
    discovered.map_or(fallback.clone(), |mut live| {
        live.max_context_tokens = live.max_context_tokens.or(fallback.max_context_tokens);
        live.max_output_tokens = live.max_output_tokens.or(fallback.max_output_tokens);
        live
    })
}

fn protocol_capabilities(capabilities: &Capabilities) -> ModelCapabilities {
    ModelCapabilities {
        tool_calling: capabilities.tool_calling,
        vision: capabilities.vision,
        thinking: capabilities.thinking,
        cache_behavior: match capabilities.cache_breakpoints {
            CacheBreakpointSupport::None => ModelCacheBehavior::None,
            CacheBreakpointSupport::Explicit => ModelCacheBehavior::Explicit,
            CacheBreakpointSupport::Automatic => ModelCacheBehavior::ProviderManaged,
        },
        max_context_tokens: capabilities.max_context_tokens,
        max_output_tokens: capabilities.max_output_tokens,
    }
}

const fn provider_discovery_status(error: &ProviderError) -> &'static str {
    match error.kind {
        ProviderErrorKind::Authentication => "provider authentication failed",
        ProviderErrorKind::RateLimited => "provider model discovery was rate limited",
        ProviderErrorKind::Timeout => "provider model discovery timed out",
        ProviderErrorKind::Server => "provider model discovery returned a server error",
        ProviderErrorKind::InvalidRequest => "provider model discovery request was rejected",
        ProviderErrorKind::ContextOverflow => "provider model discovery failed",
        ProviderErrorKind::Protocol => "provider model catalog response was invalid",
        ProviderErrorKind::Network => "provider model discovery network request failed",
        ProviderErrorKind::Cancelled => "provider model discovery was cancelled",
        ProviderErrorKind::ReplayMiss => "provider model discovery is absent from replay",
        ProviderErrorKind::NetworkDisabled => "provider model discovery is disabled by policy",
        ProviderErrorKind::Unsupported => "provider does not support model discovery",
    }
}

fn subscription_model_capabilities(pricing: Option<&ModelPricing>) -> Capabilities {
    Capabilities {
        // The subscription transport is intentionally isolated from ordinary
        // OpenAI model discovery. A refreshable catalog may enrich a known id,
        // but never makes that id selectable or proves subscription access.
        // Tool compatibility is a property of the isolated subscription
        // transport, not something models.dev pricing metadata may revoke.
        tool_calling: true,
        vision: pricing.is_some_and(|value| value.supports_vision),
        thinking: true,
        cache_breakpoints: CacheBreakpointSupport::Automatic,
        max_context_tokens: pricing.and_then(|value| value.max_context_tokens),
        max_output_tokens: pricing.and_then(|value| value.max_output_tokens),
        wire_mode: WireMode::OpenAiResponses,
    }
}

fn github_copilot_capabilities(pricing: Option<&ModelPricing>) -> Capabilities {
    Capabilities {
        // Copilot is a coding-agent route, so tools must reach lazy discovery;
        // the discovered model record remains the authoritative fail-closed gate.
        tool_calling: true,
        vision: false,
        thinking: pricing.is_some_and(|value| !value.reasoning_efforts.is_empty()),
        cache_breakpoints: CacheBreakpointSupport::None,
        max_context_tokens: pricing.and_then(|value| value.max_context_tokens),
        max_output_tokens: pricing.and_then(|value| value.max_output_tokens),
        wire_mode: WireMode::GitHubCopilot,
    }
}

fn random_subscription_session_id(provider: &str) -> Result<String, ProviderFactoryError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| {
        ProviderFactoryError::new(
            provider,
            "operating-system randomness is unavailable for subscription session id",
        )
    })?;
    let mut value = String::with_capacity(35);
    value.push_str("rw-");
    for byte in bytes {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(value)
}

fn model_capabilities(kind: AdapterKind, pricing: Option<&ModelPricing>) -> Capabilities {
    let wire_mode = match kind {
        AdapterKind::Anthropic => WireMode::AnthropicMessages,
        AdapterKind::GitHubCopilot => WireMode::GitHubCopilot,
        AdapterKind::OpenAiResponses
        | AdapterKind::OpenAiSubscription
        | AdapterKind::OpenAiCompatibleResponses => WireMode::OpenAiResponses,
        AdapterKind::OpenAiChat | AdapterKind::OpenAiCompatibleChat => {
            WireMode::OpenAiChatCompletions
        }
    };
    Capabilities {
        tool_calling: pricing.is_some_and(|value| value.supports_tools),
        vision: pricing.is_some_and(|value| value.supports_vision),
        thinking: pricing.is_some_and(|value| {
            value
                .reasoning_efforts
                .iter()
                .any(|effort| *effort != ThinkingLevel::Off)
        }),
        cache_breakpoints: match kind {
            AdapterKind::Anthropic => CacheBreakpointSupport::Explicit,
            AdapterKind::OpenAiResponses
            | AdapterKind::OpenAiChat
            | AdapterKind::OpenAiSubscription => CacheBreakpointSupport::Automatic,
            AdapterKind::GitHubCopilot
            | AdapterKind::OpenAiCompatibleResponses
            | AdapterKind::OpenAiCompatibleChat => CacheBreakpointSupport::None,
        },
        max_context_tokens: pricing.and_then(|value| value.max_context_tokens),
        max_output_tokens: pricing.and_then(|value| value.max_output_tokens),
        wire_mode,
    }
}

fn ai_credit_cost(
    pricing: &ModelPricing,
    usage: rw_providers::TokenUsage,
    micros_usd_per_credit: u64,
) -> Cost {
    let nominal = [
        (usage.input_tokens, pricing.input_per_million_micros_usd),
        (usage.output_tokens, pricing.output_per_million_micros_usd),
        (
            usage.cache_read_tokens,
            pricing
                .cache_read_per_million_micros_usd
                .unwrap_or(pricing.input_per_million_micros_usd),
        ),
        (
            usage.cache_write_tokens,
            pricing
                .cache_write_per_million_micros_usd
                .unwrap_or(pricing.input_per_million_micros_usd),
        ),
        (
            usage.reasoning_tokens,
            pricing
                .reasoning_per_million_micros_usd
                .unwrap_or(pricing.output_per_million_micros_usd),
        ),
    ]
    .into_iter()
    .try_fold(0_u64, |total, (tokens, rate)| {
        let component = u128::from(tokens)
            .checked_mul(u128::from(rate))?
            .checked_add(500_000)?
            / 1_000_000;
        total.checked_add(u64::try_from(component).ok()?)
    });
    let Some(nominal) = nominal else {
        return Cost::Unavailable {
            reason: "AI-credit cost exceeds the supported range".to_owned(),
        };
    };
    let credits = (micros_usd_per_credit > 0)
        .then(|| {
            u128::from(nominal)
                .checked_mul(1_000_000)?
                .checked_add(u128::from(micros_usd_per_credit / 2))?
                .checked_div(u128::from(micros_usd_per_credit))
        })
        .flatten()
        .and_then(|credits| u64::try_from(credits).ok());
    credits.map_or_else(
        || Cost::Unavailable {
            reason: "AI-credit conversion exceeds the supported range".to_owned(),
        },
        |credits_micros| Cost::AiCredits {
            credits_micros,
            nominal_amount_micros: Some(nominal.to_string()),
            currency: Some("USD".to_owned()),
        },
    )
}

fn nominal_cost_micros(pricing: &ModelPricing, usage: rw_providers::TokenUsage) -> Option<u64> {
    [
        (usage.input_tokens, pricing.input_per_million_micros_usd),
        (usage.output_tokens, pricing.output_per_million_micros_usd),
        (
            usage.cache_read_tokens,
            pricing
                .cache_read_per_million_micros_usd
                .unwrap_or(pricing.input_per_million_micros_usd),
        ),
        (
            usage.cache_write_tokens,
            pricing
                .cache_write_per_million_micros_usd
                .unwrap_or(pricing.input_per_million_micros_usd),
        ),
        (
            usage.reasoning_tokens,
            pricing
                .reasoning_per_million_micros_usd
                .unwrap_or(pricing.output_per_million_micros_usd),
        ),
    ]
    .into_iter()
    .try_fold(0_u64, |total, (tokens, rate)| {
        let component = u128::from(tokens)
            .checked_mul(u128::from(rate))?
            .checked_add(500_000)?
            / 1_000_000;
        total.checked_add(u64::try_from(component).ok()?)
    })
}

fn effective_model_metadata(
    model: &ResolvedModel,
    discovered: Option<ProviderModelMetadata>,
) -> ProviderModelMetadata {
    let capabilities = discovered.as_ref().map_or_else(
        || model.capabilities.clone(),
        |value| value.capabilities.clone(),
    );
    if model.accounting == UsageAccounting::SubscriptionQuota {
        return ProviderModelMetadata {
            capabilities,
            pricing: None,
            accounting: model.accounting,
        };
    }
    if matches!(model.accounting, UsageAccounting::AiCredits { .. }) {
        return ProviderModelMetadata {
            capabilities,
            pricing: discovered
                .and_then(|value| value.pricing)
                .or_else(|| model.pricing.clone()),
            accounting: model.accounting,
        };
    }
    let discovered_pricing = discovered.and_then(|value| value.pricing);
    let pricing = if model.pricing_source == Some(ModelPricingSource::UserConfig) {
        model.pricing.clone()
    } else {
        discovered_pricing.or_else(|| model.pricing.clone())
    };
    ProviderModelMetadata {
        capabilities,
        accounting: if pricing.is_some() {
            UsageAccounting::ApiDollars
        } else {
            UsageAccounting::UnpricedApi
        },
        pricing,
    }
}

/// Converts provider-neutral model metadata and normalized usage into a typed cost.
#[must_use]
pub fn cost_from_model_metadata(
    metadata: &ProviderModelMetadata,
    usage: rw_providers::TokenUsage,
) -> Cost {
    match metadata.accounting {
        UsageAccounting::ApiDollars => metadata.pricing.as_ref().map_or_else(
            || Cost::Unavailable {
                reason: "authoritative API pricing is unavailable".to_owned(),
            },
            |pricing| {
                nominal_cost_micros(pricing, usage).map_or_else(
                    || Cost::Unavailable {
                        reason: "API cost exceeds the supported range".to_owned(),
                    },
                    |amount_micros| Cost::Monetary {
                        amount_micros,
                        currency: "USD".to_owned(),
                    },
                )
            },
        ),
        UsageAccounting::AiCredits {
            micros_usd_per_credit,
        } => metadata.pricing.as_ref().map_or_else(
            || Cost::Unavailable {
                reason: "authoritative AI-credit pricing is unavailable".to_owned(),
            },
            |pricing| ai_credit_cost(pricing, usage, micros_usd_per_credit),
        ),
        UsageAccounting::SubscriptionQuota => Cost::SubscriptionQuota {
            used: Some(total_usage_tokens(usage).to_string()),
            unit: Some("tokens".to_owned()),
        },
        UsageAccounting::UnpricedApi => Cost::Unavailable {
            reason: "authoritative API pricing is unavailable".to_owned(),
        },
    }
}

fn total_usage_tokens(usage: rw_providers::TokenUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_write_tokens)
        .saturating_add(usage.reasoning_tokens)
}

#[allow(clippy::too_many_arguments)]
fn construct_adapter(
    candidate: &str,
    connection: &ProviderConnection,
    network_policy: NetworkPolicy,
    capabilities: &Capabilities,
    supported_thinking: &[ThinkingLevel],
    defer_capabilities: bool,
) -> Result<Arc<dyn Provider>, ProviderFactoryError> {
    let kind = connection.kind;
    let result: Result<Arc<dyn Provider>, ProviderError> = match kind {
        AdapterKind::Anthropic => AnthropicProvider::new(AnthropicConfig {
            name: candidate.to_owned(),
            endpoint: connection.endpoint.clone(),
            auth: Arc::clone(&connection.auth),
            proxy: connection.proxy.clone(),
            proxy_authentication: connection.proxy_authentication.clone(),
            network_policy,
            thinking_strategy: supported_thinking
                .iter()
                .any(|effort| *effort != ThinkingLevel::Off)
                .then_some(AnthropicThinkingStrategy::Adaptive),
            max_context_tokens: capabilities.max_context_tokens,
            max_output_tokens: capabilities.max_output_tokens,
        })
        .map(|provider| Arc::new(provider) as Arc<dyn Provider>),
        AdapterKind::GitHubCopilot => connection
            .copilot_runtime
            .clone()
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "GitHub Copilot runtime is missing",
                )
            })
            .and_then(|runtime| {
                GitHubCopilotProvider::new(GitHubCopilotProviderConfig {
                    name: candidate.to_owned(),
                    model_id: candidate
                        .split_once('/')
                        .map_or(candidate, |(_, model)| model)
                        .to_owned(),
                    runtime,
                })
            })
            .map(|provider| Arc::new(provider) as Arc<dyn Provider>),
        AdapterKind::OpenAiResponses
        | AdapterKind::OpenAiSubscription
        | AdapterKind::OpenAiChat
        | AdapterKind::OpenAiCompatibleResponses
        | AdapterKind::OpenAiCompatibleChat => {
            let wire_mode = match kind {
                AdapterKind::OpenAiResponses
                | AdapterKind::OpenAiSubscription
                | AdapterKind::OpenAiCompatibleResponses => OpenAiWireMode::Responses,
                AdapterKind::OpenAiChat | AdapterKind::OpenAiCompatibleChat => {
                    OpenAiWireMode::ChatCompletions
                }
                AdapterKind::Anthropic | AdapterKind::GitHubCopilot => unreachable!(),
            };
            OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
                name: candidate.to_owned(),
                endpoint: connection.endpoint.clone(),
                auth: Arc::clone(&connection.auth),
                proxy: connection.proxy.clone(),
                proxy_authentication: connection.proxy_authentication.clone(),
                network_policy,
                wire_mode,
                chat_request_profile: if matches!(kind, AdapterKind::OpenAiCompatibleChat) {
                    OpenAiChatRequestProfile::Compatible
                } else {
                    OpenAiChatRequestProfile::OpenAi
                },
                // Unknown model capabilities must not become a local denial.
                // This only enables the standard wire representation; an
                // endpoint that does not support it still returns its own
                // authoritative error.
                tool_calling: capabilities.tool_calling || defer_capabilities,
                cache_breakpoints: capabilities.cache_breakpoints,
                supported_reasoning_efforts: supported_thinking.to_vec(),
                supports_vision: capabilities.vision || defer_capabilities,
                max_context_tokens: capabilities.max_context_tokens,
                max_output_tokens: capabilities.max_output_tokens,
                headers: connection.headers.clone(),
                header_credentials: connection.header_credentials.clone(),
                extra_body: connection.extra_body.clone(),
                model_ids: connection.model_ids.clone(),
                path_template: connection.path_template.clone(),
            })
            .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
        }
    };
    result.map_err(|error| ProviderFactoryError::new(candidate, error.to_string()))
}

#[cfg(test)]
mod provider_profile_tests {
    use super::*;

    #[test]
    fn built_in_profiles_own_canonical_setup_metadata() {
        let expected = [
            (
                BuiltinProviderId::Anthropic,
                "anthropic",
                AdapterKind::Anthropic,
                ProviderAuthKind::ApiKey,
            ),
            (
                BuiltinProviderId::OpenAi,
                "openai",
                AdapterKind::OpenAiResponses,
                ProviderAuthKind::ApiKey,
            ),
            (
                BuiltinProviderId::OpenAiCodex,
                "openai_codex",
                AdapterKind::OpenAiSubscription,
                ProviderAuthKind::Oauth,
            ),
            (
                BuiltinProviderId::GitHubCopilot,
                "github_copilot",
                AdapterKind::GitHubCopilot,
                ProviderAuthKind::DeviceFlow,
            ),
        ];

        for (id, canonical_id, adapter_kind, auth_kind) in expected {
            let profile = id.profile();
            assert_eq!(BuiltinProviderId::parse(canonical_id), Some(id));
            assert_eq!(
                BuiltinProviderId::from_config(canonical_id, profile.config_kind()),
                Some(id)
            );
            assert_eq!(profile.id(), id);
            assert_eq!(profile.canonical_id(), canonical_id);
            assert_eq!(profile.config_kind(), canonical_id);
            assert_eq!(profile.adapter_kind(), adapter_kind);
            assert_eq!(profile.onboarding_auth_kind(), auth_kind);
            assert!(profile.setup_exposed());
        }
    }

    #[test]
    fn custom_adapter_kinds_do_not_become_built_in_provider_ids() {
        for (kind, adapter) in [
            ("openai_chat", AdapterKind::OpenAiChat),
            (
                "openai_compatible_responses",
                AdapterKind::OpenAiCompatibleResponses,
            ),
            ("openai_compatible", AdapterKind::OpenAiCompatibleChat),
        ] {
            assert_eq!(AdapterKind::from_config_kind(kind), Some(adapter));
            assert_eq!(BuiltinProviderId::parse(kind), None);
        }
        assert_eq!(BuiltinProviderId::parse("custom"), None);
        assert_eq!(
            BuiltinProviderId::from_config("openai", "openai_chat"),
            None
        );
    }

    #[test]
    fn custom_provider_auth_remains_config_driven() {
        let mut config = Config::default();
        config.providers.insert(
            "company_gateway".to_owned(),
            ProviderConfig {
                kind: "openai_compatible".to_owned(),
                oauth_token_env: Some("COMPANY_GATEWAY_TOKEN".to_owned()),
                ..ProviderConfig::default()
            },
        );

        assert_eq!(
            provider_auth_kind(&config, "company_gateway"),
            ProviderAuthKind::Oauth
        );
        assert_eq!(BuiltinProviderId::parse("company_gateway"), None);
    }
}

#[cfg(test)]
mod native_search_tests {
    #![allow(clippy::expect_used)]

    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct EmptyEnvironment;

    impl CredentialEnvironment for EmptyEnvironment {
        fn get(&self, _name: &str) -> Result<Option<String>, CredentialError> {
            Ok(None)
        }
    }

    #[derive(Clone, Default)]
    struct CountingCredentialStore(Arc<Mutex<(usize, usize)>>);

    impl CredentialStore for CountingCredentialStore {
        fn get(
            &self,
            _identifier: &str,
        ) -> Result<Option<StoredSecret<String>>, rw_store::credentials::CredentialStoreUnavailable>
        {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0 += 1;
            Ok(Some(StoredSecret::new(
                "version = 1\n[credentials]\n'providers.work.api_key' = 'fixture-key'\n".to_owned(),
            )))
        }

        fn get_authorized(
            &self,
            _identifier: &str,
        ) -> Result<Option<StoredSecret<String>>, rw_store::credentials::CredentialStoreUnavailable>
        {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .1 += 1;
            Ok(Some(StoredSecret::new(
                "version = 1\n[credentials]\n'providers.work.api_key' = 'fixture-key'\n".to_owned(),
            )))
        }

        fn set(
            &self,
            _identifier: &str,
            _secret: &StoredSecret<String>,
        ) -> Result<(), rw_store::credentials::CredentialStoreUnavailable> {
            Ok(())
        }
    }

    fn configured_work_provider() -> Config {
        let mut config = Config::default();
        config.providers.insert(
            "work".to_owned(),
            ProviderConfig {
                kind: "openai".to_owned(),
                base_url: Some("http://127.0.0.1:9/v1/responses".to_owned()),
                api_key_credential: Some("providers.work.api_key".to_owned()),
                ..ProviderConfig::default()
            },
        );
        config.models.default = "fast".to_owned();
        config
            .models
            .aliases
            .insert("fast".to_owned(), vec!["work/fixture".to_owned()]);
        config
    }

    fn capability_pricing() -> ModelPricing {
        ModelPricing {
            display_name: "Fixture".to_owned(),
            max_context_tokens: Some(400_000),
            max_output_tokens: Some(128_000),
            supports_tools: true,
            supports_thinking: true,
            supports_vision: true,
            reasoning_efforts: vec![ThinkingLevel::Low, ThinkingLevel::High],
            input_per_million_micros_usd: 1,
            output_per_million_micros_usd: 1,
            cache_read_per_million_micros_usd: None,
            cache_write_per_million_micros_usd: None,
            reasoning_per_million_micros_usd: None,
        }
    }

    #[test]
    fn subscription_and_copilot_pre_discovery_caps_use_catalog_enrichment() {
        let pricing = capability_pricing();
        let subscription = subscription_model_capabilities(Some(&pricing));
        assert_eq!(subscription.max_context_tokens, Some(400_000));
        assert_eq!(subscription.max_output_tokens, Some(128_000));
        assert!(subscription.tool_calling);
        assert!(subscription.thinking);
        assert!(subscription.vision);

        let copilot = github_copilot_capabilities(Some(&pricing));
        assert_eq!(copilot.max_context_tokens, Some(400_000));
        assert_eq!(copilot.max_output_tokens, Some(128_000));
        assert!(copilot.tool_calling);
        assert!(copilot.thinking);
    }

    #[test]
    fn unknown_subscription_caps_remain_explicitly_unbounded() {
        let capabilities = subscription_model_capabilities(None);
        assert_eq!(capabilities.max_context_tokens, None);
        assert_eq!(capabilities.max_output_tokens, None);
        assert!(capabilities.tool_calling);
    }

    #[test]
    fn subscription_tools_do_not_depend_on_pricing_metadata() {
        let mut pricing = capability_pricing();
        pricing.supports_tools = false;
        assert!(subscription_model_capabilities(Some(&pricing)).tool_calling);
    }

    #[test]
    fn live_ids_are_availability_truth_and_pricing_only_enriches() {
        let mut config = Config::default();
        config.providers.insert(
            "work".to_owned(),
            ProviderConfig {
                kind: "openai".to_owned(),
                ..ProviderConfig::default()
            },
        );
        config.models.default = "fast".to_owned();
        config
            .models
            .aliases
            .insert("fast".to_owned(), vec!["work/live-model".to_owned()]);
        let mut pricing = PricingTable::default();
        pricing
            .models
            .insert("openai/live-model".to_owned(), capability_pricing());
        pricing
            .models
            .insert("openai/stale-model".to_owned(), capability_pricing());
        let catalog = project_model_catalog(
            &config,
            &pricing,
            vec![(
                "work".to_owned(),
                "work/live-model".to_owned(),
                true,
                Ok(rw_providers::DiscoveredProviderCatalog {
                    provider: "work/live-model".to_owned(),
                    models: vec![rw_providers::DiscoveredModel {
                        id: "live-model".to_owned(),
                        display_name: Some("Live".to_owned()),
                        description: None,
                        capabilities: None,
                        pricing: None,
                    }],
                }),
            )],
        );
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].id, "work/live-model");
        assert_eq!(
            catalog.models[0].capabilities.max_context_tokens,
            Some(400_000)
        );
        assert!(catalog.models[0].current);
        assert!(catalog.models[0].available);
        assert!(
            catalog
                .models
                .iter()
                .all(|model| !model.id.contains("stale"))
        );
        let provider = catalog
            .providers
            .iter()
            .find(|provider| provider.name == "work")
            .expect("provider");
        assert_eq!(provider.auth_kind, ProviderAuthKind::ApiKey);
        assert_eq!(provider.next_action, ProviderNextAction::SelectModels);
    }

    #[test]
    fn one_provider_failure_remains_visible_without_fabricating_models() {
        let mut config = Config::default();
        config.providers.insert(
            "broken".to_owned(),
            ProviderConfig {
                kind: "anthropic".to_owned(),
                ..ProviderConfig::default()
            },
        );
        config.models.default = "fast".to_owned();
        config
            .models
            .aliases
            .insert("fast".to_owned(), vec!["broken/model".to_owned()]);
        let catalog = project_model_catalog(
            &config,
            &PricingTable::default(),
            vec![(
                "broken".to_owned(),
                "broken/model".to_owned(),
                true,
                Err("network unavailable".to_owned()),
            )],
        );
        assert!(
            catalog.models.is_empty(),
            "configured aliases must never masquerade as a live model catalog"
        );
        assert!(catalog.providers.iter().any(|provider| {
            provider.name == "broken" && !provider.reachable && provider.status.is_some()
        }));
        assert!(catalog.providers.iter().any(|provider| {
            provider.name == "github_copilot"
                && !provider.configured
                && provider.auth_kind == ProviderAuthKind::DeviceFlow
                && provider.next_action == ProviderNextAction::Configure
        }));
    }

    #[test]
    fn file_configured_extension_alias_never_seeds_the_live_catalog() {
        let mut config = Config::default();
        config.providers.clear();
        config.models.default = "fast".to_owned();
        config.models.aliases =
            BTreeMap::from([("fast".to_owned(), vec!["my_plugin/file_model".to_owned()])]);

        let catalog = project_model_catalog(&config, &PricingTable::default(), Vec::new());

        assert!(catalog.models.is_empty());
        assert!(
            catalog
                .providers
                .iter()
                .all(|provider| provider.name != "my_plugin"),
            "an alias file cannot invent a provider or concrete model row"
        );
    }

    #[test]
    fn first_run_placeholder_has_provider_inventory_but_no_model_rows() {
        let mut config = Config::default();
        config.providers.insert(
            "github_copilot".to_owned(),
            ProviderConfig {
                kind: "github_copilot".to_owned(),
                ..ProviderConfig::default()
            },
        );
        config.models.default = "fast".to_owned();
        config.models.aliases = BTreeMap::from([(
            "fast".to_owned(),
            vec!["github_copilot/a-configured-route-is-not-a-catalog".to_owned()],
        )]);

        let catalog = ProviderModelCatalogSource::placeholder(&config);

        assert!(catalog.models.is_empty());
        let provider = catalog
            .providers
            .iter()
            .find(|provider| provider.name == "github_copilot")
            .expect("configured provider row");
        assert!(provider.configured);
        assert!(!provider.authenticated);
        assert!(!provider.reachable);
        assert_eq!(provider.model_count, 0);
    }

    #[test]
    fn rejected_chatgpt_catalog_does_not_claim_reachability() {
        let mut config = Config::default();
        config.providers.clear();
        config.providers.insert(
            "openai_codex".to_owned(),
            ProviderConfig {
                kind: "openai_codex".to_owned(),
                ..ProviderConfig::default()
            },
        );
        config.models.default = "fast".to_owned();
        config.models.aliases = BTreeMap::from([(
            "fast".to_owned(),
            vec!["openai_codex/gpt-5.4-mini".to_owned()],
        )]);
        let catalog = project_model_catalog(
            &config,
            &PricingTable::default(),
            vec![(
                "openai_codex".to_owned(),
                "openai_codex/gpt-5.4-mini".to_owned(),
                true,
                Err("provider model discovery request was rejected".to_owned()),
            )],
        );

        let provider = catalog
            .providers
            .iter()
            .find(|provider| provider.name == "openai_codex")
            .expect("ChatGPT provider");
        assert_eq!(provider.auth_kind, ProviderAuthKind::Oauth);
        assert!(provider.authenticated);
        assert!(!provider.reachable);
        assert_eq!(provider.model_count, 0);
        assert!(
            catalog.models.is_empty(),
            "a rejected live catalog must not expose configured fallback model rows"
        );
        assert!(
            catalog
                .providers
                .iter()
                .any(|provider| { provider.name == "openai" && !provider.configured })
        );
    }

    #[tokio::test]
    async fn production_catalog_uses_one_authorized_vault_read() {
        let credential_store = CountingCredentialStore::default();
        let manager = Arc::new(CredentialManager::with_backends(
            EmptyEnvironment,
            credential_store.clone(),
            std::path::PathBuf::from("/nonexistent/rottweiler-test-credentials.toml"),
        ));
        let factory = ProviderFactory::with_backends(
            manager,
            ProxyEnvironment::default(),
            NetworkPolicy::Allow,
            PricingTable::default(),
        )
        .with_model_discovery_timeout(std::time::Duration::from_millis(5));

        let catalog = factory
            .discover_model_catalog(&configured_work_provider())
            .await
            .expect("catalog failures remain visible rows");
        let calls = *credential_store
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(calls, (0, 1));
        assert!(catalog.providers.iter().any(|provider| {
            provider.name == "work" && provider.authenticated && !provider.reachable
        }));
    }

    #[test]
    fn active_provider_build_uses_authorized_credentials() {
        let credential_store = CountingCredentialStore::default();
        let manager = Arc::new(CredentialManager::with_backends(
            EmptyEnvironment,
            credential_store.clone(),
            std::path::PathBuf::from("/nonexistent/rottweiler-test-credentials.toml"),
        ));
        let factory = ProviderFactory::with_backends(
            manager,
            ProxyEnvironment::default(),
            NetworkPolicy::Allow,
            PricingTable::default(),
        );

        factory
            .build(&configured_work_provider())
            .expect("active provider composition");
        let calls = *credential_store
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(calls, (0, 1));
    }

    #[test]
    fn provider_discovery_status_never_exposes_private_endpoint_text() {
        let error = ProviderError::new(
            ProviderErrorKind::Network,
            "request to https://private.example.invalid/secret failed",
        );
        let status = provider_discovery_status(&error);
        assert_eq!(status, "provider model discovery network request failed");
        assert!(!status.contains("private.example"));
    }

    #[test]
    fn catalog_projection_is_globally_bounded_and_marks_truncation() {
        let mut config = Config::default();
        config.providers.insert(
            "work".to_owned(),
            ProviderConfig {
                kind: "openai".to_owned(),
                ..ProviderConfig::default()
            },
        );
        let models = (0..(MAX_CATALOG_MODELS + 8))
            .map(|index| rw_providers::DiscoveredModel {
                id: format!("model-{index:04}"),
                display_name: Some("x".repeat(MAX_CATALOG_TEXT_BYTES + 10)),
                description: None,
                capabilities: None,
                pricing: None,
            })
            .collect();
        let catalog = project_model_catalog(
            &config,
            &PricingTable::default(),
            vec![(
                "work".to_owned(),
                "work/catalog-discovery".to_owned(),
                true,
                Ok(rw_providers::DiscoveredProviderCatalog {
                    provider: "work".to_owned(),
                    models,
                }),
            )],
        );
        assert!(catalog.truncated);
        assert!(catalog.models.len() <= MAX_CATALOG_MODELS);
        assert!(
            serde_json::to_vec(&catalog)
                .is_ok_and(|encoded| encoded.len() <= MAX_CATALOG_WIRE_BYTES)
        );
        assert!(
            catalog
                .models
                .iter()
                .all(|model| model.display_name.len() <= MAX_CATALOG_TEXT_BYTES)
        );
    }

    struct Candidate {
        name: &'static str,
        fail: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl WebSearcher for Candidate {
        async fn search(
            &self,
            _request: WebSearchRequest,
            _cancellation: CancellationToken,
        ) -> Result<WebSearchResponse, ToolError> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(self.name);
            if self.fail {
                Err(ToolError::Network("candidate failed".to_owned()))
            } else {
                Ok(WebSearchResponse {
                    source: WebSearchSource::ProviderNative,
                    results: Vec::new(),
                })
            }
        }
    }

    #[tokio::test]
    async fn native_search_candidates_fail_over_in_alias_order() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let router = ProviderNativeWebSearchRouter {
            candidates: vec![
                Arc::new(Candidate {
                    name: "primary",
                    fail: true,
                    calls: Arc::clone(&calls),
                }),
                Arc::new(Candidate {
                    name: "fallback",
                    fail: false,
                    calls: Arc::clone(&calls),
                }),
            ],
        };
        router
            .search(
                WebSearchRequest {
                    model_alias: Some("fast".to_owned()),
                    query: "query".to_owned(),
                    max_results: 5,
                    recency_days: None,
                    allowed_domains: Vec::new(),
                },
                CancellationToken::default(),
            )
            .await
            .expect("fallback search");
        assert_eq!(
            calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            ["primary", "fallback"]
        );
    }
}
