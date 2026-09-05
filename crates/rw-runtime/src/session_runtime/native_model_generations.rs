//! One published provider generation for model discovery and child admission.
use async_trait::async_trait;
use rw_core::{
    AgentLoopError, ModelCatalogError, ModelCatalogSnapshot, ModelCatalogSource, ModelDriver,
    ModelSource,
};
use rw_providers::{FixtureRedactor, Provider};
use rw_tools::ToolRegistry;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
};

#[cfg(test)]
mod tests;

mod children;
mod recipe;
pub(super) use children::ChildNativeModel;
pub(super) use recipe::{NativeModelRecipe, NativeProviderRecipe};
pub(crate) type NativeModelComposer =
    dyn Fn(NativeModelInput) -> Result<NativeModelGeneration, AgentLoopError> + Send + Sync;

pub(crate) type NativeChildComposer =
    dyn Fn(&std::path::Path, &str) -> Arc<dyn ModelDriver> + Send + Sync;

pub(crate) struct NativeModelInput {
    pub providers: Vec<(String, Arc<dyn Provider>)>,
    pub tools: Arc<ToolRegistry>,
    pub roots: Vec<PathBuf>,
    pub alias: String,
    pub websearch: Option<Arc<super::native_search::RuntimeWebSearcher>>,
}

/// Every component is prepared without publishing callbacks or contacting a
/// candidate provider. Its managed endpoints remain inert until publication.
pub(crate) struct NativeModelGeneration {
    pub model: Arc<dyn ModelDriver>,
    pub provider: Arc<dyn ModelDriver>,
    pub children: Arc<NativeChildComposer>,
    pub catalog: Option<Arc<dyn ModelCatalogSource>>,
    pub redactor: FixtureRedactor,
}

struct State {
    generation: u64,
    replacing: bool,
    children: usize,
    current: Arc<NativeModelGeneration>,
}

pub(crate) struct NativeModelGenerations {
    state: Mutex<State>,
    compose: Arc<NativeModelComposer>,
}

impl NativeModelGenerations {
    pub(crate) fn new(
        initial: NativeModelGeneration,
        compose: Arc<NativeModelComposer>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                generation: 0,
                replacing: false,
                children: 0,
                current: Arc::new(initial),
            }),
            compose,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn current(&self) -> Result<Arc<NativeModelGeneration>, AgentLoopError> {
        let state = self.lock();
        if state.replacing {
            return Err(busy());
        }
        Ok(Arc::clone(&state.current))
    }

    /// Close child admission atomically with checking every retained child
    /// generation. A failed preparation stays closed until explicit publication.
    pub(crate) fn begin_replacement(
        self: &Arc<Self>,
    ) -> Result<NativeModelReplacement, AgentLoopError> {
        let mut state = self.lock();
        if state.replacing || state.children != 0 {
            return Err(busy());
        }
        state.generation = state.generation.checked_add(1).ok_or_else(|| {
            AgentLoopError::InvalidConfiguration("model generation exhausted".into())
        })?;
        state.replacing = true;
        Ok(NativeModelReplacement {
            owner: Arc::clone(self),
            generation: state.generation,
        })
    }

    pub(crate) fn source(self: &Arc<Self>) -> Arc<dyn ModelSource> {
        let owner = Arc::downgrade(self);
        Arc::new(move || {
            let owner = owner.upgrade().ok_or(AgentLoopError::Closed)?;
            Ok(Arc::clone(&owner.current()?.provider))
        })
    }

    pub(super) fn child_source(self: &Arc<Self>) -> Weak<Self> {
        Arc::downgrade(self)
    }

    pub(super) fn retain(
        self: &Arc<Self>,
        resources: Arc<dyn rw_core::SessionResources>,
    ) -> Arc<dyn rw_core::SessionResources> {
        Arc::new(ModelGenerationResources {
            resources,
            _models: Arc::clone(self),
        })
    }
}

struct ModelGenerationResources {
    resources: Arc<dyn rw_core::SessionResources>,
    _models: Arc<NativeModelGenerations>,
}
#[async_trait]
impl rw_core::SessionResources for ModelGenerationResources {
    fn bind_session(&self, binding: rw_core::PluginSessionBinding) -> Result<(), AgentLoopError> {
        self.resources.bind_session(binding)
    }

    async fn shutdown(&self) -> Result<(), AgentLoopError> {
        self.resources.shutdown().await
    }
}

/// The caller must retire old extension effects before preparing/publishing a
/// candidate. This guard also prevents a child from racing that retirement.
pub(crate) struct NativeModelReplacement {
    owner: Arc<NativeModelGenerations>,
    generation: u64,
}
impl NativeModelReplacement {
    pub(crate) fn prepare(
        self,
        input: NativeModelInput,
    ) -> Result<PreparedNativeModel, AgentLoopError> {
        if input.roots.is_empty() || input.roots.iter().any(|root| !root.is_absolute()) {
            return Err(AgentLoopError::InvalidConfiguration(
                "model generation requires canonical roots".into(),
            ));
        }
        let candidate = Arc::new((self.owner.compose)(input)?);
        Ok(PreparedNativeModel {
            replacement: self,
            candidate,
        })
    }
}

pub(crate) struct PreparedNativeModel {
    replacement: NativeModelReplacement,
    candidate: Arc<NativeModelGeneration>,
}
impl PreparedNativeModel {
    pub(crate) fn model(&self) -> Arc<dyn ModelDriver> {
        Arc::clone(&self.candidate.model)
    }

    /// Publish immediately after the matching plugin gate resumes. All fallible
    /// work is complete and child admission remains closed until this swap.
    pub(crate) fn publish(self) {
        let mut state = self.replacement.owner.lock();
        // Only one replacement can exist, and no other operation changes this
        // identity while admission is closed.
        debug_assert_eq!(state.generation, self.replacement.generation);
        state.current = self.candidate;
        state.replacing = false;
    }
}

fn busy() -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(
        "model generation is retained by a child or replacement".into(),
    )
}

#[async_trait]
impl ModelCatalogSource for NativeModelGenerations {
    fn generation(&self) -> u64 {
        self.lock().generation
    }
    async fn discover(&self) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        let current = self
            .current()
            .map_err(|error| ModelCatalogError(error.to_string()))?;
        let source = current
            .catalog
            .as_ref()
            .ok_or_else(|| ModelCatalogError("model catalog is unavailable".into()))?;
        source.discover().await
    }
    async fn discover_provider(
        &self,
        provider: &str,
    ) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        let current = self
            .current()
            .map_err(|error| ModelCatalogError(error.to_string()))?;
        let source = current
            .catalog
            .as_ref()
            .ok_or_else(|| ModelCatalogError("model catalog is unavailable".into()))?;
        source.discover_provider(provider).await
    }
}
