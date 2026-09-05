use super::model_selection::RecomposableHostedModel;
use super::native_search::RuntimeWebSearcher;
use super::provider_adapter::UnavailableHostedModel;
use super::provider_catalog::ReloadingHostedCatalogSource;
use super::provider_catalog::merge_reloaded_provider_config;
use rw_core::AgentLoopError;
use rw_core::Config;
use rw_core::ModelCatalogSource;
use rw_core::ModelDriver;
use rw_core::ProviderFactory;
use rw_providers::FixtureRedactor;
use rw_store::config::ConfigLoader;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) fn prepare_provider_activation_config(
    mut config: Config,
    provider: &str,
) -> std::result::Result<Config, AgentLoopError> {
    config.providers.get(provider).ok_or_else(|| {
        AgentLoopError::InvalidConfiguration(format!("provider {provider:?} is not configured"))
    })?;
    if config.models.aliases.is_empty() {
        let model = provider_activation_candidate();
        let default = config.models.default.clone();
        config
            .models
            .aliases
            .insert(default, vec![format!("{provider}/{model}")]);
    }
    Ok(config)
}

pub(super) fn prepare_isolated_provider_activation_config(
    mut config: Config,
    provider: &str,
) -> std::result::Result<Config, AgentLoopError> {
    let provider_config = config.providers.get(provider).cloned().ok_or_else(|| {
        AgentLoopError::InvalidConfiguration(format!("provider {provider:?} is not configured"))
    })?;
    config.providers = BTreeMap::from([(provider.to_owned(), provider_config)]);
    config.models.aliases.retain(|_, candidates| {
        candidates.retain(|candidate| {
            candidate
                .split_once('/')
                .is_some_and(|(owner, model)| owner == provider && !model.is_empty())
        });
        !candidates.is_empty()
    });
    if config.models.aliases.is_empty() {
        "__provider_connection".clone_into(&mut config.models.default);
        config.models.aliases = BTreeMap::from([(
            config.models.default.clone(),
            vec![format!("{provider}/{}", provider_activation_candidate())],
        )]);
        config.models.thinking.clear();
    } else {
        if !config.models.aliases.contains_key(&config.models.default)
            && let Some(first_alias) = config.models.aliases.keys().next()
        {
            config.models.default.clone_from(first_alias);
        }
        config
            .models
            .thinking
            .retain(|alias, _| config.models.aliases.contains_key(alias));
    }
    Ok(config)
}

pub(super) fn prepare_isolated_model_initialization_config(
    mut config: Config,
    alias: &str,
) -> std::result::Result<Config, AgentLoopError> {
    let alias = alias.trim();
    if alias.is_empty() {
        return Err(AgentLoopError::InvalidConfiguration(
            "model alias must not be empty".to_owned(),
        ));
    }

    let (route_alias, candidates) = if let Some(candidates) = config.models.aliases.get(alias) {
        if candidates.is_empty() {
            return Err(AgentLoopError::InvalidConfiguration(format!(
                "model alias {alias:?} has no configured routes"
            )));
        }
        (alias.to_owned(), candidates.clone())
    } else if let Some((provider, model)) = alias.split_once('/') {
        if provider.is_empty() || model.is_empty() {
            return Err(AgentLoopError::InvalidConfiguration(format!(
                "model selection {alias:?} must use provider/model syntax"
            )));
        }
        ("__selected_model".to_owned(), vec![alias.to_owned()])
    } else {
        return Err(AgentLoopError::InvalidConfiguration(format!(
            "model alias {alias:?} is not configured"
        )));
    };

    let mut providers = std::collections::BTreeSet::new();
    for candidate in &candidates {
        let (provider, model) = candidate.split_once('/').ok_or_else(|| {
            AgentLoopError::InvalidConfiguration(format!(
                "model candidate {candidate:?} must use provider/model syntax"
            ))
        })?;
        if provider.is_empty() || model.is_empty() {
            return Err(AgentLoopError::InvalidConfiguration(format!(
                "model candidate {candidate:?} must use provider/model syntax"
            )));
        }
        providers.insert(provider.to_owned());
    }

    config
        .providers
        .retain(|provider, _| providers.contains(provider));
    config.models.aliases = BTreeMap::from([(route_alias.clone(), candidates)]);
    config.models.default.clone_from(&route_alias);
    config
        .models
        .thinking
        .retain(|configured_alias, _| configured_alias == &route_alias);
    Ok(config)
}

pub(super) fn provider_activation_candidate() -> &'static str {
    "catalog-discovery"
}

