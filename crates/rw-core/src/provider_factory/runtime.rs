use super::*;

/// Fully composed provider registry and provider-blind model router.
pub struct ProviderRuntime {
    pub(super) router: Arc<ProviderRouter>,
    pub(super) providers: BTreeMap<String, Arc<dyn Provider>>,
    pub(super) models: BTreeMap<String, ResolvedModel>,
    pub(super) dynamic_providers: std::sync::RwLock<BTreeMap<String, Arc<dyn Provider>>>,
    pub(super) dynamic_models: std::sync::RwLock<BTreeMap<String, ResolvedModel>>,
    pub(super) connections: std::sync::RwLock<BTreeMap<String, ProviderConnection>>,
    pub(super) discovery_providers: std::sync::RwLock<BTreeMap<String, Arc<dyn Provider>>>,
    pub(super) extension_providers: BTreeMap<String, Arc<dyn Provider>>,
    pub(super) provider_activator: Arc<dyn ProviderActivator>,
    pub(super) network_policy: NetworkPolicy,
    pub(super) model_discovery_timeout: std::time::Duration,
    pub(super) alias_thinking: BTreeMap<String, ThinkingLevel>,
    pub(super) alias_candidates: BTreeMap<String, Vec<String>>,
    pub(super) validated_alias_routes: std::sync::RwLock<BTreeMap<String, Vec<ModelCandidate>>>,
    pub(super) validated_concrete_models: std::sync::RwLock<std::collections::BTreeSet<String>>,
    pub(super) route_candidates: BTreeMap<String, String>,
    pub(super) default_alias: String,
    pub(super) redactor: FixtureRedactor,
    pub(super) warnings: RuntimeWarnings,
    pub(super) pricing_table: PricingTable,
    pub(super) config: Config,
    pub(super) compaction: CompactionConfig,
    pub(super) budget: BudgetConfig,
}

pub(super) enum CatalogAuthority {
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
    /// Drains abandoned local provider ownership retained by the router.
    ///
    /// # Errors
    /// Returns failed provider or accounting settlement proof.
    pub async fn settle_provider_effects(&self) -> Result<(), ProviderError> {
        self.router.settle_effects().await
    }

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

    /// Inert native search routes; execution requires a session accounting binding.
    #[must_use]
    pub fn native_web_search_factory(&self, alias: &str) -> Option<ProviderNativeWebSearchFactory> {
        let candidates = self
            .router
            .resolve(alias)
            .ok()?
            .iter()
            .filter(|route| {
                self.route_candidates
                    .get(&route.provider)
                    .and_then(|candidate| self.providers.get(candidate))
                    .is_some_and(|provider| {
                        provider.native_web_search_capability()
                            == NativeWebSearchCapability::Supported
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        if candidates.is_empty() || candidates.len() > 64 {
            return None;
        }
        let metadata = candidates
            .iter()
            .filter_map(|route| {
                let candidate = self.route_candidates.get(&route.provider)?;
                self.accounting_metadata(candidate)
                    .map(|metadata| (route.clone(), metadata))
            })
            .collect();
        Some(ProviderNativeWebSearchFactory {
            router: Arc::clone(&self.router),
            alias: alias.to_owned(),
            candidates,
            metadata,
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
        self.accounting_metadata(candidate).map_or_else(
            || Cost::Unavailable {
                reason: "model candidate accounting is unavailable".to_owned(),
            },
            |metadata| cost_from_model_metadata(&metadata, usage),
        )
    }

    fn accounting_metadata(&self, candidate: &str) -> Option<ProviderModelMetadata> {
        let dynamic = self
            .dynamic_models
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let model = self
            .models
            .get(candidate)
            .or_else(|| dynamic.get(candidate))?;
        let dynamic_providers = self
            .dynamic_providers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cached = self
            .providers
            .get(candidate)
            .or_else(|| dynamic_providers.get(candidate))
            .and_then(|provider| provider.cached_model_metadata_for(&model.model));
        Some(cached.map_or_else(
            || ProviderModelMetadata {
                capabilities: model.capabilities.clone(),
                pricing: model.pricing.clone(),
                accounting: model.accounting,
            },
            |metadata| effective_model_metadata(model, Some(metadata)),
        ))
    }

    fn attempt_gate(
        &self,
        candidates: &[ModelCandidate],
        invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<Arc<dyn rw_providers::ProviderAttemptGate>, RouterError> {
        if candidates.len() > 64 {
            return Err(RouterError::OperationAdmission(
                "provider call has more than 64 candidate routes".to_owned(),
            ));
        }
        let metadata = candidates
            .iter()
            .filter_map(|route| {
                let candidate = self
                    .route_candidates
                    .get(&route.provider)
                    .map_or(route.provider.as_str(), String::as_str);
                self.accounting_metadata(candidate)
                    .map(|metadata| (route.clone(), metadata))
            })
            .collect();
        Ok(Arc::new(crate::provider_admission::gate::InvocationGate {
            invocation,
            metadata,
        }))
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
        invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, RouterError> {
        if self.models.contains_key(alias)
            || self
                .dynamic_models
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(alias)
        {
            return self.stream_concrete(alias, request, invocation);
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
            let gate = self.attempt_gate(&candidates, invocation)?;
            return self
                .router
                .stream_candidates(alias, candidates, request, gate);
        }
        let candidates = self.router.resolve(alias)?.to_vec();
        let gate = self.attempt_gate(&candidates, invocation)?;
        self.router
            .stream_candidates(alias, candidates, request, gate)
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
        invocation: crate::provider_admission::ProviderInvocation,
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
            return self.stream_concrete(alias, request, invocation);
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
        let gate = self.attempt_gate(&candidates, invocation)?;
        self.router
            .stream_candidates(alias, candidates, request, gate)
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
        invocation: crate::provider_admission::ProviderInvocation,
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
        let route = ModelCandidate {
            provider: candidate.to_owned(),
            model: request.model.clone(),
        };
        let gate = self.attempt_gate(std::slice::from_ref(&route), invocation)?;
        self.router.stream_provider(route, provider, request, gate)
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
            continuation_configuration: continuation_configuration(&self.config, provider_name)?,
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
            continuation_configuration: continuation_configuration(&self.config, provider_name)?,
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

pub(super) async fn discover_runtime_provider(
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
    fn generation(&self) -> u64 {
        0
    }

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
