use std::{collections::BTreeMap, fmt, path::PathBuf, time::Duration};

use rw_tools::CapabilityManifest;
use rw_types::ToolCapability;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Stable identifier for one configured MCP server.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ServerId(pub String);

impl ServerId {
    /// Validates a server identifier before it is used as a namespace.
    pub fn new(value: impl Into<String>) -> Result<Self, McpError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 96
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(McpError::InvalidServerId(value));
        }
        Ok(Self(value))
    }
}

impl fmt::Display for ServerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A bearer token whose debug representation never exposes its bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretToken(String);

impl SecretToken {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretToken([REDACTED])")
    }
}

/// How a server is reached. Stdio is an argv vector and never a shell command.
#[derive(Clone, Eq, PartialEq)]
pub enum McpTransportConfig {
    Stdio {
        executable: PathBuf,
        args: Vec<String>,
        working_directory: Option<PathBuf>,
        environment: Vec<(String, String)>,
        sandbox: McpStdioSandboxPolicy,
    },
    StreamableHttp {
        endpoint: String,
        oauth: bool,
    },
}

impl fmt::Debug for McpTransportConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio {
                executable,
                args,
                working_directory,
                environment,
                sandbox,
            } => formatter
                .debug_struct("Stdio")
                .field("executable", executable)
                .field("args", args)
                .field("working_directory", working_directory)
                .field(
                    "environment_keys",
                    &environment.iter().map(|(key, _)| key).collect::<Vec<_>>(),
                )
                .field("sandbox", sandbox)
                .finish(),
            Self::StreamableHttp { endpoint, oauth } => formatter
                .debug_struct("StreamableHttp")
                .field("endpoint", endpoint)
                .field("oauth", oauth)
                .finish(),
        }
    }
}

/// Explicit per-server authority for a sandboxed stdio MCP process.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpStdioSandboxPolicy {
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub allowed_domains: Vec<String>,
}

/// One server registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServerConfig {
    pub id: ServerId,
    pub transport: McpTransportConfig,
    pub enabled: bool,
    /// `true` keeps schemas out of the provider prompt until `tool_search`.
    pub defer_tools: bool,
    /// User-owned permission classification overrides for virtual MCP tools.
    pub tool_capabilities: McpToolCapabilityOverrides,
}

/// Exact server-default and per-tool permission classification overrides.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpToolCapabilityOverrides {
    pub server_default: Option<CapabilityManifest>,
    pub tools: BTreeMap<String, CapabilityManifest>,
}

impl McpToolCapabilityOverrides {
    #[must_use]
    pub fn resolve(&self, tool: &str) -> CapabilityManifest {
        self.tools
            .get(tool)
            .cloned()
            .or_else(|| self.server_default.clone())
            .unwrap_or_else(McpToolDefinition::restrictive_capabilities)
    }
}

/// Runtime state exposed to `/mcp` and native/web clients.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerState {
    Disabled,
    Connecting,
    Ready,
    ApprovalRequired,
    Failed { message: String },
    Stopping,
}

/// Public status without transport credentials or process environment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerStatus {
    pub id: ServerId,
    pub enabled: bool,
    pub state: ServerState,
    pub tool_count: usize,
    pub resource_count: usize,
    pub prompt_count: usize,
}

/// The only MCP tool metadata included before an explicit search.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeferredTool {
    pub server: ServerId,
    pub name: String,
    pub description: String,
}

/// A full tool definition returned on demand.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct McpToolDefinition {
    pub server: ServerId,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub capabilities: CapabilityManifest,
}

impl McpToolDefinition {
    /// MCP manifests are untrusted, so network and process execution are the default.
    #[must_use]
    pub fn restrictive_capabilities() -> CapabilityManifest {
        CapabilityManifest::new([ToolCapability::Network, ToolCapability::Execute])
    }
}

/// A resource or prompt listing, namespaced by server.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct McpCatalogEntry {
    pub server: ServerId,
    pub name: String,
    pub description: String,
    pub uri: Option<String>,
}

/// Reference to a complete result written outside the provider context window.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OverflowReference {
    pub id: String,
    pub bytes: usize,
}

/// Compact model-facing result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CappedResponse {
    pub encoded: String,
    pub format: String,
    pub truncated: bool,
    pub overflow: Option<OverflowReference>,
}

/// Limits for model-facing MCP data and graceful shutdown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct McpLimits {
    pub response_bytes: usize,
    pub request_timeout: Duration,
    pub shutdown_timeout: Duration,
}

impl Default for McpLimits {
    fn default() -> Self {
        Self {
            response_bytes: 256 * 1024,
            request_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(3),
        }
    }
}

#[derive(Debug, Error)]
pub enum McpError {
    #[error("invalid MCP server id: {0}")]
    InvalidServerId(String),
    #[error("invalid MCP executable or argument: {0}")]
    InvalidCommand(String),
    #[error("MCP server is already registered: {0}")]
    DuplicateServer(ServerId),
    #[error("unknown MCP server: {0}")]
    UnknownServer(ServerId),
    #[error("MCP server is disabled: {0}")]
    Disabled(ServerId),
    #[error("MCP server is not connected: {0}")]
    NotConnected(ServerId),
    #[error("MCP transport policy rejected endpoint: {0}")]
    Policy(String),
    #[error("MCP protocol error: {0}")]
    Protocol(String),
    #[error("MCP response encoding failed: {0}")]
    Encoding(String),
    #[error("MCP overflow spool failed: {0}")]
    Spool(String),
    #[error("MCP shutdown timed out for server: {0}")]
    ShutdownTimeout(ServerId),
    #[error("MCP login required for server {server} and resource {resource}")]
    PendingLogin { server: ServerId, resource: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_tokens_are_redacted_from_debug_output() {
        let token = SecretToken::new("mcp-token-canary");
        assert!(!format!("{token:?}").contains("mcp-token-canary"));
        assert_eq!(token.expose(), "mcp-token-canary");
    }
}
