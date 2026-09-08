use std::{collections::BTreeSet, sync::Arc, time::Duration};

use crate::{McpResponse, McpResponseLimits, McpResponseSlot};
use async_trait::async_trait;
use rmcp::{
    ErrorData as McpProtocolError,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ServerCapabilities, ServerInfo, Tool,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::RwLock;

mod allocation;
mod dispatch;
mod invocation;
mod lifecycle;
mod native;
#[cfg(test)]
mod ownership_tests;
#[cfg(test)]
mod tests;
mod wire;

/// Bound on session identities owned by one MCP connection.
pub const MAX_SERVER_SESSIONS: usize = 32;

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
    native::serve(server)
        .await
        .map_err(|error| crate::McpError::Protocol(error.to_string()))
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
    fn response_limits(&self) -> McpResponseLimits;
    async fn tools(
        &self,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Vec<EngineTool>>, BridgeError>;
    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Value>, BridgeError>;
    async fn create_session(
        &self,
        title: Option<String>,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<SessionSummary>, BridgeError>;
    async fn list_sessions(
        &self,
        authorized: Vec<String>,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Vec<SessionSummary>>, BridgeError>;
    async fn send_message(
        &self,
        session_id: &str,
        message: &str,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Value>, BridgeError>;
}

#[derive(Clone)]
pub struct RottweilerMcpServer {
    bridge: Arc<dyn EngineMcpBridge>,
    authority: Arc<McpServerAuthority>,
    request_timeout: Duration,
}

/// Creates an independently bounded authority for each owned MCP connection.
pub struct RottweilerMcpServerFactory {
    bridge: Arc<dyn EngineMcpBridge>,
    authority: Arc<dyn Fn() -> Result<McpServerAuthority, BridgeError> + Send + Sync>,
    request_timeout: Duration,
}

impl RottweilerMcpServerFactory {
    #[must_use]
    pub fn new(
        bridge: Arc<dyn EngineMcpBridge>,
        authority: impl Fn() -> Result<McpServerAuthority, BridgeError> + Send + Sync + 'static,
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

    pub fn create(&self) -> Result<RottweilerMcpServer, BridgeError> {
        Ok(RottweilerMcpServer {
            bridge: Arc::clone(&self.bridge),
            authority: Arc::new((self.authority)()?),
            request_timeout: self.request_timeout,
        })
    }
}

/// Host-minted least privilege for one MCP connection.
pub struct McpServerAuthority {
    allowed_tools: BTreeSet<String>,
    sessions: RwLock<BTreeSet<String>>,
    allow_create: bool,
    allow_list: bool,
    allow_send: bool,
    _retained: crate::payload_work::Allocation,
}

impl McpServerAuthority {
    pub fn new(
        allowed_tools: impl IntoIterator<Item = String>,
        explicit_sessions: impl IntoIterator<Item = String>,
    ) -> Result<Self, BridgeError> {
        let retained = crate::payload_work::Allocation::new(64 * 1024)
            .map_err(|_| BridgeError::safe("MCP authority allocation exhausted"))?;
        let mut tools = BTreeSet::new();
        for (index, name) in allowed_tools.into_iter().enumerate() {
            if index >= 64
                || name.is_empty()
                || name.len() > 256
                || name.chars().any(char::is_control)
            {
                return Err(BridgeError::safe("MCP tool authority exceeds its contract"));
            }
            tools.insert(name.as_str().to_owned());
        }
        let mut sessions = BTreeSet::new();
        for (index, id) in explicit_sessions.into_iter().enumerate() {
            if index >= MAX_SERVER_SESSIONS || rw_types::SessionId::validate(&id).is_err() {
                return Err(BridgeError::safe(
                    "MCP session authority exceeds its contract",
                ));
            }
            sessions.insert(id.as_str().to_owned());
        }
        Ok(Self {
            allowed_tools: tools,
            sessions: RwLock::new(sessions),
            allow_create: false,
            allow_list: false,
            allow_send: false,
            _retained: retained,
        })
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

fn parse<T: serde::de::DeserializeOwned>(
    request: &mut CallToolRequestParams,
) -> Result<T, McpProtocolError> {
    serde_json::from_value(Value::Object(request.arguments.take().unwrap_or_default()))
        .map_err(|error| McpProtocolError::invalid_params(error.to_string(), None))
}

fn result<T: Serialize>(
    value: McpResponse<T>,
) -> Result<McpResponse<CallToolResponse>, McpProtocolError> {
    use rw_types::json_encoding::JsonWriter;
    let mut count = JsonWriter::count(MAX_SERVER_RESULT);
    if count.serialize(&value.value).is_err() {
        return Ok(McpResponse::wire(
            tool_error("Rottweiler MCP server result exceeded its size cap"),
            value.retained,
        ));
    }
    // The source carrier remains held while text and structured representations coexist.
    let mut retained = crate::payload_work::Allocation::new(MAX_SERVER_RESULT * 2)
        .map_err(|_| McpProtocolError::internal_error("MCP result allocation exhausted", None))?;
    let mut bytes = Vec::with_capacity(count.written());
    JsonWriter::buffer(&mut bytes, MAX_SERVER_RESULT, 0)
        .and_then(|mut writer| {
            writer
                .serialize(&value.value)
                .map_err(std::io::Error::other)
        })
        .map_err(|_| McpProtocolError::internal_error("MCP result encoding failed", None))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| McpProtocolError::internal_error("MCP result encoding failed", None))?;
    let shape = crate::ingress::profile::inspect(text.as_bytes())
        .map_err(|_| McpProtocolError::internal_error("MCP result shape exceeded its cap", None))?;
    let working =
        crate::ingress::profile::working_bytes(&shape, std::mem::size_of::<Value>().max(128), 4)
            .map_err(|_| {
                McpProtocolError::internal_error("MCP result allocation exceeded its cap", None)
            })?;
    retained
        .ensure(working.saturating_add(text.capacity()))
        .map_err(|_| McpProtocolError::internal_error("MCP result allocation exhausted", None))?;
    let structured = serde_json::to_value(&value.value)
        .map_err(|_| McpProtocolError::internal_error("MCP result encoding failed", None))?;
    let mut response = CallToolResult::success(vec![ContentBlock::text(text)]);
    response.structured_content = Some(structured);
    drop(value.value);
    let mut leases = value.retained;
    leases.push(Arc::new(retained));
    Ok(McpResponse::wire(response.into(), leases))
}

fn tool_error(message: &str) -> CallToolResponse {
    CallToolResult::error(vec![ContentBlock::text(
        message.chars().take(512).collect::<String>(),
    )])
    .into()
}

impl RottweilerMcpServer {
    fn get_info() -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rottweiler", env!("CARGO_PKG_VERSION")))
            .with_instructions("Rottweiler coding-agent sessions and approved tools")
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_tool(
        &self,
        request: CallToolRequestParams,
        decoded: Arc<crate::payload_work::Allocation>,
    ) -> Result<McpResponse<CallToolResponse>, McpProtocolError> {
        let input = invocation::prepare(request, decoded).await?;
        let slot = McpResponseSlot::new(self.bridge.response_limits()).map_err(|_| {
            McpProtocolError::internal_error("MCP response allocation exhausted", None)
        })?;
        match input.value {
            invocation::Invocation::Tool(input) => {
                if input.name.len() > 256 || !self.authority.allowed_tools.contains(&input.name) {
                    return Ok(McpResponse::wire(
                        tool_error("tool is outside this MCP connection's authority"),
                        Vec::new(),
                    ));
                }
                bridge_result(
                    self.bridge
                        .call_tool(&input.name, input.arguments, slot)
                        .await,
                )
                .await
            }
            invocation::Invocation::Create(input) => {
                if !self.authority.allow_create {
                    return Ok(McpResponse::wire(
                        tool_error("session creation is outside this MCP connection's authority"),
                        Vec::new(),
                    ));
                }
                if input.title.as_ref().is_some_and(|title| title.len() > 512) {
                    return Ok(McpResponse::wire(
                        tool_error("session title exceeds its size cap"),
                        Vec::new(),
                    ));
                }
                let mut sessions = self.authority.sessions.write().await;
                if sessions.len() >= MAX_SERVER_SESSIONS {
                    return Ok(McpResponse::wire(
                        tool_error("MCP session authority is full"),
                        Vec::new(),
                    ));
                }
                match self.bridge.create_session(input.title, slot).await {
                    Ok(value) => {
                        if rw_types::SessionId::validate(&value.id).is_err() {
                            return Err(McpProtocolError::internal_error(
                                "engine returned an invalid session identity",
                                None,
                            ));
                        }
                        sessions.insert(value.id.clone());
                        encode_result(value).await
                    }
                    Err(error) => Ok(McpResponse::wire(
                        tool_error(&error.safe_message),
                        Vec::new(),
                    )),
                }
            }

            invocation::Invocation::List => {
                if !self.authority.allow_list {
                    return Ok(McpResponse::wire(
                        tool_error("session listing is outside this MCP connection's authority"),
                        Vec::new(),
                    ));
                }
                let allowed = self
                    .authority
                    .sessions
                    .read()
                    .await
                    .iter()
                    .cloned()
                    .collect();
                bridge_result(self.bridge.list_sessions(allowed, slot).await).await
            }
            invocation::Invocation::Send(input) => {
                if !self.authority.allow_send {
                    return Ok(McpResponse::wire(
                        tool_error("session messaging is outside this MCP connection's authority"),
                        Vec::new(),
                    ));
                }
                if rw_types::SessionId::validate(&input.session_id).is_err()
                    || input.message.len() > MAX_WIRE_TEXT
                    || !self
                        .authority
                        .sessions
                        .read()
                        .await
                        .contains(&input.session_id)
                {
                    return Ok(McpResponse::wire(
                        tool_error(
                            "session is outside this MCP connection's authority or input is oversized",
                        ),
                        Vec::new(),
                    ));
                }
                bridge_result(
                    self.bridge
                        .send_message(&input.session_id, &input.message, slot)
                        .await,
                )
                .await
            }
            invocation::Invocation::Oversized => Ok(McpResponse::wire(
                tool_error("MCP request arguments exceed the size cap"),
                Vec::new(),
            )),
            invocation::Invocation::Unknown => Err(McpProtocolError::method_not_found::<
                rmcp::model::CallToolRequestMethod,
            >()),
        }
    }
}

async fn bridge_result<T: Serialize + Send + 'static>(
    value: Result<McpResponse<T>, BridgeError>,
) -> Result<McpResponse<CallToolResponse>, McpProtocolError> {
    match value {
        Ok(value) => encode_result(value).await,
        Err(error) => Ok(McpResponse::wire(
            tool_error(&error.safe_message),
            Vec::new(),
        )),
    }
}

async fn encode_result<T: Serialize + Send + 'static>(
    value: McpResponse<T>,
) -> Result<McpResponse<CallToolResponse>, McpProtocolError> {
    rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || result(value))
        .await
        .map_err(|_| McpProtocolError::internal_error("MCP result encoder worker failed", None))?
}
