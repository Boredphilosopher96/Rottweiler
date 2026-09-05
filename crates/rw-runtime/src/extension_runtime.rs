//! Host integration helpers for MCP runtime control and RPC plugin approval.

mod mcp_commands;
pub(crate) use mcp_commands::*;

mod mcp_service;
pub(crate) use mcp_service::*;

mod activation;
mod budget;
pub(crate) mod generations;
pub(crate) mod ui;
pub(crate) use budget::PluginRuntimeBudget;
pub(crate) mod delivery_budget;
pub(crate) use delivery_budget::PluginDeliveryBudget;
mod event_source;
pub(crate) use event_source::{PluginEventSource, PluginEventSources};

mod development;
pub(crate) use development::*;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use futures_util::StreamExt as _;
use miette::{IntoDiagnostic, Result, miette};
use rw_core::{
    HostError, HostMcpService, LoopbackMcpAuthority, McpApprovalReview, McpEnvironmentEntry,
    McpPolicyProxy, McpServerDescriptor, McpServerState, ProductionMcpHttpClient,
    ProductionMcpHttpConnector, SessionCommandAction, SessionCommandContext, SessionCommandOutput,
    ToonMcpEncoder, VaultMcpTokenProvider,
};
use rw_ext::{
    ApprovalStore, ApprovalStoreError, HookHandler, HookRegistration, PluginBoundaryRedactor,
    PluginEventRouter, PluginHttpStreamResponse, PluginProviderHttpHandler, PluginRpcError,
    PushHandler, RpcCommandAdapter, RpcHookHandler, RpcProviderAdapter, RpcToolAdapter,
    plugin_hook_registration,
};
use rw_ext::{
    CommandDescriptor, CommandExecutionError, CommandHandler, CommandInvocation, CommandRegistry,
    CommandRegistryError,
};
use rw_mcp::{
    FilesystemSpool, McpClient, McpConnectionApprovalPolicy, McpConnector, McpError, McpLimits,
    McpManager, McpServerConfig, McpTransportConfig, OverflowSpool, ServerState,
};
use rw_plugin_protocol::{
    METHOD_EVENT_READ, METHOD_EXTENSION_STATE_COMMIT, METHOD_EXTENSION_STATE_READ,
    METHOD_SESSION_CONTEXT_READ, METHOD_SESSION_CONTROL, METHOD_SESSION_INJECT_MESSAGE,
    METHOD_SESSION_QUERY, METHOD_SESSION_SET_STATUS, METHOD_UI_NOTIFY, PluginManifest,
};
use rw_store::config::ConfigLoader;
use rw_store::credentials::{CredentialManager, CredentialReference};
use rw_tools::{
    CancellationToken, EgressPolicy, SandboxedProtocolLauncher, SupervisedEgressProxy, Tool,
    UpstreamProxy,
};
use rw_types::{Block, CommandSource, McpServerId, Role, Turn, TurnMeta};
use serde::{Deserialize, Serialize};

use crate::extension_config::DiscoveredMcpServer;

const MAX_CONTROL_OUTPUT: usize = 32 * 1024;
const APPROVAL_VERSION: u16 = 1;

