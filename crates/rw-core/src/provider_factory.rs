//! Production composition boundary for provider adapters and model routing.

use std::{collections::BTreeMap, fmt, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use rw_providers::{
    AnthropicConfig, AnthropicProvider, AnthropicThinkingStrategy, AuthMaterial, AuthProvider,
    BoxEventStream, CacheBreakpointSupport, Capabilities, FixtureRedactor, ModelPricing,
    NetworkPolicy, OAuthRefreshConfig, OpenAiCompatibleConfig, OpenAiCompatibleProvider,
    OpenAiWireMode, PricingTable, Provider, ProviderError, ProviderErrorKind, ProviderRequest,
    ProviderRouter, ProxyAuthentication, ProxyEnvironment, ProxySettings, ProxySource,
    RefreshTokenSink, RefreshingOAuth, RetryPolicy, RouterError, Secret as ProviderSecret,
    StaticAuth, ThinkingLevel, WireFrameSink, WireMode,
};
use rw_store::credentials::{
    CredentialEnvironment, CredentialError, CredentialKeychain, CredentialManager,
    CredentialReference, OsKeychain, Secret as StoredSecret, SystemEnvironment,
};
use rw_types::config::{Config, ProviderConfig};
use thiserror::Error;
use url::{Host, Url};

use crate::admin::provider_api_key_credential_reference;

const ANTHROPIC_MESSAGES_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const OPENAI_RESPONSES_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const OPENAI_CHAT_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";

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
}

