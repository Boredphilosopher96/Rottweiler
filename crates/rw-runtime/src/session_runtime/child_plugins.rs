//! A child captures a private native plugin generation and session namespace.
use super::{durable_session::DurableEventSink, plugin_event_fanout::PluginFanoutEventSink};
use crate::{
    extension_runtime::{
        PluginSessionRuntime,
        generations::{PluginGenerationConfig, PluginGenerationOwner},
    },
    session_resources::RuntimeSessionResources,
};
use async_trait::async_trait;
use rw_core::{
    AgentLoopError, PluginSessionBinding, SessionCommandContext, SessionCommandOutput,
    SessionResources,
};
use rw_ext::{CommandRegistry, HookDispatcher};
use rw_tools::ToolRegistry;
use std::sync::Arc;

pub(super) struct ChildPlugins {
    owner: Arc<PluginGenerationOwner>,
    pub(super) runtime: Arc<PluginSessionRuntime>,
    resources: Arc<RuntimeSessionResources>,
}
impl ChildPlugins {
    pub(super) fn compose(
        configuration: &PluginGenerationConfig,
        configured: &[crate::extension_config::DiscoveredPlugin],
        roots: &[std::path::PathBuf],
    ) -> Result<Self, AgentLoopError> {
        let owner =
            PluginGenerationOwner::compose(configuration.child_session(), configured, roots)
                .map_err(|error| invalid(&error))?;
        // Install cleanup ownership before constructing any delivery workers.
        let resources = RuntimeSessionResources::native(owner.clone());
        Ok(Self {
            runtime: owner.current(),
            owner,
            resources,
        })
    }
    pub(super) fn tools(&self, registry: &mut Arc<ToolRegistry>) -> Result<(), AgentLoopError> {
        if self.runtime.tools.is_empty() {
            return Ok(());
        }
        let registry = Arc::make_mut(registry);
        for tool in &self.runtime.tools {
            registry
                .register(tool.clone())
                .map_err(|error| invalid(&error))?;
        }
        Ok(())
    }
    pub(super) fn hooks(&self, hooks: &mut HookDispatcher) -> Result<(), AgentLoopError> {
        for (registration, handler) in &self.runtime.hooks {
            hooks
                .register_shared(registration.clone(), handler.clone())
                .map_err(|error| invalid(&error))?;
        }
        Ok(())
    }
    pub(super) fn commands(
        &self,
        commands: &mut CommandRegistry<SessionCommandContext, SessionCommandOutput>,
    ) -> Result<(), AgentLoopError> {
        for (descriptor, handler) in &self.runtime.commands {
            commands
                .register_shared(descriptor.clone(), handler.clone())
                .map_err(|error| invalid(&error))?;
        }
        Ok(())
    }
    pub(super) fn delivery(
        &self,
        sink: Arc<DurableEventSink>,
        redactor: &rw_providers::FixtureRedactor,
    ) -> Result<Arc<PluginFanoutEventSink>, AgentLoopError> {
        let delivery = Arc::new(PluginFanoutEventSink::new(
            sink,
            self.runtime.event_routers.clone(),
            redactor,
        )?);
        self.owner
            .bind_delivery(delivery.clone())
            .map_err(|error| invalid(&error))?;
        Ok(delivery)
    }
    pub(super) fn resources(
        &self,
        parent_generation: Arc<dyn SessionResources>,
    ) -> Arc<dyn SessionResources> {
        let native = self.resources.clone();
        let binding = parent_generation.clone();
        let cleanup = RuntimeSessionResources::own_cleanup(
            (native.clone(), parent_generation.clone()),
            async move {
                let (native, parent) =
                    tokio::join!(native.shutdown(), parent_generation.shutdown());
                native
                    .and(parent)
                    .map_err(|error| Arc::<str>::from(error.to_string()))
            },
        );
        Arc::new(ChildResources {
            native: self.resources.clone(),
            binding,
            cleanup,
        })
    }
}
struct ChildResources {
    native: Arc<RuntimeSessionResources>,
    binding: Arc<dyn SessionResources>,
    cleanup: Arc<RuntimeSessionResources>,
}
#[async_trait]
impl SessionResources for ChildResources {
    fn bind_session(&self, binding: PluginSessionBinding) -> Result<(), AgentLoopError> {
        let native = self.native.bind_session(binding.clone());
        let parent = self.binding.bind_session(binding);
        native.and(parent)
    }
    async fn shutdown(&self) -> Result<(), AgentLoopError> {
        self.cleanup.shutdown().await
    }
}
fn invalid(error: &(impl ToString + ?Sized)) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(error.to_string())
}