pub(crate) struct PrivateMcpScratch {
    path: PathBuf,
}
impl PrivateMcpScratch {
    pub(crate) fn create() -> Result<Self> {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random)
            .map_err(|error| miette!("MCP scratch entropy failed: {error}"))?;
        let requested = std::env::temp_dir().join(format!(
            "rottweiler-mcp-{}-{}",
            std::process::id(),
            u64::from_ne_bytes(random)
        ));
        fs::create_dir(&requested).into_diagnostic()?;
        let path = fs::canonicalize(&requested).into_diagnostic()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).into_diagnostic()?;
        }
        Ok(Self { path })
    }
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for PrivateMcpScratch {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path=%self.path.display(),%error,"MCP scratch cleanup failed");
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginHttpParams {
    alias: String,
    credential_reference: String,
    request: PluginHttpRequest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginHttpRequest {
    method: PluginHttpMethod,
    url: String,
    #[serde(default)]
    headers: Vec<PluginHttpHeader>,
    body_base64: String,
    credential_header: String,
    #[serde(default)]
    credential_prefix: String,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum PluginHttpMethod {
    Get,
    Post,
    Delete,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginHttpHeader {
    name: String,
    value: String,
}

struct RuntimePluginProviderHttp {
    credentials: Arc<CredentialManager>,
    registrar: Arc<dyn rw_providers::KnownSecretRegistrar>,
    proxy: SupervisedEgressProxy,
    allowed_domains: BTreeSet<String>,
}

impl RuntimePluginProviderHttp {
    fn new(
        credentials_path: &Path,
        allowed_domains: &[String],
        registrar: Arc<dyn rw_providers::KnownSecretRegistrar>,
    ) -> Result<Self> {
        let proxy = SupervisedEgressProxy::start(EgressPolicy::new(allowed_domains))
            .map_err(|error| miette!(error.to_string()))?;
        Ok(Self {
            credentials: Arc::new(CredentialManager::system(credentials_path)),
            registrar,
            proxy,
            allowed_domains: allowed_domains.iter().cloned().collect(),
        })
    }

    fn domain_allowed(&self, url: &url::Url) -> bool {
        plugin_http_domain_allowed(&self.allowed_domains, url)
    }
}

fn plugin_http_domain_allowed(allowed_domains: &BTreeSet<String>, url: &url::Url) -> bool {
    url.host_str().is_some_and(|host| {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        allowed_domains.iter().any(|allowed| {
            host == *allowed
                || host
                    .strip_suffix(allowed)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
    })
}

#[async_trait]
impl PluginProviderHttpHandler for RuntimePluginProviderHttp {
    async fn request(
        &self,
        params: serde_json::Value,
        cancellation: &CancellationToken,
    ) -> std::result::Result<PluginHttpStreamResponse, PluginRpcError> {
        let params: PluginHttpParams = serde_json::from_value(params).map_err(|_| {
            plugin_http_error("invalid_request", "provider HTTP request is invalid")
        })?;
        let _ = params.alias;
        let url = url::Url::parse(&params.request.url)
            .map_err(|_| plugin_http_error("invalid_request", "provider HTTP URL is invalid"))?;
        if !self.domain_allowed(&url) {
            return Err(plugin_http_error(
                "domain_denied",
                "provider HTTP URL is outside the plugin allowed_domains policy",
            ));
        }
        let body = BASE64_STANDARD
            .decode(params.request.body_base64.as_bytes())
            .map_err(|_| plugin_http_error("invalid_request", "provider HTTP body is invalid"))?;
        let mut headers = params
            .request
            .headers
            .into_iter()
            .map(|header| (header.name, header.value))
            .collect::<Vec<_>>();
        if headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(&params.request.credential_header))
        {
            return Err(plugin_http_error(
                "invalid_request",
                "provider HTTP credential header cannot also be plugin-supplied",
            ));
        }
        let resolved = self
            .credentials
            .resolve(&CredentialReference::new(&params.credential_reference))
            .map_err(|_| {
                plugin_http_error(
                    "authentication",
                    "provider HTTP credential reference could not be resolved",
                )
            })?;
        let secret = rw_providers::Secret::new(resolved.secret().expose_secret().clone());
        self.registrar.register(&secret);
        headers.push((
            params.request.credential_header,
            format!(
                "{}{}",
                params.request.credential_prefix,
                secret.expose_secret()
            ),
        ));
        let method = match params.request.method {
            PluginHttpMethod::Get => rw_providers::GuardedHttpMethod::Get,
            PluginHttpMethod::Post => rw_providers::GuardedHttpMethod::Post,
            PluginHttpMethod::Delete => rw_providers::GuardedHttpMethod::Delete,
        };
        let guarded = rw_providers::GuardedHttpRequest {
            method,
            url,
            headers,
            body,
            proxy: url::Url::parse(&self.proxy.url()).ok(),
            proxy_authentication: None,
            dns_pin: None,
            allow_private_destinations: false,
            response_deadline: Duration::from_mins(5),
            frame_deadline: Duration::from_secs(30),
            max_frame_bytes: 256 * 1024,
            max_body_bytes: 64 * 1024 * 1024,
        };
        let response = tokio::select! {
            () = cancellation.cancelled() => {
                return Err(plugin_http_error("cancelled", "provider HTTP request was cancelled"));
            }
            response = rw_providers::guarded_http_request(guarded) => response,
        }
        .map_err(|error| plugin_http_guard_error(&error))?;
        Ok(PluginHttpStreamResponse {
            status: response.status,
            headers: response.headers,
            body: Box::pin(
                response
                    .body
                    .map(|chunk| chunk.map_err(|error| plugin_http_guard_error(&error))),
            ),
        })
    }
}

fn plugin_http_guard_error(error: &rw_providers::GuardedHttpFetchError) -> PluginRpcError {
    let code = match &error {
        rw_providers::GuardedHttpFetchError::Provider(error) => match error.kind {
            rw_providers::ProviderErrorKind::EffectsUnsettled => "effects_unsettled",
            rw_providers::ProviderErrorKind::Authentication => "provider_http_authentication",
            rw_providers::ProviderErrorKind::RateLimited => "provider_http_rate_limited",
            rw_providers::ProviderErrorKind::Timeout => "provider_http_timeout",
            rw_providers::ProviderErrorKind::Server => "provider_http_server",
            rw_providers::ProviderErrorKind::Network => "provider_http_network",
            rw_providers::ProviderErrorKind::NetworkDisabled => "provider_http_network_disabled",
            rw_providers::ProviderErrorKind::Cancelled => "provider_http_cancelled",
            rw_providers::ProviderErrorKind::InvalidRequest
            | rw_providers::ProviderErrorKind::ContextOverflow
            | rw_providers::ProviderErrorKind::Protocol
            | rw_providers::ProviderErrorKind::ReplayMiss
            | rw_providers::ProviderErrorKind::Unsupported => "provider_http_invalid_request",
        },
        rw_providers::GuardedHttpFetchError::Deadline => "provider_http_timeout",
        rw_providers::GuardedHttpFetchError::SizeLimit { .. }
        | rw_providers::GuardedHttpFetchError::FrameLimit { .. } => "provider_http_protocol",
    };
    plugin_http_error(code, &error.to_string())
}

fn plugin_http_error(code: &str, message: &str) -> PluginRpcError {
    PluginRpcError {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

pub(crate) struct PluginSessionRuntime {
    pub(crate) ui: Arc<ui::RuntimeUiRegistry>,
    delivery: Arc<PluginDeliveryBudget>,
    endpoints: Vec<Arc<dyn rw_ext::PluginEndpoint>>,
    push_handlers: Vec<(String, Arc<SessionPluginPushHandler>)>,
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) hooks: Vec<(HookRegistration, Arc<dyn HookHandler>)>,
    pub(crate) commands: Vec<(
        CommandDescriptor,
        Arc<dyn CommandHandler<SessionCommandContext, SessionCommandOutput>>,
    )>,
    pub(crate) providers: Vec<(String, Arc<dyn rw_providers::Provider>)>,
    pub(crate) event_routers: Vec<PluginEventRegistration>,
    pub(crate) pending: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct PluginEventRegistration {
    pub(crate) plugin_id: String,
    pub(crate) subscriptions: BTreeSet<rw_plugin_protocol::ExtensionEventKind>,
    pub(crate) router: Arc<PluginEventRouter>,
    pub(crate) handler: Arc<SessionPluginPushHandler>,
    pub(crate) budget: Arc<PluginDeliveryBudget>,
}

impl PluginSessionRuntime {
    fn new(
        budget: &Arc<PluginRuntimeBudget>,
        redactor: &Arc<SharedPluginRedactor>,
        session_ui: Arc<ui::UiSessionBudget>,
    ) -> Self {
        Self {
            ui: Arc::new(ui::RuntimeUiRegistry::new(
                Arc::clone(&budget.ui),
                Arc::clone(redactor),
                session_ui,
            )),
            delivery: Arc::clone(&budget.delivery),
            endpoints: Vec::new(),
            push_handlers: Vec::new(),
            tools: Vec::new(),
            hooks: Vec::new(),
            commands: Vec::new(),
            providers: Vec::new(),
            event_routers: Vec::new(),
            pending: Vec::new(),
        }
    }

    pub(crate) fn compose(
        configs: &[crate::extension_config::DiscoveredPlugin],
        private_root: &Path,
        workspace_roots: &[PathBuf],
        helper: &Path,
        redactor: &Arc<SharedPluginRedactor>,
        budget: &Arc<PluginRuntimeBudget>,
        session_ui: Arc<ui::UiSessionBudget>,
    ) -> Result<Self> {
        let mut runtime = Self::new(budget, redactor, session_ui);
        for config in configs.iter().filter(|config| config.enabled) {
            let manifest = match config.load_manifest() {
                Ok(manifest) => manifest,
                Err(error) => {
                    runtime
                        .pending
                        .push(format!("{}: unavailable: {error}", config.name));
                    continue;
                }
            };
            let metadata = rw_ext::PluginEndpointMetadata::new(manifest.clone())
                .map_err(|error| miette!(error.to_string()))?;
            let push_handler = Arc::new(SessionPluginPushHandler::default());
            let endpoint: Arc<dyn rw_ext::PluginEndpoint> = Arc::new(
                activation::DormantPluginEndpoint::new(activation::ActivationRecipe {
                    metadata,
                    approval: activation::ActivationApproval::Configured,
                    config: config.clone(),
                    private_root: private_root.to_path_buf(),
                    workspace_roots: workspace_roots.to_vec(),
                    helper: helper.to_path_buf(),
                    redactor: Arc::clone(redactor),
                    push_handler: Arc::clone(&push_handler),
                    budget: Arc::clone(budget),
                    #[cfg(test)]
                    launcher: None,
                }),
            );
            runtime.register_endpoint(config, &manifest, endpoint, push_handler)?;
        }
        Ok(runtime)
    }

    fn register_endpoint(
        &mut self,
        config: &crate::extension_config::DiscoveredPlugin,
        manifest: &PluginManifest,
        endpoint: Arc<dyn rw_ext::PluginEndpoint>,
        push_handler: Arc<SessionPluginPushHandler>,
    ) -> Result<()> {
        self.ui
            .register(endpoint.clone())
            .map_err(|error| miette!(error.to_string()))?;
        push_handler.bind_ui(Arc::downgrade(&self.ui), endpoint.metadata().ui_owner());
        for declaration in &manifest.capabilities.tools {
            self.tools.push(Arc::new(
                RpcToolAdapter::new(declaration.clone(), endpoint.clone())
                    .map_err(|error| miette!(error.to_string()))?,
            ));
        }
        let effects = endpoint.metadata().process_tool_effects();
        let hook_effect =
            if effects.contains(&rw_plugin_protocol::PluginToolEffect::WritesFilesystem) {
                rw_ext::HookEffect::WorkspaceMutating
            } else {
                rw_ext::HookEffect::ReadOnly
            };
        for declaration in &manifest.capabilities.hooks {
            self.hooks.push((
                plugin_hook_registration(
                    *declaration,
                    format!("plugin:{}:{}", config.name, declaration.name.as_str()),
                    hook_effect,
                )
                .with_required_capabilities(effects.iter().map(|effect| {
                    match effect {
                        rw_plugin_protocol::PluginToolEffect::ReadsFilesystem => {
                            rw_types::ToolCapability::ReadFilesystem
                        }
                        rw_plugin_protocol::PluginToolEffect::WritesFilesystem => {
                            rw_types::ToolCapability::WriteFilesystem
                        }
                        rw_plugin_protocol::PluginToolEffect::Network => {
                            rw_types::ToolCapability::Network
                        }
                        rw_plugin_protocol::PluginToolEffect::Execute => {
                            rw_types::ToolCapability::Execute
                        }
                    }
                })),
                Arc::new(RpcHookHandler::new(endpoint.clone())),
            ));
        }
        for declaration in &manifest.capabilities.commands {
            let descriptor = plugin_command_descriptor(
                &declaration.name,
                &declaration.description,
                declaration.argument_hint.as_deref(),
            );
            self.commands.push((
                descriptor,
                Arc::new(PluginSessionCommand {
                    inner: RpcCommandAdapter::new(&declaration.name, endpoint.clone()),
                }),
            ));
        }
        self.register_providers(config, manifest, &endpoint);
        if !manifest.capabilities.event_subscriptions.is_empty() {
            self.event_routers.push(PluginEventRegistration {
                plugin_id: config.name.clone(),
                subscriptions: manifest
                    .capabilities
                    .event_subscriptions
                    .iter()
                    .copied()
                    .collect(),
                router: Arc::new(PluginEventRouter::new(endpoint.clone())),
                handler: Arc::clone(&push_handler),
                budget: Arc::clone(&self.delivery),
            });
        }
        self.endpoints.push(endpoint);
        self.push_handlers.push((config.name.clone(), push_handler));
        Ok(())
    }

    fn register_providers(
        &mut self,
        config: &crate::extension_config::DiscoveredPlugin,
        manifest: &PluginManifest,
        endpoint: &Arc<dyn rw_ext::PluginEndpoint>,
    ) {
        for declaration in &manifest.capabilities.providers {
            let capabilities = rw_providers::Capabilities {
                tool_calling: true,
                vision: false,
                thinking: false,
                cache_breakpoints: rw_providers::CacheBreakpointSupport::None,
                max_context_tokens: None,
                max_output_tokens: None,
                wire_mode: rw_providers::WireMode::NormalizedReplay,
            };
            let mut adapter = RpcProviderAdapter::new(
                format!("plugin:{}", config.name),
                &declaration.alias_prefix,
                capabilities,
                endpoint.clone(),
            );
            if declaration
                .capabilities
                .iter()
                .any(|capability| capability == "models")
            {
                adapter = adapter.with_model_catalog();
            }
            self.providers
                .push((declaration.alias_prefix.clone(), Arc::new(adapter)));
        }
    }

    fn bind_generation(&self, binding: &rw_core::PluginSessionBinding) -> Result<()> {
        for (plugin_id, handler) in &self.push_handlers {
            let capability = binding
                .bind(plugin_id)
                .map_err(|error| miette!(error.to_string()))?;
            handler.bind(binding.session_id().0.clone(), capability);
        }
        Ok(())
    }

    pub(crate) fn bind_push(&self, handle: &rw_core::SessionHandle) -> Result<()> {
        for (plugin_id, handler) in &self.push_handlers {
            let capability = handle
                .plugin_session_capability(plugin_id.clone())
                .map_err(|error| miette!(error.to_string()))?;
            handler.bind(handle.session_id().0.clone(), capability);
        }
        Ok(())
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.ui.close();
        let mut failure = None;
        for endpoint in &self.endpoints {
            if let Err(error) = endpoint.close().await {
                failure.get_or_insert_with(|| miette!(error.to_string()));
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

fn plugin_command_descriptor(
    name: &str,
    description: &str,
    argument_hint: Option<&str>,
) -> CommandDescriptor {
    let descriptor = CommandDescriptor::new(name, description).with_source(CommandSource::Plugin);
    match argument_hint {
        Some(hint) => descriptor.with_argument_hint(hint),
        None => descriptor,
    }
}

#[derive(Default)]
pub(crate) struct SessionPluginPushHandler {
    ui: RwLock<
        Option<(
            std::sync::Weak<ui::RuntimeUiRegistry>,
            rw_types::extension_ui::UiContributionOwner,
        )>,
    >,
    pub(crate) event_sources: Arc<PluginEventSources>,
    attached: tokio::sync::Notify,
    binding: std::sync::RwLock<Option<(String, rw_core::PluginSessionCapability)>>,
}

impl SessionPluginPushHandler {
    fn bind_ui(
        &self,
        registry: std::sync::Weak<ui::RuntimeUiRegistry>,
        owner: rw_types::extension_ui::UiContributionOwner,
    ) {
        *self
            .ui
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((registry, owner));
    }
    fn bind(&self, session_id: String, capability: rw_core::PluginSessionCapability) {
        *self
            .binding
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((session_id, capability));
        self.attached.notify_waiters();
    }

    pub(crate) async fn capability(
        &self,
        cancellation: &CancellationToken,
    ) -> std::result::Result<rw_core::PluginSessionCapability, PluginRpcError> {
        loop {
            let attached = self.attached.notified();
            if let Ok(capability) = self.bound(&serde_json::json!({})) {
                return Ok(capability);
            }
            tokio::select! { ()=attached=>{}, ()=cancellation.cancelled()=>return Err(plugin_push_error("cancelled","plugin attachment cancelled")), }
        }
    }

    fn bound(
        &self,
        params: &serde_json::Value,
    ) -> std::result::Result<rw_core::PluginSessionCapability, PluginRpcError> {
        let (session_id, capability) = self
            .binding
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                plugin_push_error("push_unavailable", "session actor is not attached")
            })?;
        if params
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|requested| requested != session_id)
        {
            return Err(plugin_push_error(
                "wrong_session",
                "plugin push targeted a different session",
            ));
        }
        Ok(capability)
    }
}

#[async_trait]
impl PushHandler for SessionPluginPushHandler {
    async fn handle_push(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, PluginRpcError> {
        let capability = self.bound(&params)?;
        match method {
            rw_plugin_protocol::METHOD_UI_PUBLISH_PANEL => {
                let update: rw_types::extension_ui::UiPanelUpdate = serde_json::from_value(params)
                    .map_err(|_| plugin_push_error("invalid_push", "invalid panel update"))?;
                update
                    .validate()
                    .map_err(|error| plugin_push_error("invalid_push", &error.to_string()))?;
                let (registry, owner) = self
                    .ui
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                    .ok_or_else(|| {
                        plugin_push_error("ui_unavailable", "UI registry is not attached")
                    })?;
                let registry = registry
                    .upgrade()
                    .ok_or_else(|| plugin_push_error("ui_unavailable", "UI registry is closed"))?;
                let revision = registry.publish_panel(&owner, &update.id, update.data)?;
                serde_json::to_value(rw_types::extension_ui::UiPanelUpdated { revision }).map_err(
                    |_| plugin_push_error("ui_unavailable", "cannot encode panel revision"),
                )
            }
            METHOD_EVENT_READ => {
                let read = serde_json::from_value(params).map_err(|_| {
                    plugin_push_error("invalid_event_read", "invalid event source parameters")
                })?;
                serde_json::to_value(self.event_sources.read(&read)?).map_err(|_| {
                    plugin_push_error("event_read_failed", "event source encoding failed")
                })
            }
            METHOD_SESSION_CONTEXT_READ => {
                let request = serde_json::from_value(params)
                    .map_err(|_| plugin_push_error("invalid_push", "invalid context read"))?;
                plugin_push_result(capability.read_context(request).await)
            }
            METHOD_SESSION_CONTROL => {
                let request: rw_types::extension_invocation::ExtensionControlRequest =
                    serde_json::from_value(params).map_err(|_| {
                        plugin_push_error("invalid_push", "invalid session control")
                    })?;
                plugin_push_result(capability.control(request.origin, request.control).await)
            }
            METHOD_SESSION_QUERY => plugin_push_result(capability.query().await),
            METHOD_EXTENSION_STATE_READ => plugin_push_result(capability.read_state().await),
            METHOD_EXTENSION_STATE_COMMIT => {
                let transaction = plugin_state_transaction(params)?;
                plugin_push_result(capability.commit_state(transaction).await)
            }
            METHOD_SESSION_INJECT_MESSAGE => {
                let content = plugin_push_string(&params, "content")?;
                let disposition = capability
                    .inject_message(content)
                    .await
                    .map_err(|error| plugin_push_error("push_failed", &error.to_string()))?;
                let disposition = match disposition {
                    rw_core::MessageDisposition::Started => {
                        rw_plugin_protocol::InjectionDisposition::Started
                    }
                    rw_core::MessageDisposition::Queued => {
                        rw_plugin_protocol::InjectionDisposition::Queued
                    }
                    rw_core::MessageDisposition::Command => {
                        rw_plugin_protocol::InjectionDisposition::Command
                    }
                };
                serde_json::to_value(rw_plugin_protocol::InjectMessageResult { disposition })
                    .map_err(|_| {
                        plugin_push_error("push_failed", "cannot encode injection disposition")
                    })
            }
            METHOD_SESSION_SET_STATUS => {
                capability
                    .set_status(plugin_push_string(&params, "status")?)
                    .await
                    .map_err(|error| plugin_push_error("push_failed", &error.to_string()))?;
                Ok(serde_json::Value::Null)
            }
            METHOD_UI_NOTIFY => {
                capability
                    .notify(
                        plugin_push_string(&params, "title")?,
                        plugin_push_string(&params, "message")?,
                    )
                    .await
                    .map_err(|error| plugin_push_error("push_failed", &error.to_string()))?;
                Ok(serde_json::Value::Null)
            }
            _ => Err(plugin_push_error(
                "invalid_push",
                "plugin push method is unknown",
            )),
        }
    }
}

fn plugin_push_result<T: serde::Serialize>(
    result: std::result::Result<T, rw_core::AgentLoopError>,
) -> std::result::Result<serde_json::Value, PluginRpcError> {
    let value = result.map_err(|error| plugin_push_error("push_failed", &error.to_string()))?;
    serde_json::to_value(value)
        .map_err(|_| plugin_push_error("push_failed", "cannot encode host command outcome"))
}

fn plugin_push_string(
    params: &serde_json::Value,
    field: &str,
) -> std::result::Result<String, PluginRpcError> {
    params
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| plugin_push_error("invalid_push", "plugin push field is missing"))
}

fn plugin_push_error(code: &str, message: &str) -> PluginRpcError {
    PluginRpcError {
        code: code.to_owned(),
        message: message.chars().take(512).collect(),
    }
}

pub(crate) struct PluginSessionCommand {
    inner: RpcCommandAdapter,
}
#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for PluginSessionCommand {
    async fn execute(
        &self,
        context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> std::result::Result<SessionCommandOutput, CommandExecutionError> {
        let value = self.inner.execute(context, invocation).await?;
        let message = serde_json::to_string(&value).map_err(|_| {
            CommandExecutionError::new(
                "plugin_command_encoding",
                "plugin command output could not be encoded",
            )
        })?;
        if message.len() > MAX_CONTROL_OUTPUT {
            return Err(CommandExecutionError::new(
                "plugin_command_too_large",
                "plugin command output exceeded its size cap",
            ));
        }
        Ok(SessionCommandOutput {
            message,
            action: SessionCommandAction::None,
        })
    }
}

#[derive(Debug)]
pub(crate) struct SharedPluginRedactor(std::sync::RwLock<rw_providers::FixtureRedactor>);
impl SharedPluginRedactor {
    pub(crate) fn new(redactor: rw_providers::FixtureRedactor) -> Self {
        Self(std::sync::RwLock::new(redactor))
    }
    pub(crate) fn bind(&self, redactor: rw_providers::FixtureRedactor) {
        *self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = redactor;
    }
}
impl PluginBoundaryRedactor for SharedPluginRedactor {
    fn redact(&self, mut value: serde_json::Value) -> serde_json::Value {
        redact_plugin_value(
            &self
                .0
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            &mut value,
        );
        value
    }

    fn redact_bytes(&self, value: &[u8]) -> Vec<u8> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .redact_bytes(value)
    }

    fn redact_streaming_prefix(&self, value: &[u8], retain: usize) -> (Vec<u8>, Vec<u8>) {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .redact_streaming_prefix(value, retain)
    }

    fn maximum_secret_bytes(&self) -> usize {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .maximum_registered_secret_bytes()
    }
}
impl rw_providers::KnownSecretRegistrar for SharedPluginRedactor {
    fn register(&self, secret: &rw_providers::Secret) {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register_secret(secret);
    }
}
fn redact_plugin_value(redactor: &rw_providers::FixtureRedactor, value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = redactor.redact_text(text),
        serde_json::Value::Array(values) => {
            for value in values {
                redact_plugin_value(redactor, value);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                redact_plugin_value(redactor, value);
            }
        }
        _ => {}
    }
}

fn redacted_mcp_endpoint(endpoint: &str) -> Result<String> {
    let mut endpoint = url::Url::parse(endpoint).into_diagnostic()?;
    if endpoint.query().is_some() {
        endpoint.set_query(Some("redacted"));
    }
    endpoint.set_fragment(None);
    Ok(endpoint.to_string())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalFile {
    version: u16,
    approvals: BTreeMap<String, String>,
}

fn read_approval_file(path: &Path) -> Result<BTreeMap<String, String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(miette!("approval ledger must be a real regular file"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
                if metadata.uid() != rustix::process::geteuid().as_raw()
                    || metadata.permissions().mode() & 0o077 != 0
                {
                    return Err(miette!(
                        "approval ledger must be current-user owned and private"
                    ));
                }
            }
            let bytes = fs::read(path).into_diagnostic()?;
            if bytes.len() > 1024 * 1024 {
                return Err(miette!("approval ledger exceeded its size cap"));
            }
            let file: ApprovalFile = serde_json::from_slice(&bytes).into_diagnostic()?;
            if file.version != APPROVAL_VERSION {
                return Err(miette!("unsupported approval ledger version"));
            }
            Ok(file.approvals)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error).into_diagnostic(),
    }
}

fn persist_approval_file(path: &Path, values: &BTreeMap<String, String>) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| miette!("approval ledger has no parent"))?;
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|error| miette!("approval entropy failed: {error}"))?;
    let temporary = parent.join(format!(".approval-{}.tmp", u64::from_ne_bytes(random)));
    let cleanup = TempFileCleanup(temporary.clone());
    let bytes = serde_json::to_vec(&ApprovalFile {
        version: APPROVAL_VERSION,
        approvals: values.clone(),
    })
    .into_diagnostic()?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).into_diagnostic()?;
    file.write_all(&bytes).into_diagnostic()?;
    file.sync_all().into_diagnostic()?;
    fs::rename(&temporary, path).into_diagnostic()?;
    std::mem::forget(cleanup);
    fs::File::open(parent)
        .and_then(|file| file.sync_all())
        .into_diagnostic()?;
    Ok(())
}

