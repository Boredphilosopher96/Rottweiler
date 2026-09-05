//! Declarative registrations and live plugin connections have separate owners.
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use rw_plugin_protocol::{
    PluginCapabilities, PluginManifest, PluginToolCapability, PluginToolEffect,
};
use rw_tools::CancellationToken;

use crate::{CapabilityEnforcer, PluginHost, PluginRpcClient, PluginRpcError};

/// Validated declarations available before any source preparation or process start.
/// These declarations describe required authority; they do not grant it.
#[derive(Clone)]
pub struct PluginEndpointMetadata {
    manifest: Arc<PluginManifest>,
    process_effects: BTreeSet<PluginToolEffect>,
    ui_generation: rw_types::extension_ui::UiGenerationId,
}

impl PluginEndpointMetadata {
    /// Parses an inert registration snapshot. Launch approval remains mandatory.
    ///
    /// # Errors
    /// Rejects an invalid manifest before it enters the extension registries.
    pub fn new(manifest: PluginManifest) -> Result<Self, PluginRpcError> {
        manifest.validate().map_err(|error| PluginRpcError {
            code: "invalid_manifest".to_owned(),
            message: error.to_string(),
        })?;
        let process_effects = declared_process_effects(&manifest.capabilities);
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).map_err(|_| PluginRpcError {
            code: "generation_unavailable".into(),
            message: "plugin generation entropy unavailable".into(),
        })?;
        let ui_generation = rw_types::extension_ui::UiGenerationId::from_bytes(bytes);
        Ok(Self {
            manifest: Arc::new(manifest),
            process_effects,
            ui_generation,
        })
    }

    #[must_use]
    pub fn ui_owner(&self) -> rw_types::extension_ui::UiContributionOwner {
        rw_types::extension_ui::UiContributionOwner {
            extension: self.manifest.name.clone(),
            generation: self.ui_generation.clone(),
        }
    }

    #[must_use]
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    #[must_use]
    pub fn process_tool_effects(&self) -> &BTreeSet<PluginToolEffect> {
        &self.process_effects
    }

    #[must_use]
    pub fn tool_declaration_matches(&self, declaration: &PluginToolCapability) -> bool {
        self.manifest
            .capabilities
            .tools
            .iter()
            .any(|expected| expected == declaration)
    }
}

/// A connection published only after approval, launch and initialization succeed.
/// The capability enforcer always belongs to this connection's actual process.
#[derive(Clone)]
pub struct PluginConnection {
    client: Arc<dyn PluginRpcClient>,
    enforcer: Arc<CapabilityEnforcer>,
    effect_domains: Arc<[String]>,
    continuation_provenance: rw_providers::ContinuationProvenance,
}

impl PluginConnection {
    pub(crate) fn with_client(mut self, client: Arc<dyn PluginRpcClient>) -> Self {
        self.client = client;
        self
    }

    #[must_use]
    pub fn from_host(host: &PluginHost) -> Self {
        Self {
            client: host.client(),
            enforcer: host.enforcer(),
            effect_domains: host.effect_domains(),
            continuation_provenance: host.continuation_provenance().clone(),
        }
    }

    #[must_use]
    pub fn client(&self) -> &Arc<dyn PluginRpcClient> {
        &self.client
    }

    #[must_use]
    pub fn enforcer(&self) -> &Arc<CapabilityEnforcer> {
        &self.enforcer
    }
    pub(crate) fn effect_domains(&self) -> &[String] {
        &self.effect_domains
    }

    #[must_use]
    pub fn continuation_provenance(&self) -> &rw_providers::ContinuationProvenance {
        &self.continuation_provenance
    }
}

/// Owns one immutable extension generation from registration through closure.
///
/// Implementations must own admitted activation independently of this future.
/// Dropping a connection waiter revokes that activation, and `settle_effects`
/// must report its local cleanup outcome. A failed generation cannot restart.
#[async_trait]
pub trait PluginEndpoint: Send + Sync {
    fn metadata(&self) -> &PluginEndpointMetadata;

