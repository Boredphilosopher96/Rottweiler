//! Reusable assembly boundary for headless Rottweiler applications.
//!
//! `rw-core` owns the provider- and presentation-neutral engine. This crate
//! owns the explicit wiring surface used by executable frontends and SDKs.

use std::sync::Arc;

use rw_core::{EngineHost, EngineHostConfig, HostError, HostQueryService, SessionFactory};

mod extension_config;
mod extension_runtime;
mod history;
mod mode_recovery;
mod plugin_process;
mod project_commands;
mod session_host;
mod session_runtime;
mod source_plugin;
mod storage_root;
mod subagent_metadata;
mod workflow_runtime;

pub use extension_runtime::PrivatePluginApprovalStore;
pub use session_host::{RuntimeHostOptions, RuntimeSessionFactory};

/// Durable session replay, search, and export APIs.
pub mod session_history {
    pub use crate::history::{
        MAX_HISTORY_BYTES, MAX_HISTORY_EVENTS, export_transcript, list_sessions, load_events,
        load_events_with_size, replay_jsonl, search_sessions, write_transcript_export,
    };
}

/// Trusted discovery types for executable MCP and plugin configuration.
pub mod executable_config {
    pub use crate::extension_config::{
        ContentAttestation, CredentialBinding, DiscoveredMcpServer, DiscoveredMcpTransport,
        DiscoveredPlugin, DiscoveredPluginTarget, ExecutableConfigCatalog, ExecutableConfigOrigin,
        discover_executable_configs,
    };
}

/// Intentional session-runtime surface consumed by headless frontends.
pub mod session {
    pub use crate::session_runtime::{
        HostedProviderMode, RunAction, RunOptions, discover_model_catalog,
        discover_runtime_extensions, extension_user_roots, initialize_private_storage_root,
        load_inherited_accounting_boundary_bounded, locate_wasm_host_executable, new_session_id,
        register_credential_environment, run, select_interactive_session,
    };
}

/// Sandboxed extension-process launch boundary.
pub mod plugin {
    pub use crate::plugin_process::SandboxedPluginLauncher;
    pub use crate::source_plugin::resolve_plugin_process;
}

/// Provider-output rendering selected by a non-interactive runtime client.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    StreamJson,
}

#[cfg(unix)]
pub(crate) fn rustix_device_id<T: TryInto<u64>>(device: T) -> Option<u64> {
    device.try_into().ok()
}

#[cfg(unix)]
pub(crate) fn rustix_mode_bits<T: Into<u32>>(mode: T) -> u32 {
    mode.into()
}

/// Builds one bounded headless engine host from an injected session factory.
///
/// The same factory supplies durable session composition and remote-safe
/// queries. Transport and presentation remain outside this type, so the
/// returned host can back the CLI server, print mode, MCP, or an SDK client.
pub struct HeadlessRuntimeBuilder<F> {
    config: EngineHostConfig,
    factory: Arc<F>,
}

impl<F> HeadlessRuntimeBuilder<F>
where
    F: SessionFactory + HostQueryService,
{
    #[must_use]
    pub fn new(factory: Arc<F>) -> Self {
        Self {
            config: EngineHostConfig::default(),
            factory,
        }
    }

    #[must_use]
    pub const fn with_config(mut self, config: EngineHostConfig) -> Self {
        self.config = config;
        self
    }

    /// Constructs the reusable protocol host.
    ///
    /// # Errors
    ///
    /// Returns [`HostError`] when a configured host capacity is zero.
    pub fn build(self) -> Result<EngineHost, HostError> {
        let sessions: Arc<dyn SessionFactory> = self.factory.clone();
        let queries: Arc<dyn HostQueryService> = self.factory;
        EngineHost::new(self.config, sessions, queries)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use rw_core::{
        CommandDescriptor, CreateSessionRequest, EngineHostConfig, HostError, HostQueryService,
        ModelCatalogSnapshot, SessionDescriptor, SessionFactory, WorkspaceDiff, WorkspaceFileMatch,
        WorkspaceFilePreview, WorkspaceStatus,
    };

    use super::HeadlessRuntimeBuilder;

    struct EmptyFactory;

    #[async_trait]
    impl SessionFactory for EmptyFactory {
        fn allocate_session_id(&self) -> Result<rw_core::SessionId, HostError> {
            Ok(rw_core::SessionId("empty".to_owned()))
        }

        async fn create(
            &self,
            _request: CreateSessionRequest,
        ) -> Result<rw_core::HostedSession, HostError> {
            Err(HostError::Protocol("not configured".to_owned()))
        }

        async fn resume(
            &self,
            _session_id: &rw_core::SessionId,
        ) -> Result<rw_core::HostedSession, HostError> {
            Err(HostError::Protocol("not configured".to_owned()))
        }
    }

    #[async_trait]
    impl HostQueryService for EmptyFactory {
        async fn command_descriptors(&self) -> Result<Vec<CommandDescriptor>, HostError> {
            Ok(Vec::new())
        }

        async fn model_catalog(
            &self,
            _refresh: bool,
            _selected_model: Option<&str>,
            _resolved_model: Option<&str>,
        ) -> Result<ModelCatalogSnapshot, HostError> {
            Err(HostError::Query("not configured".to_owned()))
        }

        async fn search_workspace_files(
            &self,
            _session: &SessionDescriptor,
            _query: &str,
            _limit: u32,
        ) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
            Ok((Vec::new(), false))
        }

        async fn preview_workspace_file(
            &self,
            _session: &SessionDescriptor,
            _path: &str,
            _max_bytes: u32,
        ) -> Result<WorkspaceFilePreview, HostError> {
            Err(HostError::Query("not configured".to_owned()))
        }

        async fn workspace_status(
            &self,
            _session: &SessionDescriptor,
        ) -> Result<WorkspaceStatus, HostError> {
            Err(HostError::Query("not configured".to_owned()))
        }

        async fn workspace_diff(
            &self,
            _session: &SessionDescriptor,
            _path: &str,
            _max_bytes: u32,
        ) -> Result<WorkspaceDiff, HostError> {
            Err(HostError::Query("not configured".to_owned()))
        }
    }

    #[test]
    fn builder_rejects_zero_capacity_before_any_session_work() {
        let result = HeadlessRuntimeBuilder::new(Arc::new(EmptyFactory))
            .with_config(EngineHostConfig {
                max_sessions: 0,
                max_deduplicated_requests: 1,
            })
            .build();
        assert!(result.is_err());
    }
}
