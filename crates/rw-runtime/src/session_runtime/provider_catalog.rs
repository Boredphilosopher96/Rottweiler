use crate::storage_root::initialize_private_storage_root;
use async_trait::async_trait;
use miette::IntoDiagnostic;
use miette::Result;
use miette::miette;
use rw_core::CachedModelCatalog;
use rw_core::Config;
use rw_core::ModelCatalogError;
use rw_core::ModelCatalogSnapshot;
use rw_core::ModelCatalogSource;
use rw_core::ProviderFactory;
use rw_core::ProviderModelCatalogSource;
use rw_core::merge_model_catalog_provider;
use rw_providers::PricingTable;
use rw_providers::default_models_path;
use rw_store::catalog_cache::load_model_catalog_cache;
use rw_store::catalog_cache::store_model_catalog_cache;
use rw_store::config::ConfigLoader;
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) async fn load_effective_pricing_table() -> Result<PricingTable> {
    let path = default_models_path()
        .map_err(|error| miette!("user model catalog path is unavailable: {error}"))?;
    if path.is_file() {
        PricingTable::load(&path)
            .await
            .map_err(|error| miette!("cached model metadata is invalid: {error}"))
    } else {
        Ok(PricingTable::default())
    }
}

/// Discovers the effective provider model catalog.
///
/// # Errors
/// Returns an error when configuration or provider discovery fails.
pub async fn discover_model_catalog(refresh: bool) -> Result<ModelCatalogSnapshot> {
    let loader = ConfigLoader::from_environment().into_diagnostic()?;
    let credentials_path = loader.credentials_path().clone();
    let effective = loader.load().into_diagnostic()?;
    for warning in effective.warnings() {
        tracing::warn!("{}", warning.message());
    }
    let pricing = load_effective_pricing_table().await?;
    let cache_path = credentials_path
        .parent()
        .ok_or_else(|| miette!("configuration root has no parent"))?
        .join("model-catalog.json");
    let initial_catalog = load_model_catalog_cache(&cache_path)
        .ok()
        .flatten()
        .or_else(|| Some(ProviderModelCatalogSource::placeholder(&effective.config)));
    let source = Arc::new(ProviderModelCatalogSource::system(
        credentials_path,
        pricing,
        effective.config,
    ));
    let snapshot = CachedModelCatalog::with_initial(source, initial_catalog)
        .get(refresh)
        .await
        .map_err(|error| miette!(error.to_string()))?;
    if refresh
        && let Some(storage_root) = cache_path.parent()
        && initialize_private_storage_root(storage_root).is_ok()
        && store_model_catalog_cache(&cache_path, &snapshot).is_err()
    {
        tracing::warn!("refreshed models could not be cached securely");
    }
    Ok(snapshot)
}

pub(super) struct ReloadingHostedCatalogSource {
    pub(super) factory: ProviderFactory,
    pub(super) base_config: Config,
    pub(super) user_config_path: PathBuf,
    pub(super) project_config_path: PathBuf,
}

/// Persists both full and provider-scoped live catalogs. Provider auth uses
/// the scoped path, so omitting this wrapper would leave the process cache
/// healthy while the next app launch fell back to an unauthenticated
/// placeholder until the provider modal forced another refresh.
pub(super) struct PersistingHostedCatalogSource {
    pub(super) inner: Arc<dyn ModelCatalogSource>,
    pub(super) cache_path: PathBuf,
    pub(super) initial: ModelCatalogSnapshot,
}

#[async_trait]
impl ModelCatalogSource for PersistingHostedCatalogSource {
    fn generation(&self) -> u64 {
        self.inner.generation()
    }

    async fn discover(&self) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        let snapshot = self.inner.discover().await?;
        persist_catalog_snapshot(self.cache_path.clone(), snapshot.clone()).await;
        Ok(snapshot)
    }

    async fn discover_provider(
        &self,
        provider: &str,
    ) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        let update = self.inner.discover_provider(provider).await?;
        let base = load_model_catalog_cache(&self.cache_path)
            .ok()
            .flatten()
            .unwrap_or_else(|| self.initial.clone());
        let durable = merge_model_catalog_provider(base, update.clone(), provider);
        persist_catalog_snapshot(self.cache_path.clone(), durable).await;
        Ok(update)
    }
}

pub(super) async fn persist_catalog_snapshot(path: PathBuf, snapshot: ModelCatalogSnapshot) {
    // Catalog persistence is a cache optimization. A successful authenticated
    // provider operation must not be relabelled as failed if the private cache
    // cannot be refreshed.
    let _ = rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
        store_model_catalog_cache(&path, &snapshot)
    })
    .await;
}

#[async_trait]
impl ModelCatalogSource for ReloadingHostedCatalogSource {
    fn generation(&self) -> u64 {
        0
    }

    async fn discover(&self) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        let user_config_path = self.user_config_path.clone();
        let project_config_path = self.project_config_path.clone();
        let base_config = self.base_config.clone();
        let config = rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            ConfigLoader::new(user_config_path, project_config_path)
                .load()
                .map(|loaded| merge_reloaded_provider_config(base_config, loaded.config))
        })
        .await
        .map_err(|_| ModelCatalogError("provider configuration reload failed".to_owned()))?
        .map_err(|_| {
            ModelCatalogError("effective provider configuration is unavailable".to_owned())
        })?;
        self.factory
            .discover_model_catalog(&config)
            .await
            .map_err(|error| ModelCatalogError(error.to_string()))
    }

    async fn discover_provider(
        &self,
        provider: &str,
    ) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        let user_config_path = self.user_config_path.clone();
        let project_config_path = self.project_config_path.clone();
        let base_config = self.base_config.clone();
        let config = rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            ConfigLoader::new(user_config_path, project_config_path)
                .load()
                .map(|loaded| merge_reloaded_provider_config(base_config, loaded.config))
        })
        .await
        .map_err(|_| ModelCatalogError("provider configuration reload failed".to_owned()))?
        .map_err(|_| {
            ModelCatalogError("effective provider configuration is unavailable".to_owned())
        })?;
        self.factory
            .discover_provider_model_catalog(&config, provider)
            .await
            .map_err(|error| ModelCatalogError(error.to_string()))
    }
}

pub(super) fn merge_reloaded_provider_config(mut base: Config, loaded: Config) -> Config {
    for (name, provider) in loaded.providers {
        base.providers.entry(name).or_insert(provider);
    }
    if base.models.aliases.is_empty() && !loaded.models.aliases.is_empty() {
        base.models = loaded.models;
    }
    base
}
