use super::model_effects;
use super::provider_activation::ActivatedHostedProvider;
use super::provider_activation::HostedProviderActivator;
use super::provider_activation::HostedRuntimeInitializer;
use async_trait::async_trait;
use miette::Result;
use rw_core::AgentLoopError;
use rw_core::ModelCatalogError;
use rw_core::ModelCatalogSnapshot;
use rw_core::ModelCatalogSource;
use rw_core::ModelDriver;
use rw_providers::BoxEventStream;
use rw_providers::ProviderRequest;
use rw_types::config::ThinkingLevel;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Prepares a private provider runtime generation and stages it by provider.
/// Connecting a provider never changes the active model; a staged generation
/// is swapped in only when the user later selects one of that provider's
/// concrete catalog models. A timed-out blocking preparation may continue, but
/// it owns no live session state and therefore cannot commit late.
pub(super) struct RecomposableHostedModel {
    pub(super) effects: model_effects::ModelEffects,
    pub(super) model: RwLock<Arc<dyn ModelDriver>>,
    pub(super) standby: RwLock<BTreeMap<String, ActivatedHostedProvider>>,
    pub(super) retained: RwLock<Vec<RetainedHostedSelection>>,
    pub(super) prepared: RwLock<BTreeMap<String, PreparedHostedSelection>>,
    pub(super) active_post_commit: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
    pub(super) catalog: Arc<dyn ModelCatalogSource>,
    pub(super) activate: Arc<HostedProviderActivator>,
    pub(super) initialize: Option<Arc<HostedRuntimeInitializer>>,
    pub(super) initial_alias: Option<String>,
    pub(super) initial_load_pending: AtomicBool,
    pub(super) activation: tokio::sync::Mutex<()>,
    pub(super) activation_deadline: Duration,
    pub(super) activation_inflight: Arc<AtomicBool>,
}

#[derive(Clone)]
pub(super) struct PreparedHostedSelection {
    pub(super) provider: Option<String>,
    pub(super) replacement_model: Arc<dyn ModelDriver>,
    pub(super) post_commit: Option<Arc<dyn Fn() + Send + Sync>>,
    pub(super) completes_initialization: bool,
}

