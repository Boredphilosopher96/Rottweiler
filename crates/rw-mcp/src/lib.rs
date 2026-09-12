//! MCP client/server integration with deferred schemas and fail-closed transport boundaries.
#![allow(clippy::missing_errors_doc)]

mod encoding;
mod http_io;
mod payload_work;
mod response;
pub use encoding::EncodedPayload;
pub use http_io::{
    MCP_HTTP_MAX_HEADER_BYTES, MCP_HTTP_MAX_HEADER_VALUE_BYTES, MCP_HTTP_MAX_HEADERS, McpHttpBody,
    McpHttpClient, McpHttpMethod, McpHttpResponse, validate_mcp_http_headers,
};
pub use response::{McpResponse, McpResponseLimits, McpResponseSlot};
mod client;
mod ingress;
mod manager;
mod server;
mod spool;
mod types;

#[cfg(feature = "test-support")]
pub use client::TestOnlyUnsandboxedStdioConnector;
pub use client::{
    McpAuthorizationProvider, McpClient, McpConnectionApprovalPolicy, McpConnector,
    McpInboundRouter, SandboxedStdioConnector, connect_http, sorted_headers,
};
pub use manager::MAX_DEFERRED_PROMPT_BYTES;
pub use manager::{CompactJsonEncoder, MAX_SERVERS, McpManager, StructuredResponseEncoder};
pub use server::{
    BridgeError, EngineMcpBridge, EngineTool, MAX_SERVER_SESSIONS, McpServerAuthority,
    RottweilerMcpServer, RottweilerMcpServerFactory, SessionSummary, serve_stdio,
};
pub use spool::{
    FilesystemSpool, OverflowSpool, PayloadRedactor, PayloadSource, RetainedPayloadWindow,
};
pub use types::*;

pub const COMPONENT: &str = "mcp";