/// User-private durable plugin approvals. Values are launch-identity fingerprints,
/// never executable paths, manifests, environment values, or credentials.
pub struct PrivatePluginApprovalStore {
    path: PathBuf,
    values: Mutex<BTreeMap<String, String>>,
}

impl PrivatePluginApprovalStore {
    /// Opens the private durable plugin-approval ledger.
    ///
    /// # Errors
    /// Returns an error when the root or ledger is unsafe, malformed, or unreadable.
    pub fn open(private_root: &Path) -> Result<Self> {
        validate_private_root(private_root)?;
        let path = private_root.join("plugin-approvals-v1.json");
        let values = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(miette!(
                        "plugin approval ledger must be a real regular file"
                    ));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
                    if metadata.uid() != rustix::process::geteuid().as_raw()
                        || metadata.permissions().mode() & 0o077 != 0
                    {
                        return Err(miette!(
                            "plugin approval ledger must be current-user owned and private"
                        ));
                    }
                }
                let bytes = fs::read(&path).into_diagnostic()?;
                if bytes.len() > 1024 * 1024 {
                    return Err(miette!("plugin approval ledger exceeded its size cap"));
                }
                let file: ApprovalFile = serde_json::from_slice(&bytes).into_diagnostic()?;
                if file.version != APPROVAL_VERSION {
                    return Err(miette!("unsupported plugin approval ledger version"));
                }
                file.approvals
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error).into_diagnostic(),
        };
        Ok(Self {
            path,
            values: Mutex::new(values),
        })
    }

    fn persist(&self, values: &BTreeMap<String, String>) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| miette!("plugin approval ledger has no parent"))?;
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random)
            .map_err(|error| miette!("approval entropy failed: {error}"))?;
        let temporary = parent.join(format!(
            ".plugin-approvals-{}.tmp",
            u64::from_ne_bytes(random)
        ));
        let cleanup = TempFileCleanup(temporary.clone());
        let bytes = serde_json::to_vec(&ApprovalFile {
            version: APPROVAL_VERSION,
            approvals: values.clone(),
        })
        .into_diagnostic()?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).into_diagnostic()?;
        file.write_all(&bytes).into_diagnostic()?;
        file.sync_all().into_diagnostic()?;
        fs::rename(&temporary, &self.path).into_diagnostic()?;
        std::mem::forget(cleanup);
        fs::File::open(parent)
            .and_then(|file| file.sync_all())
            .into_diagnostic()?;
        Ok(())
    }
}