#[derive(Clone)]
pub(super) struct RetainedHostedSelection {
    pub(super) model: Arc<dyn ModelDriver>,
    pub(super) post_commit: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl RecomposableHostedModel {
    #[cfg(test)]
    pub(super) fn new(
        inner: Arc<dyn ModelDriver>,
        catalog: Arc<dyn ModelCatalogSource>,
        activate: Arc<HostedProviderActivator>,
    ) -> Self {
        Self::with_deadline_and_active_callback(
            inner,
            catalog,
            activate,
            Duration::from_secs(5),
            None,
            None,
            None,
        )
    }

    #[cfg(test)]
    pub(super) fn new_with_active_callback(
        inner: Arc<dyn ModelDriver>,
        catalog: Arc<dyn ModelCatalogSource>,
        activate: Arc<HostedProviderActivator>,
        active_post_commit: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        Self::with_deadline_and_active_callback(
            inner,
            catalog,
            activate,
            Duration::from_secs(5),
            active_post_commit,
            None,
            None,
        )
    }

    pub(super) fn new_lazy(
        inner: Arc<dyn ModelDriver>,
        initial_alias: String,
        catalog: Arc<dyn ModelCatalogSource>,
        activate: Arc<HostedProviderActivator>,
        initialize: Arc<HostedRuntimeInitializer>,
    ) -> Self {
        Self::with_deadline_and_active_callback(
            inner,
            catalog,
            activate,
            Duration::from_secs(5),
            None,
            Some(initialize),
            Some(initial_alias),
        )
    }

    #[cfg(test)]
    pub(super) fn with_deadline(
        inner: Arc<dyn ModelDriver>,
        catalog: Arc<dyn ModelCatalogSource>,
        activate: Arc<HostedProviderActivator>,
        activation_deadline: Duration,
    ) -> Self {
        Self::with_deadline_and_active_callback(
            inner,
            catalog,
            activate,
            activation_deadline,
            None,
            None,
            None,
        )
    }

    pub(super) fn with_deadline_and_active_callback(
        inner: Arc<dyn ModelDriver>,
        catalog: Arc<dyn ModelCatalogSource>,
        activate: Arc<HostedProviderActivator>,
        activation_deadline: Duration,
        active_post_commit: Option<Arc<dyn Fn() + Send + Sync>>,
        initialize: Option<Arc<HostedRuntimeInitializer>>,
        initial_alias: Option<String>,
    ) -> Self {
        let initial_load_pending = initialize.is_some();
        Self {
            effects: model_effects::ModelEffects::default(),
            model: RwLock::new(inner),
            standby: RwLock::new(BTreeMap::new()),
            retained: RwLock::new(Vec::new()),
            prepared: RwLock::new(BTreeMap::new()),
            active_post_commit: RwLock::new(active_post_commit),
            catalog,
            activate,
            initialize,
            initial_alias,
            initial_load_pending: AtomicBool::new(initial_load_pending),
            activation: tokio::sync::Mutex::new(()),
            activation_deadline,
            activation_inflight: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn current(&self) -> Arc<dyn ModelDriver> {
        self.model
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(super) fn commit_selection(&self, prepared: PreparedHostedSelection) {
        if let Some(provider) = &prepared.provider {
            self.standby
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(provider);
        }
        let previous = {
            let mut current = self
                .model
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::replace(&mut *current, Arc::clone(&prepared.replacement_model))
        };
        let previous_post_commit = {
            let mut active = self
                .active_post_commit
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::replace(&mut *active, prepared.post_commit.clone())
        };
        if !Arc::ptr_eq(&previous, &prepared.replacement_model) {
            let mut retained = self
                .retained
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !retained
                .iter()
                .any(|known| Arc::ptr_eq(&known.model, &previous))
            {
                retained.push(RetainedHostedSelection {
                    model: previous,
                    post_commit: previous_post_commit,
                });
            }
            retained.retain(|known| !Arc::ptr_eq(&known.model, &prepared.replacement_model));
        }
        if let Some(post_commit) = prepared.post_commit {
            post_commit();
        }
        if prepared.completes_initialization {
            self.initial_load_pending.store(false, Ordering::Release);
        }
    }

    pub(super) async fn stage_standby_model(
        &self,
        alias: &str,
        provider: &str,
    ) -> std::result::Result<bool, AgentLoopError> {
        let activated = self
            .standby
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
            .cloned();
        let Some(activated) = activated else {
            return Ok(false);
        };
        activated.replacement_model.prepare_model(alias).await?;
        if !activated.replacement_model.has_model_alias(alias) {
            return Err(AgentLoopError::Provider(format!(
                "model {alias:?} is not available from the connected provider"
            )));
        }
        self.prepared
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                alias.to_owned(),
                PreparedHostedSelection {
                    provider: Some(provider.to_owned()),
                    replacement_model: activated.replacement_model,
                    post_commit: activated.post_commit,
                    completes_initialization: false,
                },
            );
        Ok(true)
    }

    pub(super) async fn initialize_model(
        &self,
        alias: &str,
    ) -> std::result::Result<bool, AgentLoopError> {
        if !self.initial_load_pending.load(Ordering::Acquire) {
            return Ok(false);
        }
        let _activation = self.activation.lock().await;
        if !self.initial_load_pending.load(Ordering::Acquire) {
            return Ok(false);
        }
        if self
            .prepared
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(alias)
        {
            return Ok(true);
        }
        let Some(initialize) = self.initialize.clone() else {
            return Ok(false);
        };
        // This is an explicitly requested first provider use and can include a
        // browser/device handshake plus live model discovery. Do not impose the
        // short provider-menu activation deadline on that network-bound flow.
        // Credentials come from Rottweiler's private file, so this path never
        // invokes an operating-system credential prompt. If this future is
        // cancelled, the private result owns no live session state and cannot
        // commit late.
        let alias_owned = alias.to_owned();
        let mut initialized = tokio::task::spawn_blocking(move || initialize(&alias_owned))
            .await
            .map_err(|_| AgentLoopError::Provider("provider initialization failed".to_owned()))??;
        if let Some(pre_commit) = initialized.pre_commit.take() {
            pre_commit();
        }
        initialized.replacement_model.prepare_model(alias).await?;
        if !initialized.replacement_model.has_model_alias(alias) {
            return Err(AgentLoopError::Provider(format!(
                "model {alias:?} is not available from the initialized provider runtime"
            )));
        }
        let prepared = PreparedHostedSelection {
            provider: None,
            replacement_model: initialized.replacement_model,
            post_commit: initialized.post_commit,
            completes_initialization: true,
        };
        if self.initial_alias.as_deref() == Some(alias) {
            // The session's initial selection is already durable before the
            // lazy provider runtime exists. Ordinary turns prepare that same
            // alias without a ModelChanged event, so it is safe and necessary
            // to activate here. Commit directly instead of briefly publishing
            // the selection in `prepared`: a concurrent first-turn prepare
            // must never observe a staged selection and stream through the
            // unavailable placeholder before this activation completes.
            self.commit_selection(prepared);
        } else {
            // A different alias is a model switch and remains staged until the
            // durable ModelChanged event commits.
            self.prepared
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(alias.to_owned(), prepared);
        }
        Ok(true)
    }
}

#[async_trait]
impl ModelCatalogSource for RecomposableHostedModel {
    fn generation(&self) -> u64 {
        self.catalog.generation()
    }

    async fn discover(&self) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        self.catalog.discover().await
    }

    async fn discover_provider(
        &self,
        provider: &str,
    ) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        self.catalog.discover_provider(provider).await
    }
}

#[async_trait]
impl ModelDriver for RecomposableHostedModel {
    fn native_web_searcher(
        &self,
        alias: &str,
        invocation: rw_core::provider_admission::ProviderInvocation,
    ) -> Option<Arc<dyn rw_tools::WebSearcher>> {
        self.current().native_web_searcher(alias, invocation)
    }

