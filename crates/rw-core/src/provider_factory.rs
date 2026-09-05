//! Production composition boundary for provider adapters and model routing.

mod catalog;
pub use catalog::cost_from_model_metadata;
use catalog::{
    declared_pricing, discovery_candidate, effective_model_metadata, effective_pricing,
    find_pricing, github_copilot_capabilities, model_capabilities, project_model_catalog,
    provider_discovery_status, random_subscription_session_id, subscription_model_capabilities,
};

mod activation;
pub use activation::*;

mod native_search;
pub use native_search::*;

mod runtime;
pub use runtime::*;

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
    async fn settle_effects(&self) -> std::result::Result<(), rw_providers::ProviderError> {
        self.inner.settle_effects().await
    }

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
mod provider_profile_tests;

#[cfg(test)]
mod native_search_tests;
