use std::{collections::BTreeSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use rmcp::{
    ErrorData as McpProtocolError, ServerHandler, ServiceExt as _,
    model::{
        CallToolRequestParams, CallToolResult, ContentBlock, Implementation, ListToolsResult,
        ServerCapabilities, ServerInfo, Tool,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::RwLock;

const MAX_WIRE_TEXT: usize = 16 * 1024;
const MAX_SERVER_RESULT: usize = 256 * 1024;
const MAX_SERVER_ARGUMENTS: usize = 64 * 1024;

/// Serve one already-authorized Rottweiler MCP server over the process stdio.
///
/// # Errors
///
/// Returns a sanitized protocol error when initialization fails or the service
/// task terminates abnormally.
pub async fn serve_stdio(server: RottweilerMcpServer) -> Result<(), crate::McpError> {
    let running = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| crate::McpError::Protocol(error.to_string()))?;
    running
        .waiting()
        .await
        .map_err(|error| crate::McpError::Protocol(error.to_string()))?;
    Ok(())
}

/// Deliberately caller-safe bridge failure; internal errors must be redacted before construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeError {
    safe_message: String,
}

impl BridgeError {
    #[must_use]
    pub fn safe(message: impl Into<String>) -> Self {
        Self {
            safe_message: message.into().chars().take(512).collect(),
        }
    }
}

/// Abstract engine tool exposed by MCP server mode.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EngineTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub state: String,
}

/// Narrow boundary: the adapter neither owns nor silently takes a driver's lease.
#[async_trait]
pub trait EngineMcpBridge: Send + Sync + 'static {
    async fn tools(&self) -> Result<Vec<EngineTool>, BridgeError>;
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, BridgeError>;
    async fn create_session(&self, title: Option<String>) -> Result<SessionSummary, BridgeError>;
    async fn list_sessions(&self) -> Result<Vec<SessionSummary>, BridgeError>;
    async fn send_message(&self, session_id: &str, message: &str) -> Result<Value, BridgeError>;
}

pub struct RottweilerMcpServer {
    bridge: Arc<dyn EngineMcpBridge>,
    authority: Arc<McpServerAuthority>,
    request_timeout: Duration,
}

/// Creates a new server and authority for every accepted HTTP connection.
/// The HTTP host must enforce exact Host/Origin allowlists and a pre-decode body/frame cap.
pub struct RottweilerMcpServerFactory {
    bridge: Arc<dyn EngineMcpBridge>,
    authority: Arc<dyn Fn() -> McpServerAuthority + Send + Sync>,
    request_timeout: Duration,
}

impl RottweilerMcpServerFactory {
    #[must_use]
    pub fn new(
        bridge: Arc<dyn EngineMcpBridge>,
        authority: impl Fn() -> McpServerAuthority + Send + Sync + 'static,
    ) -> Self {
        Self {
            bridge,
            authority: Arc::new(authority),
            request_timeout: Duration::from_secs(30),
        }
    }

    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    #[must_use]
    pub fn create(&self) -> RottweilerMcpServer {
        RottweilerMcpServer {
            bridge: Arc::clone(&self.bridge),
            authority: Arc::new((self.authority)()),
            request_timeout: self.request_timeout,
        }
    }
}

/// Host-minted least privilege for one MCP connection.
pub struct McpServerAuthority {
    allowed_tools: BTreeSet<String>,
    sessions: RwLock<BTreeSet<String>>,
    allow_create: bool,
    allow_list: bool,
    allow_send: bool,
}

impl McpServerAuthority {
    #[must_use]
    pub fn new(
        allowed_tools: impl IntoIterator<Item = String>,
        explicit_sessions: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            allowed_tools: allowed_tools.into_iter().collect(),
            sessions: RwLock::new(explicit_sessions.into_iter().collect()),
            allow_create: false,
            allow_list: false,
            allow_send: false,
        }
    }

    #[must_use]
    pub fn with_session_access(mut self, create: bool, list: bool, send: bool) -> Self {
        self.allow_create = create;
        self.allow_list = list;
        self.allow_send = send;
        self
    }
}

impl RottweilerMcpServer {
    #[must_use]
    pub fn new(bridge: Arc<dyn EngineMcpBridge>, authority: McpServerAuthority) -> Self {
        Self {
            bridge,
            authority: Arc::new(authority),
            request_timeout: Duration::from_secs(30),
        }
    }

    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    fn builtin_tools() -> Vec<Tool> {
        [
            ("rottweiler_tools_call", "Call an approved Rottweiler tool", json!({"type":"object","required":["name","arguments"],"properties":{"name":{"type":"string"},"arguments":{"type":"object"}}})),
            ("rottweiler_sessions_create", "Create a Rottweiler session owned by this MCP client", json!({"type":"object","properties":{"title":{"type":"string"}}})),
            ("rottweiler_sessions_list", "List Rottweiler sessions without taking their driver lease", json!({"type":"object"})),
            ("rottweiler_sessions_send", "Send a message to a session this client may drive", json!({"type":"object","required":["session_id","message"],"properties":{"session_id":{"type":"string"},"message":{"type":"string"}}})),
        ].into_iter().filter_map(|(name, description, schema)| tool(name, description, schema)).collect()
    }
}