    async fn settle_effects(&self) -> std::result::Result<(), rw_core::AgentLoopError> {
        self.effects.settle().await
    }

    fn stream(
        &self,
        alias: &str,
        request: ProviderRequest,
        invocation: rw_core::provider_admission::ProviderInvocation,
    ) -> std::result::Result<BoxEventStream, AgentLoopError> {
        self.effects.stream(self.current(), |driver| {
            driver.stream(alias, request, invocation)
        })
    }

    fn stream_for_provider(
        &self,
        alias: &str,
        provider: Option<&str>,
        request: ProviderRequest,
        invocation: rw_core::provider_admission::ProviderInvocation,
    ) -> std::result::Result<BoxEventStream, AgentLoopError> {
        self.effects.stream(self.current(), |driver| {
            driver.stream_for_provider(alias, provider, request, invocation)
        })
    }

    fn context_metadata(&self, alias: &str) -> rw_core::ModelContextMetadata {
        self.current().context_metadata(alias)
    }

    fn has_model_alias(&self, alias: &str) -> bool {
        if self.initial_load_pending.load(Ordering::Acquire) {
            return !alias.trim().is_empty();
        }
        if self.current().has_model_alias(alias) {
            return true;
        }
        let Some((provider, model)) = alias.split_once('/') else {
            return false;
        };
        if provider.is_empty() || model.trim().is_empty() {
            return false;
        }
        if self
            .standby
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
            .is_some_and(|activated| activated.replacement_model.has_model_alias(alias))
        {
            return true;
        }
        if self
            .prepared
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(alias)
            .is_some_and(|prepared| prepared.replacement_model.has_model_alias(alias))
        {
            return true;
        }
        self.retained
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|retained| retained.model.has_model_alias(alias))
    }

    fn title_model_alias(&self) -> Option<String> {
        self.current().title_model_alias()
    }

