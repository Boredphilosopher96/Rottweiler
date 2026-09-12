//! A session publishes every native extension adapter from one immutable generation.
use super::{
    PluginRuntimeBudget, PluginSessionRuntime, SessionPluginPushHandler, SharedPluginRedactor,
    activation, ui,
};
use crate::extension_config::DiscoveredPlugin;
use crate::session_runtime::native_model_generations::{
    NativeModelGenerations, NativeModelInput, NativeModelReplacement, PreparedNativeModel,
};
use crate::session_runtime::plugin_event_fanout::{PluginFanoutEventSink, PreparedPluginDelivery};
use miette::{Result, miette};
use rw_core::AgentLoopError;
use rw_ext::{
    PluginEndpoint, PluginEndpointMetadata,
    invocation::{ExclusiveInvocationGuard, ExtensionInvocations, PreparedExtensionGeneration},
};
use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};
use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Clone)]
pub(crate) struct PluginGenerationConfig {
    pub(crate) private_root: PathBuf,
    pub(crate) helper: Arc<crate::extension_runtime::SandboxHelperSource>,
    pub(crate) redactor: Arc<SharedPluginRedactor>,
    pub(crate) budget: Arc<PluginRuntimeBudget>,
    pub(crate) session_ui: Arc<ui::UiSessionBudget>,
}

struct PreparedPlugin {
    config: DiscoveredPlugin,
    raw: Arc<dyn PluginEndpoint>,
    handler: Arc<SessionPluginPushHandler>,
}
struct PluginBatch {
    plugins: Vec<PreparedPlugin>,
    pending: Vec<String>,
}
impl PluginGenerationConfig {
    pub(crate) fn child_session(&self) -> Self {
        Self {
            session_ui: Arc::new(ui::UiSessionBudget::default()),
            ..self.clone()
        }
    }
    fn discover(
        &self,
        configured: &[DiscoveredPlugin],
        development: Option<&DiscoveredPlugin>,
        roots: &[PathBuf],
    ) -> Result<PluginBatch> {
        if configured.iter().filter(|config| config.enabled).count()
            + usize::from(development.is_some())
            > rw_ext::invocation::MAX_EXTENSION_ENDPOINTS
        {
            return Err(miette!("session native extension count exceeds admission"));
        }
        let mut batch = PluginBatch {
            plugins: Vec::new(),
            pending: Vec::new(),
        };
        for (config, approval) in configured
            .iter()
            .filter(|config| config.enabled)
            .map(|config| (config, activation::ActivationApproval::Configured))
            .chain(
                development
                    .map(|config| (config, activation::ActivationApproval::SessionDevelopment)),
            )
        {
            let manifest = match config.load_manifest() {
                Ok(manifest) => manifest,
                Err(error) => {
                    batch
                        .pending
                        .push(format!("{}: unavailable: {error}", config.name));
                    continue;
                }
            };
            let metadata = PluginEndpointMetadata::new(manifest)
                .map_err(|error| miette!(error.to_string()))?;
            let handler = Arc::new(SessionPluginPushHandler::default());
            let raw = Arc::new(activation::DormantPluginEndpoint::new(
                activation::ActivationRecipe {
                    metadata,
                    approval,
                    config: config.clone(),
                    private_root: self.private_root.clone(),
                    workspace_roots: roots.to_vec(),
                    helper: self.helper.clone(),
                    redactor: self.redactor.clone(),
                    push_handler: handler.clone(),
                    budget: self.budget.clone(),
                    #[cfg(test)]
                    launcher: None,
                },
            ));
            batch.plugins.push(PreparedPlugin {
                config: config.clone(),
                raw,
                handler,
            });
        }
        Ok(batch)
    }
    fn runtime(
        &self,
        batch: PluginBatch,
        managed: Vec<Arc<dyn PluginEndpoint>>,
    ) -> Result<Arc<PluginSessionRuntime>> {
        if batch.plugins.len() != managed.len() {
            return Err(miette!("native generation endpoint cardinality differs"));
        }
        let mut runtime =
            PluginSessionRuntime::new(&self.budget, &self.redactor, self.session_ui.clone());
        runtime.pending = batch.pending;
        for (plugin, endpoint) in batch.plugins.into_iter().zip(managed) {
            let manifest = endpoint.metadata().manifest().clone();
            runtime.register_endpoint(&plugin.config, &manifest, endpoint, plugin.handler)?;
        }
        Ok(Arc::new(runtime))
    }
}

