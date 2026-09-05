//! A session publishes every native extension adapter from one immutable generation.
use super::{
    PluginRuntimeBudget, PluginSessionRuntime, SessionPluginPushHandler, SharedPluginRedactor,
    activation, ui,
};
use crate::extension_config::DiscoveredPlugin;
use crate::session_runtime::plugin_event_fanout::{PluginFanoutEventSink, PreparedPluginDelivery};
use miette::{Result, miette};
use rw_ext::{
    PluginEndpoint, PluginEndpointMetadata,
    invocation::{ExclusiveInvocationGuard, ExtensionInvocations, PreparedExtensionGeneration},
};
use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};
use tokio::sync::{Mutex, OwnedMutexGuard};

pub(crate) struct PluginGenerationConfig {
    pub(crate) private_root: PathBuf,
    pub(crate) helper: PathBuf,
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
            closed: std::sync::atomic::AtomicBool::new(false),
        }))
    }
    pub(crate) fn current(&self) -> Arc<PluginSessionRuntime> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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
    ) -> Result<PreparedPluginGeneration> {
        let operation = self.operation.clone().lock_owned().await;
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(miette!("native extension session is closed"));
        }
        let batch = self
            .configuration
            .discover(configured, development, roots)?;
        let raw = batch
            .plugins
            .iter()
            .map(|plugin| plugin.raw.clone())
            .collect::<Vec<_>>();
        let delivery = self
            .delivery
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| miette!("native generation has no event delivery owner"))?;
        delivery
            .pause_and_settle()
            .await
            .map_err(|error| miette!(error.to_string()))?;
        let guard = self
            .invocations
            .pause_and_settle()
            .await
            .map_err(|error| miette!(error.to_string()))?;
        self.current().ui.close();
        let candidate = guard
            .prepare(&raw)
            .map_err(|error| miette!(error.to_string()))?;
        let managed = candidate
            .endpoints()
            .map_err(|error| miette!(error.to_string()))?;
        let runtime = self.configuration.runtime(batch, managed)?;
        let binding = self
            .binding
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| miette!("native generation has no session binding"))?;
        runtime.bind_generation(&binding)?;
        let delivery = delivery
            .prepare(runtime.event_routers.clone())
            .map_err(|error| miette!(error.to_string()))?;
        Ok(PreparedPluginGeneration {
            owner: self.clone(),
            runtime,
            delivery,
            guard,
            candidate,
            _operation: operation,
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
        let delivery_proof = if let Some(delivery) = delivery {
            delivery
                .pause_and_settle()
                .await
                .map_err(|error| miette!(error.to_string()))
        } else {
            Ok(())
        };
        let guard = self
            .invocations
            .pause_and_settle()
            .await
            .map_err(|error| miette!(error.to_string()))?;
        drop(guard);
        delivery_proof
    }
}

/// Keeps admission closed until the actor publishes its complete runtime boundary.
pub(crate) struct PreparedPluginGeneration {
    owner: Arc<PluginGenerationOwner>,
    pub(crate) runtime: Arc<PluginSessionRuntime>,
    delivery: PreparedPluginDelivery,
    guard: ExclusiveInvocationGuard,
    candidate: PreparedExtensionGeneration,
    _operation: OwnedMutexGuard<()>,
}
impl PreparedPluginGeneration {
    pub(crate) fn publish(self) -> Result<Arc<PluginSessionRuntime>> {
        *self
            .owner
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = self.runtime.clone();
        self.guard
            .resume(self.candidate)
            .map_err(|error| miette!(error.to_string()))?;
        self.delivery
            .publish()
            .map_err(|error| miette!(error.to_string()))?;
        Ok(self.runtime)
    }
}
