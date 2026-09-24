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
        PricingTable::bundled()
            .map_err(|error| miette!("bundled model metadata is invalid: {error}"))
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
    if refresh {
        refresh_stale_metadata().await;
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

// Refresh only on model discovery, never on the startup readiness path. Requests
// share a process-wide retry interval and retain an offline fallback on failure.
async fn refresh_stale_metadata() {
    use std::time::{Duration, Instant};
    static LAST_ATTEMPT: tokio::sync::Mutex<Option<Instant>> = tokio::sync::Mutex::const_new(None);
    let Ok(mut last) = LAST_ATTEMPT.try_lock() else {
        return;
    };
    if last.is_some_and(|instant| instant.elapsed() < Duration::from_hours(1)) {
        return;
    }
    let Ok(path) = default_models_path() else {
        return;
    };
    let fresh = rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
        std::fs::metadata(path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age < Duration::from_hours(24))
    })
    .await
    .unwrap_or(false);
    if fresh {
        return;
    }
    *last = Some(Instant::now());
    let _ = rw_core::refresh_model_catalog_with_download_timeout(
        rw_providers::DEFAULT_MODELS_DEV_URL,
        None,
        Some(Duration::from_secs(3)),
    )
    .await;
}

impl ReloadingHostedCatalogSource {
    async fn refreshed_factory(&self) -> Result<ProviderFactory, ModelCatalogError> {
        // Injected roots must never discover or replace a different user's metadata.
        // Their supplied factory remains the fallback until a scoped file exists.
        let scoped_path = self.user_config_path.with_file_name("models.toml");
        if default_models_path().is_ok_and(|path| path == scoped_path) {
            refresh_stale_metadata().await;
        }
        let factory = self.factory.clone();
        let user_config_path = self.user_config_path.clone();
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            super::provider_activation::refreshed_activation_factory(&factory, &user_config_path)
        })
        .await
        .map_err(|_| ModelCatalogError("model metadata reload failed".into()))?
        .map_err(|_| ModelCatalogError("model metadata is unavailable".into()))
    }
}

/// Persists both full and provider-scoped live catalogs so authenticated model
/// availability survives process restart without another catalog refresh.
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
        self.refreshed_factory()
            .await?
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
        self.refreshed_factory()
            .await?
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

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use rw_core::ModelDriver;
    use rw_types::config::ProviderConfig;

    fn pricing(context: u64) -> PricingTable {
        PricingTable {
            source_url: "https://example.test/models".into(),
            snapshot_date: "2026-09-13".into(),
            revision: context.to_string(),
            models: std::collections::BTreeMap::from([(
                "local/test".into(),
                rw_providers::ModelPricing {
                    max_context_tokens: Some(context),
                    supports_tools: true,
                    ..Default::default()
                },
            )]),
        }
    }

    #[tokio::test]
    async fn scoped_metadata_refresh_updates_only_the_new_runtime_generation()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let user_config_path = root.path().join("config.toml");
        let factory = ProviderFactory::system(root.path().join("credentials.toml"), pricing(1000));
        let mut config = Config::default();
        config.providers.insert(
            "local".into(),
            ProviderConfig {
                kind: "openai_compatible".into(),
                base_url: Some("http://127.0.0.1:12345/v1".into()),
                ..Default::default()
            },
        );
        config.models.default = "local/test".into();
        config.models.aliases =
            std::collections::BTreeMap::from([("local/test".into(), vec!["local/test".into()])]);
        config.models.thinking.clear();
        let source = ReloadingHostedCatalogSource {
            factory: factory.clone(),
            base_config: config.clone(),
            user_config_path: user_config_path.clone(),
            project_config_path: root.path().join("project.toml"),
        };
        let old = source.refreshed_factory().await?.build(&config)?;
        assert_eq!(
            old.context_metadata("local/test").max_context_tokens,
            Some(1000)
        );
        std::fs::write(root.path().join("models.toml"), pricing(2000).to_toml()?)?;
        let discovered = source.refreshed_factory().await?.build(&config)?;
        let activated = super::super::provider_activation::refreshed_activation_factory(
            &factory,
            &user_config_path,
        )?
        .build(&config)?;
        assert_eq!(
            discovered.context_metadata("local/test").max_context_tokens,
            Some(2000)
        );
        assert_eq!(
            activated.context_metadata("local/test").max_context_tokens,
            Some(2000)
        );
        assert_eq!(
            old.context_metadata("local/test").max_context_tokens,
            Some(1000)
        );
        std::fs::write(root.path().join("models.toml"), "invalid metadata")?;
        assert!(source.refreshed_factory().await.is_err());
        Ok(())
    }
}