pub(crate) struct PluginGenerationOwner {
    pub(crate) invocations: Arc<ExtensionInvocations>,
    configuration: PluginGenerationConfig,
    current: RwLock<Arc<PluginSessionRuntime>>,
    binding: RwLock<Option<rw_core::PluginSessionBinding>>,
    operation: Arc<Mutex<()>>,
    delivery: RwLock<Option<Arc<PluginFanoutEventSink>>>,
    models: RwLock<Option<Arc<NativeModelGenerations>>>,
    closed: std::sync::atomic::AtomicBool,
}
impl PluginGenerationOwner {
    pub(crate) fn compose(
        configuration: PluginGenerationConfig,
        configured: &[DiscoveredPlugin],
        roots: &[PathBuf],
    ) -> Result<Arc<Self>> {
        let batch = configuration.discover(configured, None, roots)?;
        let raw = batch
            .plugins
            .iter()
            .map(|plugin| plugin.raw.clone())
            .collect::<Vec<_>>();
        let invocations =
            ExtensionInvocations::new(&raw).map_err(|error| miette!(error.to_string()))?;
        let managed = invocations
            .endpoints()
            .map_err(|error| miette!(error.to_string()))?;
        let current = configuration.runtime(batch, managed)?;
        Ok(Arc::new(Self {
            invocations,
            configuration,
            current: RwLock::new(current),
            binding: RwLock::new(None),
            operation: Arc::new(Mutex::new(())),
            delivery: RwLock::new(None),
            models: RwLock::new(None),
            closed: std::sync::atomic::AtomicBool::new(false),
        }))
    }
    pub(crate) fn child_configuration(&self) -> Arc<PluginGenerationConfig> {
        Arc::new(self.configuration.child_session())
    }
    pub(crate) fn current(&self) -> Arc<PluginSessionRuntime> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub(crate) fn bind_models(&self, models: Arc<NativeModelGenerations>) -> Result<()> {
        let mut slot = self
            .models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return Err(miette!("native model owner is already bound"));
        }
        *slot = Some(models);
        Ok(())
    }
    pub(crate) fn bind_delivery(&self, delivery: Arc<PluginFanoutEventSink>) -> Result<()> {
        let mut slot = self
            .delivery
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return Err(miette!("native delivery owner is already bound"));
        }
        *slot = Some(delivery);
        Ok(())
    }
    pub(crate) fn bind(&self, binding: rw_core::PluginSessionBinding) -> Result<()> {
        self.current().bind_generation(&binding)?;
        *self
            .binding
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(binding);
        Ok(())
    }
    pub(crate) async fn prepare(
        self: &Arc<Self>,
        configured: &[DiscoveredPlugin],
        development: Option<&DiscoveredPlugin>,
        roots: &[PathBuf],
    ) -> std::result::Result<PreparedPluginGeneration, AgentLoopError> {
        let operation = self.operation.clone().lock_owned().await;
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(AgentLoopError::Closed);
        }
        let batch = self
            .configuration
            .discover(configured, development, roots)
            .map_err(|error| invalid(&error))?;
        let raw = batch
            .plugins
            .iter()
            .map(|plugin| plugin.raw.clone())
            .collect::<Vec<_>>();
        let binding = self
            .binding
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| invalid("native generation has no session binding"))?;
        let delivery = self
            .delivery
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| invalid("native generation has no event delivery owner"))?;
        let models = self
            .models
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| invalid("native generation has no model owner"))?;
        // Child admission and the retained-generation check are one transaction.
        // Do not retire a plugin process until that guard is held.
        let model = models.begin_replacement()?;
        let (delivery_proof, native_proof) = tokio::join!(
            delivery.pause_and_settle(),
            self.invocations.pause_and_settle(),
        );
        // Both actual owners are asked for proof even if either one fails.
        let guard = native_proof.map_err(|error| unsettled(&error))?;
        delivery_proof.map_err(|error| unsettled(&error))?;
        self.current().ui.close();
        let candidate = guard.prepare(&raw).map_err(|error| unsettled(&error))?;
        let managed = candidate.endpoints().map_err(|error| unsettled(&error))?;
        let runtime = self
            .configuration
            .runtime(batch, managed)
            .map_err(|error| unsettled(&error))?;
        runtime
            .bind_generation(&binding)
            .map_err(|error| unsettled(&error))?;
        let delivery = delivery
            .prepare(runtime.event_routers.clone())
            .map_err(|error| unsettled(&error))?;
        Ok(PreparedPluginGeneration {
            owner: self.clone(),
            runtime,
            delivery,
            guard,
            candidate,
            model,
            operation,
        })
    }
    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        let _operation = self.operation.lock().await;
        self.current().ui.close();
        let delivery = self
            .delivery
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let delivery_proof = async {
            if let Some(delivery) = delivery {
                delivery
                    .pause_and_settle()
                    .await
                    .map_err(|error| miette!(error.to_string()))
            } else {
                Ok(())
            }
        };
        let (delivery, native) = tokio::join!(delivery_proof, self.invocations.pause_and_settle());
        let guard = native.map_err(|error| miette!(error.to_string()))?;
        drop(guard);
        delivery
    }
}

