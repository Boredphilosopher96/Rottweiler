//! Rottweiler-as-MCP-server composition for the CLI stdio transport.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use miette::{Result, miette};
use rw_core::{
    BoundClient, ClientCommand, ClientId, CommandMeta, CommandOutcome, EngineHost,
    EngineHostConfig, PROTOCOL_VERSION, PermissionApprover, PermissionGate, PermissionOutcome,
    PermissionRequest, RequestId,
};
use rw_mcp::{
    BridgeError, EngineMcpBridge, EngineTool, MAX_SERVER_SESSIONS, McpResponse, McpResponseLimits,
    McpResponseSlot, McpServerAuthority, RottweilerMcpServerFactory, SessionSummary, serve_stdio,
};
use rw_tools::{GlobTool, GrepTool, LsTool, ReadTool, Tool, ToolContext, ToolLimits, ToolRegistry};
use rw_types::{ApprovalDecision, PermissionModeDescriptor};
use serde_json::{Value, json};

use rw_runtime::{RuntimeHostOptions, RuntimeSessionFactory, session::HostedProviderMode};

mod construction;
use construction::{Construction, WORKING_BYTES};

/// Inputs for one stdio MCP server process.
pub(crate) struct StdioServerOptions {
    pub(crate) workspace_roots: Vec<PathBuf>,
    pub(crate) storage_root: PathBuf,
    pub(crate) credentials_path: PathBuf,
    pub(crate) config: rw_core::Config,
    pub(crate) permission_mode: Option<PermissionModeDescriptor>,
    pub(crate) max_turns: usize,
    pub(crate) provider_mode: HostedProviderMode,
    pub(crate) dangerously_trust: bool,
}

struct DenyPrompt;

#[async_trait]
impl PermissionApprover for DenyPrompt {
    async fn decide(&self, _request: PermissionRequest) -> ApprovalDecision {
        ApprovalDecision::Deny
    }
}

struct CliMcpBridge {
    response_limits: McpResponseLimits,
    host: EngineHost,
    registry: Arc<ToolRegistry>,
    tool_context: ToolContext,
    permissions: PermissionGate,
    bound: BoundClient,
    workspace: String,
    request_sequence: AtomicU64,
    request_namespace: String,
}

impl CliMcpBridge {
    fn next_meta(&self) -> CommandMeta {
        let sequence = self.request_sequence.fetch_add(1, Ordering::Relaxed);
        CommandMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: self.bound.client_id.clone(),
            request_id: RequestId(format!("mcp-{}-{sequence}", self.request_namespace)),
        }
    }

    async fn shutdown(&self) {
        let _ = self
            .host
            .dispatch(
                self.bound.clone(),
                ClientCommand::ShutdownHost {
                    meta: self.next_meta(),
                },
            )
            .await;
    }
}

fn require_accepted(
    outcome: &CommandOutcome,
    safe_message: &'static str,
) -> Result<(), BridgeError> {
    match outcome {
        CommandOutcome::Accepted {} => Ok(()),
        CommandOutcome::Rejected { .. } => Err(BridgeError::safe(safe_message)),
    }
}

#[async_trait]
impl EngineMcpBridge for CliMcpBridge {
    fn response_limits(&self) -> McpResponseLimits {
        self.response_limits
    }

