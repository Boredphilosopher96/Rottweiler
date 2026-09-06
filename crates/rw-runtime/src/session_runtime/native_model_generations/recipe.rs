//! Rebuild provider routing from immutable configuration and candidate endpoints.
use super::super::{
    native_search::AliasAwareWebSearchModel, nested_instructions::NestedInstructionsModel,
    prompt_model::PromptRecordingModel, prompt_shapes::PromptShapeJournal,
    provider_activation::lazy_live_provider_model, provider_catalog::PersistingHostedCatalogSource,
};
use super::{NativeChildComposer, NativeModelGeneration, NativeModelInput};
use rw_core::{
    AgentLoopError, Config, ModelCatalogSource, ModelDriver, ProviderFactory,
    ProviderModelCatalogSource,
};
use rw_providers::{FixtureRedactor, PricingTable};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, OnceLock, RwLock},
};

pub(in crate::session_runtime) enum NativeProviderRecipe {
    Live {
        credentials: PathBuf,
        pricing: PricingTable,
        config: Config,
    },
    Fixed(Arc<dyn ModelDriver>),
}
pub(in crate::session_runtime) struct NativeModelRecipe {
    pub provider: NativeProviderRecipe,
    pub redactor: FixtureRedactor,
    pub prompt_shapes: Option<Arc<PromptShapeJournal>>,
    pub catalog_path: PathBuf,
    pub instruction_roots: Arc<RwLock<Vec<PathBuf>>>,
    pub active_sources: Arc<RwLock<BTreeSet<PathBuf>>>,
}
impl NativeModelRecipe {
    /// Each child owns model selection and plugin endpoints. Only the immutable
    /// provider configuration is captured; endpoints are supplied by that child.
    pub(in crate::session_runtime) fn child_composer(&self) -> Arc<NativeChildComposer> {
        match &self.provider {
            NativeProviderRecipe::Live {
                credentials,
                pricing,
                config,
            } => {
                let factory = ProviderFactory::system(credentials, pricing.clone());
                let config = config.clone();
                let user_config = credentials.with_file_name("config.toml");
                let redactor = self.redactor.clone();
                Arc::new(move |workspace, alias, providers| {
                    lazy_live_provider_model(
                        factory.clone().with_extension_providers(providers),
                        config.clone(),
                        user_config.clone(),
                        workspace.join(".rottweiler/config.toml"),
                        alias.to_owned(),
                        redactor.clone(),
                    )
                })
            }
            NativeProviderRecipe::Fixed(provider) => {
                let provider = Arc::clone(provider);
                Arc::new(move |_, _, _| Arc::clone(&provider))
            }
        }
    }

    pub(in crate::session_runtime) fn compose(
        &self,
        input: NativeModelInput,
    ) -> Result<NativeModelGeneration, AgentLoopError> {
        let children = self.child_composer();
        let (provider, catalog): (Arc<dyn ModelDriver>, Option<Arc<dyn ModelCatalogSource>>) =
            match &self.provider {
                NativeProviderRecipe::Live {
                    credentials,
                    pricing,
                    config,
                } => {
                    let primary = input.roots.first().ok_or_else(|| {
                        AgentLoopError::InvalidConfiguration(
                            "provider generation has no workspace".into(),
                        )
                    })?;
                    let factory = ProviderFactory::system(credentials, pricing.clone())
                        .with_extension_providers(input.providers);
                    let model = lazy_live_provider_model(
                        factory,
                        config.clone(),
                        credentials.with_file_name("config.toml"),
                        primary.join(".rottweiler/config.toml"),
                        input.alias,
                        self.redactor.clone(),
                    );
                    let catalog: Arc<dyn ModelCatalogSource> =
                        Arc::new(PersistingHostedCatalogSource {
                            inner: model.clone(),
                            cache_path: self.catalog_path.clone(),
                            initial: ProviderModelCatalogSource::placeholder(config),
                        });
                    (model, Some(catalog))
                }
                NativeProviderRecipe::Fixed(provider) => (Arc::clone(provider), None),
            };
        let mut model = Arc::clone(&provider);
        if let Some(journal) = &self.prompt_shapes {
            model = Arc::new(PromptRecordingModel {
                inner: model,
                journal: Arc::clone(journal),
            });
        }
        model = AliasAwareWebSearchModel::wrap(model, input.websearch.as_ref());
        let tools = OnceLock::from(Arc::downgrade(&input.tools));
        model = Arc::new(NestedInstructionsModel {
            inner: model,
            tools: Arc::new(tools),
            workspace_roots: Arc::clone(&self.instruction_roots),
            active_sources: Arc::clone(&self.active_sources),
            memory_redactor: self.redactor.clone(),
        });
        Ok(NativeModelGeneration {
            model,
            provider,
            children,
            catalog,
            redactor: self.redactor.clone(),
        })
    }
}