    /// Obtains an initialized connection within the generation's fixed deadline.
    async fn connect(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<PluginConnection, PluginRpcError>;

    /// Proves settlement for cancelled or dropped invocations and activation.
    async fn settle_effects(&self) -> Result<(), PluginRpcError>;

    /// Permanently closes admission and proves all locally owned effects settled.
    async fn close(&self) -> Result<(), PluginRpcError>;
}

/// An explicitly launched host, such as a development attachment.
/// Dormant configured extensions use their own activation owner in the runtime.
pub struct ReadyPluginEndpoint {
    metadata: PluginEndpointMetadata,
    host: Arc<PluginHost>,
    closed: AtomicBool,
}

impl ReadyPluginEndpoint {
    /// Takes ownership of an already initialized host.
    ///
    /// # Errors
    /// Rejects invalid initialized metadata.
    pub fn new(host: Arc<PluginHost>) -> Result<Self, PluginRpcError> {
        let metadata = PluginEndpointMetadata::new(host.manifest().clone())?;
        Ok(Self {
            metadata,
            host,
            closed: AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl PluginEndpoint for ReadyPluginEndpoint {
    fn metadata(&self) -> &PluginEndpointMetadata {
        &self.metadata
    }

    async fn connect(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<PluginConnection, PluginRpcError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PluginRpcError {
                code: "closed".to_owned(),
                message: "plugin endpoint is closed".to_owned(),
            });
        }
        if cancellation.is_cancelled() {
            return Err(PluginRpcError {
                code: "cancelled".to_owned(),
                message: "plugin connection was cancelled".to_owned(),
            });
        }
        Ok(PluginConnection::from_host(&self.host))
    }

    async fn settle_effects(&self) -> Result<(), PluginRpcError> {
        self.host.client().settle_effects().await
    }

    async fn close(&self) -> Result<(), PluginRpcError> {
        self.closed.store(true, Ordering::Release);
        self.host.shutdown().await.map_err(|error| PluginRpcError {
            code: "effects_unsettled".to_owned(),
            message: error.to_string(),
        })
    }
}

pub(crate) fn declared_process_effects(
    capabilities: &PluginCapabilities,
) -> BTreeSet<PluginToolEffect> {
    let mut effects = capabilities
        .tools
        .iter()
        .flat_map(|tool| tool.caps.iter().copied())
        .collect::<BTreeSet<_>>();
    if !capabilities.providers.is_empty() {
        effects.insert(PluginToolEffect::Network);
    }
    effects
}

#[cfg(test)]
pub(crate) fn fixture_endpoint(
    manifest: PluginManifest,
    client: Arc<dyn PluginRpcClient>,
    enforcer: Arc<CapabilityEnforcer>,
) -> Arc<dyn PluginEndpoint> {
    #[allow(clippy::expect_used)]
    let metadata = PluginEndpointMetadata::new(manifest).expect("fixture manifest");
    Arc::new(FixtureEndpoint {
        metadata,
        connection: PluginConnection {
            effect_domains: Arc::from([]),
            client,
            enforcer,
            continuation_provenance: rw_providers::ContinuationProvenance::bind(&[b"fixture"]),
        },
    })
}

#[cfg(test)]
struct FixtureEndpoint {
    metadata: PluginEndpointMetadata,
    connection: PluginConnection,
}
#[cfg(test)]
#[async_trait]
impl PluginEndpoint for FixtureEndpoint {
    fn metadata(&self) -> &PluginEndpointMetadata {
        &self.metadata
    }
    async fn connect(&self, _: &CancellationToken) -> Result<PluginConnection, PluginRpcError> {
        Ok(self.connection.clone())
    }
    async fn settle_effects(&self) -> Result<(), PluginRpcError> {
        self.connection.client.settle_effects().await
    }
    async fn close(&self) -> Result<(), PluginRpcError> {
        self.settle_effects().await
    }
}

#[cfg(test)]
mod tests;