    async fn tools(
        &self,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Vec<EngineTool>>, BridgeError> {
        Construction::new(Arc::clone(&self.registry), slot)
            .map_cpu(construction::tool_descriptors)
            .await?
            .adopt()
            .await
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Value>, BridgeError> {
        let work = Construction::new(arguments, slot)
            .map_cpu(construction::tool_arguments)
            .await?;
        let tool = self
            .registry
            .resolve(name)
            .ok_or_else(|| BridgeError::safe("tool is unavailable"))?;
        let capabilities = tool
            .invocation_capabilities(&work.value.0)
            .map_err(|_| BridgeError::safe("tool input could not be authorized"))?;
        let request_id = self.next_meta().request_id.0;
        let request = PermissionRequest {
            invocation_id: rw_types::ToolInvocationId(request_id.clone()),
            id: request_id,
            tool_name: name.to_owned(),
            arguments: work.value.1,
            capabilities: capabilities.capabilities().to_vec(),
            approval_diff: None,
        };
        if self.permissions.authorize(request, &DenyPrompt).await != PermissionOutcome::Allowed {
            return Err(BridgeError::safe("tool invocation was denied by policy"));
        }
        let output = tool
            .execute(&self.tool_context, work.value.0)
            .await
            .map_err(|_| BridgeError::safe("tool execution failed"))?;
        Construction::new(output, work.slot)
            .map_cpu(|output| {
                Ok(json!({
                    "content": output.content,
                    "data": output.data,
                    "truncated": output.truncated,
                }))
            })
            .await?
            .adopt()
            .await
    }

    async fn create_session(
        &self,
        _title: Option<String>,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<SessionSummary>, BridgeError> {
        construction::create_session(self, slot).await
    }

    async fn list_sessions(
        &self,
        mut authorized: Vec<String>,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Vec<SessionSummary>>, BridgeError> {
        construction::validate_authorized(&mut authorized)?;
        let mut sessions = Vec::with_capacity(authorized.len());
        for id in authorized {
            let id = rw_core::SessionId(id);
            if let Some(session) = self.host.session(&id).await {
                sessions.push(SessionSummary {
                    id: id.0,
                    state: if session.is_driver(&self.bound.client_id) {
                        "driver"
                    } else {
                        "idle"
                    }
                    .to_owned(),
                });
            }
        }
        Construction::new(sessions, slot).adopt().await
    }

    async fn send_message(
        &self,
        session_id: &str,
        message: &str,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Value>, BridgeError> {
        let outcome = self
            .host
            .dispatch(
                self.bound.clone(),
                ClientCommand::SendMessage {
                    meta: self.next_meta(),
                    session_id: rw_core::SessionId(session_id.to_owned()),
                    content: message.to_owned(),
                    attachments: Vec::new(),
                },
            )
            .await;
        require_accepted(&outcome.outcome, "engine rejected the session message")?;
        Construction::new(json!({"accepted": true, "session_id": session_id}), slot)
            .adopt()
            .await
    }
}

fn read_only_tools() -> Result<Arc<ToolRegistry>> {
    let limits = ToolLimits::default();
    let tools: [Arc<dyn Tool>; 4] = [
        Arc::new(ReadTool::new(limits)),
        Arc::new(GrepTool::new(limits)),
        Arc::new(GlobTool::new(limits)),
        Arc::new(LsTool::new(limits)),
    ];
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry
            .register(tool)
            .map_err(|_| miette!("MCP tool registry could not initialize"))?;
    }
    Ok(Arc::new(registry))
}

/// Run one production stdio MCP connection until its peer disconnects.
pub(crate) async fn run_stdio(options: StdioServerOptions) -> Result<()> {
    let workspace = options
        .workspace_roots
        .first()
        .ok_or_else(|| miette!("MCP server requires an authorized workspace"))?
        .clone();
    let host_options = RuntimeHostOptions {
        storage_root: options.storage_root,
        credentials_path: options.credentials_path,
        config: options.config,
        allowed_workspaces: options.workspace_roots.clone(),
        permission_mode: options.permission_mode,
        max_turns: options.max_turns,
        provider_mode: options.provider_mode,
        dangerously_trust: options.dangerously_trust,
        wait_for_execution_lease: false,
    };
    let factory = Arc::new(
        RuntimeSessionFactory::new(host_options)
            .await
            .map_err(|_| miette!("MCP engine host could not initialize"))?,
    );
    let host = rw_runtime::HeadlessRuntimeBuilder::new(factory)
        .with_config(EngineHostConfig {
            max_sessions: MAX_SERVER_SESSIONS,
            ..EngineHostConfig::default()
        })
        .build()
        .map_err(|_| miette!("MCP engine host could not initialize"))?;
    let registry = read_only_tools()?;
    let allowed_tools = registry
        .descriptors()
        .into_iter()
        .map(|descriptor| descriptor.name)
        .collect::<Vec<_>>();
    let tool_context = ToolContext::from_workspace_roots(&options.workspace_roots)
        .map_err(|_| miette!("MCP workspace authority could not initialize"))?;
    let permissions = PermissionGate::for_headless_mode(PermissionModeDescriptor::AutoSafe)
        .with_workspace_roots(&options.workspace_roots);
    let mut request_entropy = [0_u8; 16];
    getrandom::fill(&mut request_entropy)
        .map_err(|_| miette!("MCP request identity entropy is unavailable"))?;
    let bridge = Arc::new(CliMcpBridge {
        response_limits: McpResponseLimits::new(WORKING_BYTES)
            .map_err(|_| miette!("MCP response construction limit is invalid"))?,
        host,
        registry,
        tool_context,
        permissions,
        bound: BoundClient {
            client_id: ClientId(format!("mcp-stdio-{}", std::process::id())),
        },
        workspace: workspace.to_string_lossy().into_owned(),
        request_sequence: AtomicU64::new(1),
        request_namespace: format!("{:032x}", u128::from_le_bytes(request_entropy)),
    });
    let server = RottweilerMcpServerFactory::new(bridge.clone(), move || {
        McpServerAuthority::new(allowed_tools.clone(), std::iter::empty())
            .map(|authority| authority.with_session_access(true, true, true))
    })
    .create()
    .map_err(|_| miette!("MCP server authority could not initialize"))?;
    let result = serve_stdio(server).await;
    bridge.shutdown().await;
    result.map_err(|_| miette!("Rottweiler MCP stdio service ended abnormally"))
}

#[cfg(test)]
mod tests;