#[derive(Clone)]
pub(super) struct ActivatedHostedProvider {
    pub(super) replacement_model: Arc<dyn ModelDriver>,
    pub(super) pre_commit: Option<Arc<dyn Fn() + Send + Sync>>,
    pub(super) post_commit: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub(super) type HostedProviderActivator =
    dyn Fn(&str) -> std::result::Result<ActivatedHostedProvider, AgentLoopError> + Send + Sync;

pub(super) type HostedRuntimeInitializer =
    dyn Fn(&str) -> std::result::Result<ActivatedHostedProvider, AgentLoopError> + Send + Sync;

pub(super) fn live_provider_activator(
    factory: ProviderFactory,
    base_config: Config,
    user_config_path: PathBuf,
    project_config_path: PathBuf,
    redactor: FixtureRedactor,
    searcher: Option<Arc<RuntimeWebSearcher>>,
) -> Arc<HostedProviderActivator> {
    Arc::new(move |provider| {
        let loaded = ConfigLoader::new(user_config_path.clone(), project_config_path.clone())
            .load()
            .map_err(|error| {
                AgentLoopError::InvalidConfiguration(format!(
                    "provider activation configuration could not reload: {error}"
                ))
            })?
            .config;
        let config = merge_reloaded_provider_config(base_config.clone(), loaded);
        let config = prepare_provider_activation_config(config, provider)?;
        // Connecting one provider must not resolve credentials for every
        // other configured provider. Live catalog discovery stays separate.
        let isolated = prepare_isolated_provider_activation_config(config, provider)?;
        let runtime = Arc::new(
            factory
                .build(&isolated)
                .map_err(|error| AgentLoopError::Provider(error.to_string()))?,
        );
        let pre_runtime = Arc::clone(&runtime);
        let pre_redactor = redactor.clone();
        let pre_commit: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            pre_redactor.merge_from(&pre_runtime.fixture_redactor());
        });
        let post_runtime = Arc::clone(&runtime);
        let post_searcher = searcher.clone();
        let post_commit: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(searcher) = &post_searcher {
                let runtime = Arc::clone(&post_runtime);
                searcher.bind_native_resolver(Some(Arc::new(move |alias| {
                    runtime.native_web_searcher(alias)
                })));
            }
        });
        let model: Arc<dyn ModelDriver> = runtime;
        Ok(ActivatedHostedProvider {
            replacement_model: model,
            pre_commit: Some(pre_commit),
            post_commit: Some(post_commit),
        })
    })
}

pub(super) fn lazy_live_provider_model(
    factory: ProviderFactory,
    base_config: Config,
    user_config_path: PathBuf,
    project_config_path: PathBuf,
    persisted_model_alias: String,
    redactor: FixtureRedactor,
    searcher: Option<Arc<RuntimeWebSearcher>>,
) -> Arc<RecomposableHostedModel> {
    let fallback_catalog: Arc<dyn ModelCatalogSource> = Arc::new(ReloadingHostedCatalogSource {
        factory: factory.clone(),
        base_config: base_config.clone(),
        user_config_path: user_config_path.clone(),
        project_config_path: project_config_path.clone(),
    });
    let initial_model: Arc<dyn ModelDriver> = Arc::new(UnavailableHostedModel {
        alias: persisted_model_alias.clone(),
        reason: "the provider has not been connected for this session yet".to_owned(),
        compaction: base_config.compaction.clone(),
        budget: base_config.budget.clone(),
    });

    let initialize_factory = factory.clone();
    let initialize_base_config = base_config.clone();
    let initialize_user_config_path = user_config_path.clone();
    let initialize_project_config_path = project_config_path.clone();
    let initialize_redactor = redactor.clone();
    let initialize_searcher = searcher.clone();
    let initialize: Arc<HostedRuntimeInitializer> = Arc::new(move |alias| {
        let loaded = ConfigLoader::new(
            initialize_user_config_path.clone(),
            initialize_project_config_path.clone(),
        )
        .load()
        .map_err(|error| {
            AgentLoopError::InvalidConfiguration(format!(
                "provider initialization configuration could not reload: {error}"
            ))
        })?
        .config;
        let config = merge_reloaded_provider_config(initialize_base_config.clone(), loaded);
        let isolated = prepare_isolated_model_initialization_config(config, alias)?;
        let runtime = Arc::new(
            initialize_factory
                .build(&isolated)
                .map_err(|error| AgentLoopError::Provider(error.to_string()))?,
        );
        let pre_runtime = Arc::clone(&runtime);
        let pre_redactor = initialize_redactor.clone();
        let pre_commit: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            pre_redactor.merge_from(&pre_runtime.fixture_redactor());
        });
        let post_runtime = Arc::clone(&runtime);
        let post_searcher = initialize_searcher.clone();
        let post_commit: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(searcher) = &post_searcher {
                let runtime = Arc::clone(&post_runtime);
                searcher.bind_native_resolver(Some(Arc::new(move |alias| {
                    runtime.native_web_searcher(alias)
                })));
            }
        });
        let model: Arc<dyn ModelDriver> = runtime;
        Ok(ActivatedHostedProvider {
            replacement_model: model,
            pre_commit: Some(pre_commit),
            post_commit: Some(post_commit),
        })
    });

    let activate = live_provider_activator(
        factory,
        base_config,
        user_config_path,
        project_config_path,
        redactor,
        searcher,
    );

    Arc::new(RecomposableHostedModel::new_lazy(
        initial_model,
        persisted_model_alias,
        fallback_catalog,
        activate,
        initialize,
    ))
}
