//! MCP client/server integration with deferred schemas and fail-closed transport boundaries.
#![allow(clippy::missing_errors_doc)]

mod client;
mod manager;
mod server;
mod spool;
mod types;

#[cfg(feature = "test-support")]
pub use client::{GuardedStreamableHttpConnector, TestOnlyUnsandboxedStdioConnector};
pub use client::{
    McpAuthorizationProvider, McpClient, McpConnectionApprovalPolicy, McpConnector,
    SandboxedStdioConnector, boxed_running_http_client, sorted_headers,
};
pub use manager::{CompactJsonEncoder, McpManager, StructuredResponseEncoder};
pub use server::{
    BridgeError, EngineMcpBridge, EngineTool, McpServerAuthority, RottweilerMcpServer,
    RottweilerMcpServerFactory, SessionSummary, serve_stdio,
};
pub use spool::{FilesystemSpool, OverflowSpool};
pub use types::*;

pub const COMPONENT: &str = "mcp";
