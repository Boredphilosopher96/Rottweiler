//! Production composition boundary for provider adapters and model routing.

use std::{collections::BTreeMap, fmt, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use rw_providers::{
    AnthropicConfig, AnthropicProvider, AnthropicThinkingStrategy, AuthMaterial, AuthProvider,
    BoxEventStream, CacheBreakpointSupport, Capabilities, FixtureRedactor,
    GITHUB_COPILOT_COMPILED_CLIENT_ID, GitHubCopilotProvider, GitHubCopilotProviderConfig,
    GitHubCopilotRuntime, ModelPricing, NetworkPolicy, OAuthRefreshConfig,
    OPENAI_SUBSCRIPTION_CLIENT_ID, OPENAI_SUBSCRIPTION_RESPONSES_ENDPOINT,
    OPENAI_SUBSCRIPTION_TOKEN_ENDPOINT, OpenAiCompatibleConfig, OpenAiCompatibleProvider,
    OpenAiSubscriptionAuth, OpenAiSubscriptionAuthConfig, OpenAiSubscriptionTokenSink,
    OpenAiWireMode, PricingTable, Provider, ProviderError, ProviderErrorKind,
    ProviderModelMetadata, ProviderRequest, ProviderRouter, ProxyAuthentication, ProxyEnvironment,
    ProxySettings, ProxySource, RefreshTokenSink, RefreshingOAuth, RetryPolicy, RouterError,
    Secret as ProviderSecret, StaticAuth, ThinkingLevel, UsageAccounting, WireFrameSink, WireMode,
};
use rw_store::credentials::{
    CredentialEnvironment, CredentialError, CredentialKeychain, CredentialManager,
    CredentialReference, OsKeychain, Secret as StoredSecret, SystemEnvironment,
};
use rw_types::{
    Cost,
    config::{BudgetConfig, CompactionConfig, Config, ProviderConfig},
};
use thiserror::Error;
use url::{Host, Url};

use crate::admin::provider_api_key_credential_reference;
use crate::copilot_credentials::{GitHubCopilotCredential, github_copilot_credential_id};
use crate::subscription_credentials::{
    OpenAiSubscriptionCredentialBundle, openai_subscription_credential_id,
};

const ANTHROPIC_MESSAGES_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const OPENAI_RESPONSES_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const OPENAI_CHAT_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";
const GITHUB_COPILOT_ENDPOINT: &str = "https://api.githubcopilot.com";

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
    accounting: UsageAccounting,
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

    /// Catalog pricing/capability record, when one was available.
    #[must_use]
    pub const fn pricing(&self) -> Option<&ModelPricing> {
        self.pricing.as_ref()
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
    alias_thinking: BTreeMap<String, ThinkingLevel>,
    alias_candidates: BTreeMap<String, Vec<String>>,
    route_candidates: BTreeMap<String, String>,
    default_alias: String,
    redactor: FixtureRedactor,
    warnings: RuntimeWarnings,
    pricing_table: PricingTable,
    compaction: CompactionConfig,
    budget: BudgetConfig,
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
        if let Some(metadata) = provider
            .model_metadata()
            .await
            .map_err(|error| ProviderFactoryError::new(candidate, error.to_string()))?
        {
            return Ok(metadata);
        }
        let model = self.models.get(candidate).ok_or_else(|| {
            ProviderFactoryError::new(candidate, "model metadata is inconsistent")
        })?;
        Ok(ProviderModelMetadata {
            capabilities: model.capabilities.clone(),
            pricing: model.pricing.clone(),
            accounting: model.accounting,
        })
    }

    /// Known-secret redactor for [`rw_providers::Recorder`].
    #[must_use]
    pub fn fixture_redactor(&self) -> FixtureRedactor {
        self.redactor.clone()
    }

    /// Credential fallback warnings that the active UI must surface.
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
        let candidate = self.alias_candidates.get(alias)?.first()?;
        self.providers
            .get(candidate)
            .and_then(|provider| provider.cached_model_metadata())
            .map(|metadata| metadata.capabilities)
            .or_else(|| {
                self.models
                    .get(candidate)
                    .map(|model| model.capabilities.clone())
            })
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
        let Some(model) = self.models.get(candidate) else {
            return Cost::Unavailable {
                reason: "model candidate accounting is unavailable".to_owned(),
            };
        };
        let cached_metadata = self
            .providers
            .get(candidate)
            .and_then(|provider| provider.cached_model_metadata());
        let accounting = cached_metadata
            .as_ref()
            .map_or(model.accounting, |metadata| metadata.accounting);
        match accounting {
            UsageAccounting::ApiDollars => model
                .catalog_model
                .as_deref()
                .and_then(|canonical| self.pricing_table.cost(canonical, usage).ok().flatten())
                .map_or_else(
                    || Cost::Unavailable {
                        reason: "authoritative API pricing is unavailable".to_owned(),
                    },
                    |cost| Cost::Monetary {
                        amount_micros: cost.total_micros_usd,
                        currency: "USD".to_owned(),
                    },
                ),
            UsageAccounting::AiCredits {
                micros_usd_per_credit,
            } => cached_metadata
                .as_ref()
                .and_then(|metadata| metadata.pricing.as_ref())
                .or(model.pricing.as_ref())
                .map_or_else(
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
        if let Some(thinking) = self.alias_thinking.get(alias) {
            request.thinking = *thinking;
        }
        self.router.stream_alias(alias, request)
    }
}

/// Injectable production provider-composition boundary.
pub struct ProviderFactory<E = SystemEnvironment, K = OsKeychain> {
    credentials: Arc<CredentialManager<E, K>>,
    proxy_environment: ProxyEnvironment,
    network_policy: NetworkPolicy,
    pricing: PricingTable,
    retry: RetryPolicy,
    github_copilot_test_origins: BTreeMap<String, GitHubCopilotTestOrigin>,
}

#[derive(Clone)]
struct GitHubCopilotTestOrigin {
    origin: Url,
    oauth_client_id: String,
}

impl ProviderFactory<SystemEnvironment, OsKeychain> {
    /// Creates a production factory using process environment and OS keychain.
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
    K: CredentialKeychain + Send + Sync + 'static,
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
        }
    }

    /// Replaces the bounded router retry policy.
    #[must_use]
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
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
        validate_proxy_auth_fields(
            "network",
            config.network.proxy.as_deref(),
            config.network.proxy_username.as_deref(),
            config.network.proxy_password_credential.as_deref(),
        )?;
        for (name, provider) in &config.providers {
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
        // racing refresh-token rotation or duplicating token exchanges.
        let mut connections = BTreeMap::new();
        for (provider_name, _) in unique_candidates.values() {
            if connections.contains_key(provider_name) {
                continue;
            }
            let provider_config = config.providers.get(provider_name).ok_or_else(|| {
                ProviderFactoryError::new(
                    provider_name,
                    "model candidate references an unconfigured provider",
                )
            })?;
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
            connections.insert(
                provider_name.clone(),
                ProviderConnection {
                    kind,
                    endpoint,
                    auth,
                    copilot_runtime,
                    proxy: proxy.map(|value| value.url),
                    proxy_authentication,
                },
            );
        }

        let mut route_candidates = BTreeMap::new();
        for (index, (candidate, (provider_name, model))) in unique_candidates.iter().enumerate() {
            let connection = connections.get(provider_name).ok_or_else(|| {
                ProviderFactoryError::new(provider_name, "provider connection is inconsistent")
            })?;
            let kind = connection.kind;
            let (catalog_model, pricing) = if kind == AdapterKind::GitHubCopilot {
                (None, None)
            } else {
                find_pricing(
                    &self.pricing,
                    provider_name,
                    model,
                    kind.catalog_namespace(),
                )
            };
            if kind == AdapterKind::OpenAiSubscription && !subscription_model_allowed(model) {
                return Err(ProviderFactoryError::new(
                    provider_name,
                    "model is not in the conservative ChatGPT subscription allowlist",
                ));
            }
            let supported_thinking = if kind == AdapterKind::OpenAiSubscription {
                vec![
                    ThinkingLevel::Off,
                    ThinkingLevel::Low,
                    ThinkingLevel::Medium,
                    ThinkingLevel::High,
                ]
            } else {
                pricing
                    .as_ref()
                    .map_or_else(Vec::new, |value| value.reasoning_efforts.clone())
            };
            let capabilities = match kind {
                AdapterKind::OpenAiSubscription => subscription_model_capabilities(),
                AdapterKind::GitHubCopilot => github_copilot_capabilities(),
                _ => model_capabilities(kind, pricing.as_ref()),
            };
            let accounting = match kind {
                AdapterKind::OpenAiSubscription => UsageAccounting::SubscriptionQuota,
                AdapterKind::GitHubCopilot => UsageAccounting::AiCredits {
                    micros_usd_per_credit: 10_000,
                },
                _ if pricing.is_some() => UsageAccounting::ApiDollars,
                _ => UsageAccounting::UnpricedApi,
            };
            let inner = construct_adapter(
                candidate,
                kind,
                connection.endpoint.clone(),
                Arc::clone(&connection.auth),
                connection.copilot_runtime.clone(),
                connection.proxy.clone(),
                connection.proxy_authentication.clone(),
                self.network_policy,
                &capabilities,
                &supported_thinking,
            )?;
            let bounded: Arc<dyn Provider> = Arc::new(ModelBoundProvider {
                inner,
                expected_model: model.clone(),
                capabilities: capabilities.clone(),
                supported_thinking,
                defer_capabilities: kind == AdapterKind::GitHubCopilot,
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
                    accounting,
                },
            );
        }

        for (alias, candidates) in &config.models.aliases {
            let routed = candidates
                .iter()
                .map(|candidate| {
                    let registration = registration_keys.get(candidate).ok_or_else(|| {
                        ProviderFactoryError::new("models", "candidate registry is inconsistent")
                    })?;
                    let model = &unique_candidates[candidate].1;
                    Ok(format!("{registration}/{model}"))
                })
                .collect::<Result<Vec<_>, ProviderFactoryError>>()?;
            router_aliases.insert(alias.clone(), routed);
        }
        let router = ProviderRouter::with_registry(router_aliases, registry, self.retry.clone())
            .map_err(|error| ProviderFactoryError::new("models", error.to_string()))?;
        let alias_thinking = config
            .models
            .thinking
            .iter()
            .map(|(alias, level)| (alias.clone(), convert_thinking(*level)))
            .collect();
        let alias_candidates = config.models.aliases.clone();
        Ok(ProviderRuntime {
            router,
            providers,
            models,
            alias_thinking,
            alias_candidates,
            route_candidates,
            default_alias: config.models.default.clone(),
            redactor,
            warnings,
            pricing_table: self.pricing.clone(),
            compaction: config.compaction.clone(),
            budget: config.budget.clone(),
        })
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
        if explicit_api && oauth_configured {
            return Err(ProviderFactoryError::new(
                provider_name,
                "API-key and OAuth authentication are both configured",
            ));
        }
        if oauth_configured {
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
            return Ok(Arc::new(StaticAuth::new(AuthMaterial::ApiKey(secret))));
        }
        if provider.base_url.is_some() && is_loopback(endpoint) {
            return Ok(Arc::new(StaticAuth::new(AuthMaterial::None)));
        }
        Err(ProviderFactoryError::new(
            provider_name,
            "unauthenticated providers require an explicit loopback endpoint",
        ))
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
        let reference = CredentialReference::new(openai_subscription_credential_id(provider_name));
        let resolved = self.resolve_required(provider_name, &reference)?;
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
            GITHUB_COPILOT_COMPILED_CLIENT_ID.ok_or_else(|| {
                ProviderFactoryError::new(
                    provider_name,
                    "this build has no Rottweiler GitHub Copilot OAuth client identity",
                )
            })?
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
        self.credentials
            .resolve(reference)
            .map_err(|error| ProviderFactoryError::new(provider, error.to_string()))
    }

    fn resolve_optional(
        &self,
        provider: &str,
        reference: &CredentialReference,
    ) -> Result<Option<rw_store::credentials::ResolvedCredential>, ProviderFactoryError> {
        match self.credentials.resolve(reference) {
            Ok(value) => Ok(Some(value)),
            Err(CredentialError::NotFound { .. } | CredentialError::KeychainUnavailable { .. }) => {
                Ok(None)
            }
            Err(error) => Err(ProviderFactoryError::new(provider, error.to_string())),
        }
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
    K: CredentialKeychain + Send + Sync + 'static,
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
    K: CredentialKeychain + Send + Sync + 'static,
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
}