#[allow(clippy::needless_pass_by_value)]
fn tool(name: &'static str, description: &'static str, schema: Value) -> Option<Tool> {
    let input_schema = schema.as_object()?.clone();
    Some(Tool::new(name, description, input_schema))
}

#[derive(Deserialize)]
struct ToolCall {
    name: String,
    #[serde(default)]
    arguments: Value,
}
#[derive(Deserialize)]
struct CreateSession {
    title: Option<String>,
}
#[derive(Deserialize)]
struct SendMessage {
    session_id: String,
    message: String,
}

fn arguments(request: &CallToolRequestParams) -> Value {
    Value::Object(request.arguments.clone().unwrap_or_default())
}

fn parse<T: serde::de::DeserializeOwned>(
    request: &CallToolRequestParams,
) -> Result<T, McpProtocolError> {
    serde_json::from_value(arguments(request))
        .map_err(|error| McpProtocolError::invalid_params(error.to_string(), None))
}

fn result(value: Value) -> CallToolResult {
    let bytes = serde_json::to_vec(&value).unwrap_or_default();
    if bytes.len() > MAX_SERVER_RESULT {
        return tool_error("Rottweiler MCP server result exceeded its size cap");
    }
    CallToolResult::structured(value)
}

fn tool_error(message: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(
        message.chars().take(512).collect::<String>(),
    )])
}