impl ApprovalStore for PrivatePluginApprovalStore {
    fn approved_fingerprint(
        &self,
        plugin_name: &str,
    ) -> std::result::Result<Option<String>, ApprovalStoreError> {
        self.values
            .lock()
            .map(|values| values.get(plugin_name).cloned())
            .map_err(|_| ApprovalStoreError {
                message: "approval ledger lock was poisoned".to_owned(),
            })
    }
    fn record_approval(
        &self,
        plugin_name: &str,
        fingerprint: &str,
    ) -> std::result::Result<(), ApprovalStoreError> {
        let mut values = self.values.lock().map_err(|_| ApprovalStoreError {
            message: "approval ledger lock was poisoned".to_owned(),
        })?;
        let mut updated = values.clone();
        updated.insert(plugin_name.to_owned(), fingerprint.to_owned());
        self.persist(&updated).map_err(|error| ApprovalStoreError {
            message: error.to_string(),
        })?;
        *values = updated;
        Ok(())
    }
}

impl PrivatePluginApprovalStore {
    /// Revokes an approved plugin launch identity.
    ///
    /// # Errors
    /// Returns an error when the ledger cannot be locked or durably updated.
    pub fn revoke(&self, plugin_name: &str) -> Result<bool> {
        let mut values = self
            .values
            .lock()
            .map_err(|_| miette!("plugin approval ledger lock was poisoned"))?;
        if !values.contains_key(plugin_name) {
            return Ok(false);
        }
        let mut updated = values.clone();
        updated.remove(plugin_name);
        self.persist(&updated)?;
        *values = updated;
        Ok(true)
    }
}

struct TempFileCleanup(PathBuf);
impl Drop for TempFileCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn validate_private_root(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).into_diagnostic()?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(miette!("plugin approval root must be a real directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(miette!(
                "plugin approval root must be current-user owned and mode 0700"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

fn plugin_state_transaction(
    params: serde_json::Value,
) -> std::result::Result<rw_types::extension_contract::ExtensionStateTransaction, PluginRpcError> {
    let transaction: rw_types::extension_contract::ExtensionStateTransaction =
        serde_json::from_value(params)
            .map_err(|_| plugin_push_error("invalid_push", "invalid state transaction"))?;
    if transaction.acknowledged.is_some() {
        return Err(plugin_push_error(
            "invalid_push",
            "delivery acknowledgement is host-owned",
        ));
    }
    Ok(transaction)
}