impl fmt::Debug for ModelBoundProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelBoundProvider")
            .field("name", &self.inner.name())
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
        self.inner.name()
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn model_metadata(&self) -> Result<Option<ProviderModelMetadata>, ProviderError> {
        self.inner.model_metadata().await
    }

    fn cached_model_metadata(&self) -> Option<ProviderModelMetadata> {
        self.inner.cached_model_metadata()
    }

    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        self.validate(&request)?;
        self.inner.stream(request).await
    }

    async fn stream_with_wire_sink(
        &self,
        request: ProviderRequest,
        sink: Arc<dyn WireFrameSink>,
    ) -> Result<BoxEventStream, ProviderError> {
        self.validate(&request)?;
        self.inner.stream_with_wire_sink(request, sink).await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdapterKind {
    Anthropic,
    OpenAiResponses,
    OpenAiChat,
    OpenAiSubscription,
    GitHubCopilot,
    OpenAiCompatibleResponses,
    OpenAiCompatibleChat,
}

impl AdapterKind {
    fn parse(provider: &str, value: &str) -> Result<Self, ProviderFactoryError> {
        match value {
            "anthropic" => Ok(Self::Anthropic),
            "openai" | "openai_responses" => Ok(Self::OpenAiResponses),
            "openai_chat" => Ok(Self::OpenAiChat),
            "openai_codex" | "openai_subscription" => Ok(Self::OpenAiSubscription),
            "github_copilot" => Ok(Self::GitHubCopilot),
            "openai_compatible_responses" => Ok(Self::OpenAiCompatibleResponses),
            "openai_compatible" | "openai_compatible_chat" => Ok(Self::OpenAiCompatibleChat),
            _ => Err(ProviderFactoryError::new(
                provider,
                "unsupported adapter kind; expected anthropic, github_copilot, openai, openai_chat, openai_codex, openai_compatible, or openai_compatible_responses",
            )),
        }
    }

    const fn has_official_default(self) -> bool {
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

    const fn default_api_key_environment(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::OpenAiResponses | Self::OpenAiChat => Some("OPENAI_API_KEY"),
            Self::OpenAiSubscription
            | Self::GitHubCopilot
            | Self::OpenAiCompatibleResponses
            | Self::OpenAiCompatibleChat => None,
        }
    }
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
    let value = config.base_url.as_deref().or(match kind {
        AdapterKind::Anthropic => Some(ANTHROPIC_MESSAGES_ENDPOINT),
        AdapterKind::OpenAiResponses => Some(OPENAI_RESPONSES_ENDPOINT),
        AdapterKind::OpenAiChat => Some(OPENAI_CHAT_ENDPOINT),
        AdapterKind::OpenAiSubscription => Some(OPENAI_SUBSCRIPTION_RESPONSES_ENDPOINT),
        AdapterKind::GitHubCopilot => Some(GITHUB_COPILOT_ENDPOINT),
        AdapterKind::OpenAiCompatibleResponses | AdapterKind::OpenAiCompatibleChat => None,
    });
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
    parse_remote_or_loopback_endpoint(provider, value)
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

fn subscription_model_allowed(model: &str) -> bool {
    matches!(
        model,
        "gpt-5.5" | "gpt-5.3-codex-spark" | "gpt-5.4" | "gpt-5.4-mini"
    )
}

fn subscription_model_capabilities() -> Capabilities {
    Capabilities {
        tool_calling: true,
        vision: false,
        thinking: true,
        cache_breakpoints: CacheBreakpointSupport::Automatic,
        max_context_tokens: None,
        max_output_tokens: None,
        wire_mode: WireMode::OpenAiResponses,
    }
}

fn github_copilot_capabilities() -> Capabilities {
    Capabilities {
        // Copilot is a coding-agent route, so tools must reach lazy discovery;
        // the discovered model record remains the authoritative fail-closed gate.
        tool_calling: true,
        vision: false,
        thinking: false,
        cache_breakpoints: CacheBreakpointSupport::None,
        max_context_tokens: None,
        max_output_tokens: None,
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
        // The current catalog has no authoritative per-model vision field.
        vision: false,
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
    kind: AdapterKind,
    endpoint: Url,
    auth: Arc<dyn AuthProvider>,
    copilot_runtime: Option<Arc<GitHubCopilotRuntime>>,
    proxy: Option<Url>,
    proxy_authentication: Option<ProxyAuthentication>,
    network_policy: NetworkPolicy,
    capabilities: &Capabilities,
    supported_thinking: &[ThinkingLevel],
) -> Result<Arc<dyn Provider>, ProviderFactoryError> {
    let result: Result<Arc<dyn Provider>, ProviderError> = match kind {
        AdapterKind::Anthropic => AnthropicProvider::new(AnthropicConfig {
            name: candidate.to_owned(),
            endpoint,
            auth,
            proxy,
            proxy_authentication,
            network_policy,
            thinking_strategy: supported_thinking
                .iter()
                .any(|effort| *effort != ThinkingLevel::Off)
                .then_some(AnthropicThinkingStrategy::Adaptive),
            max_context_tokens: capabilities.max_context_tokens,
            max_output_tokens: capabilities.max_output_tokens,
        })
        .map(|provider| Arc::new(provider) as Arc<dyn Provider>),
        AdapterKind::GitHubCopilot => copilot_runtime
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
                endpoint,
                auth,
                proxy,
                proxy_authentication,
                network_policy,
                wire_mode,
                tool_calling: capabilities.tool_calling,
                cache_breakpoints: capabilities.cache_breakpoints,
                supported_reasoning_efforts: supported_thinking.to_vec(),
                supports_vision: capabilities.vision,
                max_context_tokens: capabilities.max_context_tokens,
                max_output_tokens: capabilities.max_output_tokens,
            })
            .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
        }
    };
    result.map_err(|error| ProviderFactoryError::new(candidate, error.to_string()))
}

const fn convert_thinking(level: rw_types::config::ThinkingLevel) -> ThinkingLevel {
    match level {
        rw_types::config::ThinkingLevel::Off => ThinkingLevel::Off,
        rw_types::config::ThinkingLevel::Low => ThinkingLevel::Low,
        rw_types::config::ThinkingLevel::Medium => ThinkingLevel::Medium,
        rw_types::config::ThinkingLevel::High => ThinkingLevel::High,
    }
}