impl ServerHandler for RottweilerMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rottweiler", env!("CARGO_PKG_VERSION")))
            .with_instructions("Rottweiler coding-agent sessions and approved tools")
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<ListToolsResult, McpProtocolError> {
        Ok(ListToolsResult {
            tools: Self::builtin_tools(),
            next_cursor: None,
            meta: None,
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        Self::builtin_tools()
            .into_iter()
            .find(|tool| tool.name == name)
    }

    #[allow(clippy::too_many_lines)]
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<CallToolResult, McpProtocolError> {
        if serde_json::to_vec(&request.arguments)
            .is_ok_and(|bytes| bytes.len() > MAX_SERVER_ARGUMENTS)
        {
            return Ok(tool_error("MCP request arguments exceed the size cap"));
        }
        let output = match request.name.as_ref() {
            "rottweiler_tools_call" => {
                let input: ToolCall = parse(&request)?;
                if input.name.len() > 256 || !self.authority.allowed_tools.contains(&input.name) {
                    return Ok(tool_error(
                        "tool is outside this MCP connection's authority",
                    ));
                }
                tokio::time::timeout(
                    self.request_timeout,
                    self.bridge.call_tool(&input.name, input.arguments),
                )
                .await
                .unwrap_or_else(|_| Err(BridgeError::safe("engine tool request timed out")))
            }
            "rottweiler_sessions_create" => {
                if !self.authority.allow_create {
                    return Ok(tool_error(
                        "session creation is outside this MCP connection's authority",
                    ));
                }
                let input: CreateSession = parse(&request)?;
                if input.title.as_ref().is_some_and(|title| title.len() > 512) {
                    return Ok(tool_error("session title exceeds its size cap"));
                }
                let created = tokio::time::timeout(
                    self.request_timeout,
                    self.bridge.create_session(input.title),
                )
                .await
                .unwrap_or_else(|_| Err(BridgeError::safe("session creation timed out")));
                match created {
                    Ok(value) => {
                        self.authority
                            .sessions
                            .write()
                            .await
                            .insert(value.id.clone());
                        serde_json::to_value(value)
                            .map_err(|_| BridgeError::safe("session result encoding failed"))
                    }
                    Err(error) => Err(error),
                }
            }
            "rottweiler_sessions_list" => {
                if !self.authority.allow_list {
                    return Ok(tool_error(
                        "session listing is outside this MCP connection's authority",
                    ));
                }
                let allowed = self.authority.sessions.read().await.clone();
                tokio::time::timeout(self.request_timeout, self.bridge.list_sessions())
                    .await
                    .unwrap_or_else(|_| Err(BridgeError::safe("session listing timed out")))
                    .map(|sessions| {
                        sessions
                            .into_iter()
                            .filter(|session| allowed.contains(&session.id))
                            .collect::<Vec<_>>()
                    })
                    .and_then(|value| {
                        serde_json::to_value(value)
                            .map_err(|_| BridgeError::safe("session result encoding failed"))
                    })
            }
            "rottweiler_sessions_send" => {
                if !self.authority.allow_send {
                    return Ok(tool_error(
                        "session messaging is outside this MCP connection's authority",
                    ));
                }
                let input: SendMessage = parse(&request)?;
                if input.session_id.len() > 256
                    || input.message.len() > MAX_WIRE_TEXT
                    || !self
                        .authority
                        .sessions
                        .read()
                        .await
                        .contains(&input.session_id)
                {
                    return Ok(tool_error(
                        "session is outside this MCP connection's authority or input is oversized",
                    ));
                }
                tokio::time::timeout(
                    self.request_timeout,
                    self.bridge.send_message(&input.session_id, &input.message),
                )
                .await
                .unwrap_or_else(|_| Err(BridgeError::safe("session message timed out")))
            }
            _ => {
                return Err(McpProtocolError::method_not_found::<
                    rmcp::model::CallToolRequestMethod,
                >());
            }
        };
        Ok(output.map_or_else(|error| tool_error(&error.safe_message), result))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use rmcp::model::CallToolRequestParams;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Bridge {
        messages: AtomicUsize,
    }

    #[async_trait]
    impl EngineMcpBridge for Bridge {
        async fn tools(&self) -> Result<Vec<EngineTool>, BridgeError> {
            Ok(Vec::new())
        }
        async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, BridgeError> {
            Ok(json!({"name":name,"arguments":arguments}))
        }
        async fn create_session(
            &self,
            _title: Option<String>,
        ) -> Result<SessionSummary, BridgeError> {
            Ok(SessionSummary {
                id: "owned".to_owned(),
                state: "idle".to_owned(),
            })
        }
        async fn list_sessions(&self) -> Result<Vec<SessionSummary>, BridgeError> {
            Ok(vec![
                SessionSummary {
                    id: "owned".to_owned(),
                    state: "idle".to_owned(),
                },
                SessionSummary {
                    id: "foreign".to_owned(),
                    state: "idle".to_owned(),
                },
            ])
        }
        async fn send_message(
            &self,
            session_id: &str,
            message: &str,
        ) -> Result<Value, BridgeError> {
            self.messages.fetch_add(1, Ordering::Relaxed);
            Ok(json!({"session":session_id,"message":message}))
        }
    }

    fn arguments(value: &Value) -> rmcp::model::JsonObject {
        value.as_object().cloned().expect("object")
    }

    #[tokio::test]
    async fn another_agent_drives_server_fixture_with_scoped_authority() {
        let bridge = Arc::new(Bridge {
            messages: AtomicUsize::new(0),
        });
        let factory = RottweilerMcpServerFactory::new(bridge.clone(), || {
            McpServerAuthority::new(["read".to_owned()], std::iter::empty())
                .with_session_access(true, true, true)
        });
        let server = factory.create();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (server_service, client_service) =
            tokio::join!(server.serve(server_io), ().serve(client_io));
        let mut server_service = server_service.expect("server");
        let mut client_service = client_service.expect("client");
        assert_eq!(
            client_service
                .peer()
                .list_all_tools()
                .await
                .expect("tools")
                .len(),
            4
        );

        let denied = client_service
            .peer()
            .call_tool(
                CallToolRequestParams::new("rottweiler_tools_call")
                    .with_arguments(arguments(&json!({"name":"bash","arguments":{}}))),
            )
            .await
            .expect("denied");
        assert_eq!(denied.is_error, Some(true));
        let created = client_service
            .peer()
            .call_tool(
                CallToolRequestParams::new("rottweiler_sessions_create")
                    .with_arguments(arguments(&json!({}))),
            )
            .await
            .expect("create");
        assert_eq!(created.is_error, Some(false));
        let sent = client_service
            .peer()
            .call_tool(
                CallToolRequestParams::new("rottweiler_sessions_send")
                    .with_arguments(arguments(&json!({"session_id":"owned","message":"hello"}))),
            )
            .await
            .expect("send");
        assert_eq!(sent.is_error, Some(false));
        let foreign = client_service
            .peer()
            .call_tool(
                CallToolRequestParams::new("rottweiler_sessions_send").with_arguments(arguments(
                    &json!({"session_id":"foreign","message":"steal"}),
                )),
            )
            .await
            .expect("foreign");
        assert_eq!(foreign.is_error, Some(true));
        assert_eq!(bridge.messages.load(Ordering::Relaxed), 1);
        client_service.close().await.expect("close client");
        server_service.close().await.expect("close server");
    }
}
