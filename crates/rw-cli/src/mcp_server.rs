//! Rottweiler-as-MCP-server composition for the CLI stdio transport.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use miette::{Result, miette};
use rw_core::{
    BoundClient, ClientCommand, ClientId, CommandMeta, CommandOutcome, EngineEvent, EngineHost,
    EngineHostConfig, HeadlessPermissionMode, PROTOCOL_VERSION, PermissionApprover, PermissionGate,
    PermissionOutcome, PermissionRequest, RequestId,
};
use rw_mcp::{
    BridgeError, EngineMcpBridge, EngineTool, McpServerAuthority, RottweilerMcpServerFactory,
    SessionSummary, serve_stdio,
};
use rw_tools::{GlobTool, GrepTool, LsTool, ReadTool, Tool, ToolContext, ToolLimits, ToolRegistry};
use rw_types::ApprovalDecision;
use serde_json::{Value, json};

use rw_runtime::{RuntimeHostOptions, RuntimeSessionFactory, session::HostedProviderMode};

const HOST_RESULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_EXPOSED_TOOLS: usize = 32;

/// Inputs for one stdio MCP server process.
pub(crate) struct StdioServerOptions {
    pub(crate) workspace_roots: Vec<PathBuf>,
    pub(crate) storage_root: PathBuf,
    pub(crate) credentials_path: PathBuf,
    pub(crate) config: rw_core::Config,
    pub(crate) permission_mode: Option<rw_runtime::PermissionMode>,
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
    host: EngineHost,
    registry: Arc<ToolRegistry>,
    tool_context: ToolContext,
    permissions: PermissionGate,
    bound: BoundClient,
    workspace: String,
    request_sequence: AtomicU64,
}

impl CliMcpBridge {
    fn next_meta(&self) -> CommandMeta {
        let sequence = self.request_sequence.fetch_add(1, Ordering::Relaxed);
        CommandMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: self.bound.client_id.clone(),
            request_id: RequestId(format!("mcp-{sequence}")),
        }
    }

    async fn dispatch_with_sessions(
        &self,
        command: ClientCommand,
    ) -> Result<Vec<rw_core::SessionDescriptor>, BridgeError> {
        let request_id = command.meta().request_id.clone();
        let mut events = self
            .host
            .subscribe(self.bound.clone(), None, None)
            .await
            .map_err(|_| BridgeError::safe("engine session query is unavailable"))?;
        require_accepted(
            &self.host.dispatch(self.bound.clone(), command).await,
            "engine session request was rejected",
        )?;
        tokio::time::timeout(HOST_RESULT_TIMEOUT, async {
            while let Some(event) = events.recv().await {
                match event {
                    Ok(EngineEvent::SessionsListed { meta, sessions })
                        if meta.request_id == request_id =>
                    {
                        return Ok(sessions);
                    }
                    Ok(_) => {}
                    Err(_) => {
                        return Err(BridgeError::safe(
                            "engine session result stream is unavailable",
                        ));
                    }
                }
            }
            Err(BridgeError::safe(
                "engine session result stream ended unexpectedly",
            ))
        })
        .await
        .unwrap_or_else(|_| Err(BridgeError::safe("engine session request timed out")))
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
        CommandOutcome::Accepted => Ok(()),
        CommandOutcome::Rejected { .. } => Err(BridgeError::safe(safe_message)),
    }
}

#[async_trait]
impl EngineMcpBridge for CliMcpBridge {
    async fn tools(&self) -> Result<Vec<EngineTool>, BridgeError> {
        Ok(self
            .registry
            .descriptors()
            .into_iter()
            .take(MAX_EXPOSED_TOOLS)
            .map(|descriptor| EngineTool {
                name: descriptor.name,
                description: descriptor.description,
                input_schema: descriptor.input_schema,
            })
            .collect())
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, BridgeError> {
        let tool = self
            .registry
            .resolve(name)
            .ok_or_else(|| BridgeError::safe("tool is unavailable"))?;
        let capabilities = tool
            .invocation_capabilities(&arguments)
            .map_err(|_| BridgeError::safe("tool input could not be authorized"))?;
        let request = PermissionRequest {
            id: self.next_meta().request_id.0,
            tool_name: name.to_owned(),
            arguments: arguments.clone(),
            capabilities: capabilities.capabilities().to_vec(),
            approval_diff: None,
        };
        if self.permissions.authorize(request, &DenyPrompt).await != PermissionOutcome::Allowed {
            return Err(BridgeError::safe("tool invocation was denied by policy"));
        }
        let output = tool
            .execute(&self.tool_context, arguments)
            .await
            .map_err(|_| BridgeError::safe("tool execution failed"))?;
        Ok(json!({
            "content": output.content,
            "data": output.data,
            "truncated": output.truncated,
        }))
    }

    async fn create_session(&self, _title: Option<String>) -> Result<SessionSummary, BridgeError> {
        let sessions = self
            .dispatch_with_sessions(ClientCommand::CreateSession {
                meta: self.next_meta(),
                cwd: self.workspace.clone(),
                model: None,
            })
            .await?;
        let session = sessions
            .into_iter()
            .next()
            .ok_or_else(|| BridgeError::safe("engine did not return the created session"))?;
        Ok(SessionSummary {
            id: session.session_id.0,
            state: "driver".to_owned(),
        })
    }

    async fn list_sessions(&self) -> Result<Vec<SessionSummary>, BridgeError> {
        self.dispatch_with_sessions(ClientCommand::ListSessions {
            meta: self.next_meta(),
        })
        .await
        .map(|sessions| {
            sessions
                .into_iter()
                .map(|session| SessionSummary {
                    state: if session.driver_client_id.as_ref() == Some(&self.bound.client_id) {
                        "driver"
                    } else {
                        "idle"
                    }
                    .to_owned(),
                    id: session.session_id.0,
                })
                .collect()
        })
    }

    async fn send_message(&self, session_id: &str, message: &str) -> Result<Value, BridgeError> {
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
        require_accepted(&outcome, "engine rejected the session message")?;
        Ok(json!({"accepted": true, "session_id": session_id}))
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
            .map_err(|_| miette!("MCP engine host could not initialize"))?,
    );
    let host = rw_runtime::HeadlessRuntimeBuilder::new(factory)
        .with_config(EngineHostConfig::default())
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
    let permissions = PermissionGate::for_headless_mode(HeadlessPermissionMode::AutoSafe)
        .with_workspace_roots(&options.workspace_roots);
    let bridge = Arc::new(CliMcpBridge {
        host,
        registry,
        tool_context,
        permissions,
        bound: BoundClient {
            client_id: ClientId(format!("mcp-stdio-{}", std::process::id())),
        },
        workspace: workspace.to_string_lossy().into_owned(),
        request_sequence: AtomicU64::new(1),
    });
    let server = RottweilerMcpServerFactory::new(bridge.clone(), move || {
        McpServerAuthority::new(allowed_tools.clone(), std::iter::empty())
            .with_session_access(true, true, true)
    })
    .create();
    let result = serve_stdio(server).await;
    bridge.shutdown().await;
    result.map_err(|_| miette!("Rottweiler MCP stdio service ended abnormally"))
}