    async fn prepare_model(&self, alias: &str) -> std::result::Result<(), AgentLoopError> {
        self.prepared
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(alias);
        if let Some((provider, _)) = alias.split_once('/')
            && self.stage_standby_model(alias, provider).await?
        {
            return Ok(());
        }
        if self.initialize_model(alias).await? {
            return Ok(());
        }
        let current = self.current();
        let current_error = match current.prepare_model(alias).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let retained = self
            .retained
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for candidate in retained.into_iter().rev() {
            if candidate.model.prepare_model(alias).await.is_ok()
                && candidate.model.has_model_alias(alias)
            {
                self.prepared
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        alias.to_owned(),
                        PreparedHostedSelection {
                            provider: None,
                            replacement_model: candidate.model,
                            post_commit: candidate.post_commit,
                            completes_initialization: false,
                        },
                    );
                return Ok(());
            }
        }
        Err(current_error)
    }

    fn commit_prepared_model(&self, alias: &str) {
        let prepared = self
            .prepared
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(alias);
        let Some(prepared) = prepared else {
            return;
        };
        self.commit_selection(prepared);
    }

    fn discard_prepared_model(&self, alias: &str) {
        self.prepared
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(alias);
    }

    async fn activate_provider(
        &self,
        provider: &str,
        _selected_model: Option<&str>,
    ) -> std::result::Result<(), AgentLoopError> {
        let _activation = self.activation.lock().await;
        let activate = Arc::clone(&self.activate);
        let provider = provider.to_owned();
        let activation_provider = provider.clone();
        if self
            .activation_inflight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(AgentLoopError::Provider(
                "provider activation is already in progress".to_owned(),
            ));
        }
        let inflight = Arc::clone(&self.activation_inflight);
        let mut activated = tokio::time::timeout(
            self.activation_deadline,
            tokio::task::spawn_blocking(move || {
                struct ClearInflight(Arc<AtomicBool>);
                impl Drop for ClearInflight {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Release);
                    }
                }
                let _clear = ClearInflight(inflight);
                activate(&activation_provider)
            }),
        )
        .await
        .map_err(|_| AgentLoopError::Provider("provider activation timed out".to_owned()))?
        .map_err(|_| AgentLoopError::Provider("provider activation failed".to_owned()))??;
        if let Some(pre_commit) = &activated.pre_commit {
            pre_commit();
        }
        activated.pre_commit = None;
        self.standby
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider, activated);
        Ok(())
    }

    fn has_provider_for_alias(&self, alias: &str, provider: &str) -> bool {
        let exact_provider_route = alias
            .split_once('/')
            .is_some_and(|(alias_provider, model)| {
                alias_provider == provider && !model.trim().is_empty()
            });
        if exact_provider_route && self.initial_load_pending.load(Ordering::Acquire) {
            // The initial lazy runtime is intentionally unavailable until the
            // user selects a model. Exact concrete routes still need to pass
            // protocol prevalidation so the context-transfer choice can be
            // shown before provider preparation touches credentials.
            return true;
        }
        if self.current().has_provider_for_alias(alias, provider) {
            return true;
        }
        if self
            .standby
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
            .is_some_and(|activated| {
                activated
                    .replacement_model
                    .has_provider_for_alias(alias, provider)
                    || (exact_provider_route && activated.replacement_model.has_model_alias(alias))
            })
        {
            // A successfully authenticated provider is staged independently
            // of the active model. Its exact routes are selectable before the
            // later prepare/commit step swaps the runtime generation.
            return true;
        }
        if self
            .prepared
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(alias)
            .is_some_and(|prepared| {
                prepared
                    .replacement_model
                    .has_provider_for_alias(alias, provider)
            })
        {
            return true;
        }
        self.retained
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|model| model.model.has_provider_for_alias(alias, provider))
    }

    fn thinking_for_model(&self, model: &str, fallback: ThinkingLevel) -> ThinkingLevel {
        if let Some(prepared) = self
            .prepared
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(model)
        {
            return prepared
                .replacement_model
                .thinking_for_model(model, fallback);
        }
        self.current().thinking_for_model(model, fallback)
    }

    fn supports_vision(&self, alias: &str) -> bool {
        self.current().supports_vision(alias)
    }

    fn compaction_config(&self) -> rw_core::CompactionConfig {
        self.current().compaction_config()
    }

    fn budget_config(&self) -> rw_core::BudgetConfig {
        self.current().budget_config()
    }

    fn cost(&self, alias: &str, usage: rw_core::ModelTokenUsage) -> rw_core::Cost {
        self.current().cost(alias, usage)
    }

    fn cost_for_reported_model(
        &self,
        alias: &str,
        reported_model: Option<&str>,
        usage: rw_core::ModelTokenUsage,
    ) -> rw_core::Cost {
        self.current()
            .cost_for_reported_model(alias, reported_model, usage)
    }

    fn cost_for_route(
        &self,
        alias: &str,
        route: Option<&str>,
        reported_model: Option<&str>,
        usage: rw_core::ModelTokenUsage,
    ) -> rw_core::Cost {
        self.current()
            .cost_for_route(alias, route, reported_model, usage)
    }

    fn qualified_model_for_route(
        &self,
        alias: &str,
        route: Option<&str>,
        reported_model: Option<&str>,
    ) -> Option<String> {
        self.current()
            .qualified_model_for_route(alias, route, reported_model)
    }
}