/// Fully composed provider registry and provider-blind model router.
pub struct ProviderRuntime {
    router: ProviderRouter,
    providers: BTreeMap<String, Arc<dyn Provider>>,
    models: BTreeMap<String, ResolvedModel>,
    alias_thinking: BTreeMap<String, ThinkingLevel>,
    default_alias: String,
    redactor: FixtureRedactor,
    warnings: RuntimeWarnings,
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
        }
    }

    /// Replaces the bounded router retry policy.
    #[must_use]
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
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
            let auth = self.resolve_auth(
                provider_name,
                provider_config,
                kind,
                &endpoint,
                proxy.as_ref().map(|value| &value.url),
                proxy_authentication.as_ref(),
                &redactor,
                &warnings,
            )?;
            connections.insert(
                provider_name.clone(),
                ProviderConnection {
                    kind,
                    endpoint,
                    auth,
                    proxy: proxy.map(|value| value.url),
                    proxy_authentication,
                },
            );
        }

        for (index, (candidate, (provider_name, model))) in unique_candidates.iter().enumerate() {
            let connection = connections.get(provider_name).ok_or_else(|| {
                ProviderFactoryError::new(provider_name, "provider connection is inconsistent")
            })?;
            let kind = connection.kind;
            let (catalog_model, pricing) = find_pricing(
                &self.pricing,
                provider_name,
                model,
                kind.catalog_namespace(),
            );
            let capabilities = model_capabilities(kind, pricing.as_ref());
            let inner = construct_adapter(
                candidate,
                kind,
                connection.endpoint.clone(),
                Arc::clone(&connection.auth),
                connection.proxy.clone(),
                connection.proxy_authentication.clone(),
                self.network_policy,
                &capabilities,
                pricing.as_ref(),
            )?;
            let bounded: Arc<dyn Provider> = Arc::new(ModelBoundProvider {
                inner,
                expected_model: model.clone(),
                capabilities: capabilities.clone(),
                supported_thinking: pricing
                    .as_ref()
                    .map_or_else(Vec::new, |value| value.reasoning_efforts.clone()),
            });
            let registration_key = format!("__model_{index:08}");
            registration_keys.insert(candidate.clone(), registration_key.clone());
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
                    pricing,
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
        Ok(ProviderRuntime {
            router,
            providers,
            models,
            alias_thinking,
            default_alias: config.models.default.clone(),
            redactor,
            warnings,
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
}

struct ProviderConnection {
    kind: AdapterKind,
    endpoint: Url,
    auth: Arc<dyn AuthProvider>,
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
    OpenAiCompatibleResponses,
    OpenAiCompatibleChat,
}

impl AdapterKind {
    fn parse(provider: &str, value: &str) -> Result<Self, ProviderFactoryError> {
        match value {
            "anthropic" => Ok(Self::Anthropic),
            "openai" | "openai_responses" => Ok(Self::OpenAiResponses),
            "openai_chat" => Ok(Self::OpenAiChat),
            "openai_compatible_responses" => Ok(Self::OpenAiCompatibleResponses),
            "openai_compatible" | "openai_compatible_chat" => Ok(Self::OpenAiCompatibleChat),
            _ => Err(ProviderFactoryError::new(
                provider,
                "unsupported adapter kind; expected anthropic, openai, openai_chat, openai_compatible, or openai_compatible_responses",
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
            Self::OpenAiResponses | Self::OpenAiChat => Some("openai"),
            Self::OpenAiCompatibleResponses | Self::OpenAiCompatibleChat => None,
        }
    }

    const fn default_api_key_environment(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::OpenAiResponses | Self::OpenAiChat => Some("OPENAI_API_KEY"),
            Self::OpenAiCompatibleResponses | Self::OpenAiCompatibleChat => None,
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
        AdapterKind::OpenAiCompatibleResponses | AdapterKind::OpenAiCompatibleChat => None,
    });
    let value = value.ok_or_else(|| {
        ProviderFactoryError::new(
            provider,
            "openai-compatible adapters require an explicit endpoint",
        )
    })?;
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

fn model_capabilities(kind: AdapterKind, pricing: Option<&ModelPricing>) -> Capabilities {
    let wire_mode = match kind {
        AdapterKind::Anthropic => WireMode::AnthropicMessages,
        AdapterKind::OpenAiResponses | AdapterKind::OpenAiCompatibleResponses => {
            WireMode::OpenAiResponses
        }
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
        // Price fields alone do not prove a cache-control protocol.
        cache_breakpoints: CacheBreakpointSupport::None,
        max_context_tokens: pricing.and_then(|value| value.max_context_tokens),
        max_output_tokens: pricing.and_then(|value| value.max_output_tokens),
        wire_mode,
    }
}

#[allow(clippy::too_many_arguments)]
fn construct_adapter(
    candidate: &str,
    kind: AdapterKind,
    endpoint: Url,
    auth: Arc<dyn AuthProvider>,
    proxy: Option<Url>,
    proxy_authentication: Option<ProxyAuthentication>,
    network_policy: NetworkPolicy,
    capabilities: &Capabilities,
    pricing: Option<&ModelPricing>,
) -> Result<Arc<dyn Provider>, ProviderFactoryError> {
    let result: Result<Arc<dyn Provider>, ProviderError> = match kind {
        AdapterKind::Anthropic => AnthropicProvider::new(AnthropicConfig {
            name: candidate.to_owned(),
            endpoint,
            auth,
            proxy,
            proxy_authentication,
            network_policy,
            thinking_strategy: pricing
                .filter(|value| {
                    value
                        .reasoning_efforts
                        .iter()
                        .any(|effort| *effort != ThinkingLevel::Off)
                })
                .map(|_| AnthropicThinkingStrategy::Adaptive),
            max_context_tokens: capabilities.max_context_tokens,
            max_output_tokens: capabilities.max_output_tokens,
        })
        .map(|provider| Arc::new(provider) as Arc<dyn Provider>),
        AdapterKind::OpenAiResponses
        | AdapterKind::OpenAiChat
        | AdapterKind::OpenAiCompatibleResponses
        | AdapterKind::OpenAiCompatibleChat => {
            let wire_mode = match kind {
                AdapterKind::OpenAiResponses | AdapterKind::OpenAiCompatibleResponses => {
                    OpenAiWireMode::Responses
                }
                AdapterKind::OpenAiChat | AdapterKind::OpenAiCompatibleChat => {
                    OpenAiWireMode::ChatCompletions
                }
                AdapterKind::Anthropic => unreachable!(),
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
                supported_reasoning_efforts: pricing
                    .map_or_else(Vec::new, |value| value.reasoning_efforts.clone()),
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
