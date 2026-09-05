use super::*;

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
pub(super) struct GitHubCopilotTestOrigin {
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

pub(super) struct CredentialRefreshSink<E, K> {
    manager: Arc<CredentialManager<E, K>>,
    reference: CredentialReference,
    provider: String,
    warnings: RuntimeWarnings,
}

pub(super) struct CredentialSubscriptionSink<E, K> {
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