/// Keeps admission closed until the actor publishes its complete runtime boundary.
pub(crate) struct PreparedPluginGeneration {
    owner: Arc<PluginGenerationOwner>,
    pub(crate) runtime: Arc<PluginSessionRuntime>,
    delivery: PreparedPluginDelivery,
    guard: ExclusiveInvocationGuard,
    candidate: PreparedExtensionGeneration,
    model: NativeModelReplacement,
    operation: OwnedMutexGuard<()>,
}
impl PreparedPluginGeneration {
    pub(crate) fn with_model(
        self,
        mut input: NativeModelInput,
    ) -> std::result::Result<PreparedNativePublication, AgentLoopError> {
        input.providers.clone_from(&self.runtime.providers);
        let model = self
            .model
            .prepare(input)
            .map_err(|error| unsettled(&error))?;
        Ok(PreparedNativePublication {
            owner: self.owner,
            runtime: self.runtime,
            delivery: self.delivery,
            guard: self.guard,
            candidate: self.candidate,
            model,
            _operation: self.operation,
        })
    }
}

pub(crate) struct PreparedNativePublication {
    owner: Arc<PluginGenerationOwner>,
    runtime: Arc<PluginSessionRuntime>,
    delivery: PreparedPluginDelivery,
    guard: ExclusiveInvocationGuard,
    candidate: PreparedExtensionGeneration,
    model: PreparedNativeModel,
    _operation: OwnedMutexGuard<()>,
}
impl PreparedNativePublication {
    pub(crate) fn model(&self) -> Arc<dyn rw_core::ModelDriver> {
        self.model.model()
    }
    pub(crate) fn publication(
        self,
        orchestrator: rw_core::SubagentOrchestrator,
        tools: Arc<rw_tools::ToolRegistry>,
    ) -> rw_core::RuntimePublication {
        rw_core::RuntimePublication::Prepared(Arc::new(Publication(std::sync::Mutex::new(Some((
            self,
            orchestrator,
            tools,
        ))))))
    }
    fn publish(
        self,
        orchestrator: &rw_core::SubagentOrchestrator,
        tools: Arc<rw_tools::ToolRegistry>,
    ) -> std::result::Result<(), AgentLoopError> {
        if self.owner.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(AgentLoopError::Closed);
        }
        self.delivery.publish_with(|| {
            orchestrator.bind_tools(tools);
            self.guard
                .resume(self.candidate)
                .map_err(|error| unsettled(&error))?;
            *self
                .owner
                .current
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = self.runtime;
            // No fallible operation may follow reopening child admission.
            self.model.publish();
            Ok(())
        })
    }
}
type PublicationParts = (
    PreparedNativePublication,
    rw_core::SubagentOrchestrator,
    Arc<rw_tools::ToolRegistry>,
);
struct Publication(std::sync::Mutex<Option<PublicationParts>>);
impl rw_core::PreparedRuntimePublication for Publication {
    fn publish(&self) -> std::result::Result<(), AgentLoopError> {
        let (prepared, orchestrator, tools) = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| unsettled("native generation publication was already consumed"))?;
        prepared.publish(&orchestrator, tools)
    }
}
fn invalid(error: &(impl ToString + ?Sized)) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(error.to_string())
}
fn unsettled(error: &(impl ToString + ?Sized)) -> AgentLoopError {
    AgentLoopError::EffectsUnsettled(error.to_string())
}
