//! The host supplies policy-checked bytes; protocol parsing stays in the admitted client.
use crate::{McpError, payload_work::Allocation};
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpHttpMethod {
    Get,
    Post,
    Delete,
}

/// A bounded raw response. The client reserves chunk/header working storage before
/// issuing a request and keeps that reservation while polling or retaining chunks.
pub struct McpHttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Each owned chunk is at most four MiB; no response JSON has been decoded.
    pub body: BoxStream<'static, Result<Vec<u8>, McpError>>,
}

/// Required host HTTP authority. No ambient client, endpoint resolution or proxy
/// configuration is constructed inside MCP. Headers/body are already admitted by
/// the calling transport, whose physical request owner remains until completion.
#[async_trait]
pub trait McpHttpClient: Send + Sync {
    async fn request(
        &self,
        method: McpHttpMethod,
        uri: &str,
        headers: Vec<(String, String)>,
        body: McpHttpBody,
    ) -> Result<McpHttpResponse, McpError>;
}

/// Maximum number of headers in one complete MCP HTTP request.
pub const MCP_HTTP_MAX_HEADERS: usize = 32;
/// Maximum aggregate header name and value bytes.
pub const MCP_HTTP_MAX_HEADER_BYTES: usize = 32 * 1024;
/// Maximum bytes in one header value.
pub const MCP_HTTP_MAX_HEADER_VALUE_BYTES: usize = 8 * 1024;

/// Validate the complete header set before any HTTP effect. Callers additionally
/// enforce which authority may supply authentication and protocol headers.
/// This borrows admitted storage and never allocates a second header collection.
pub fn validate_mcp_http_headers(headers: &[(String, String)]) -> Result<(), McpError> {
    let invalid = || McpError::Policy("MCP HTTP headers exceed their contract".into());
    if headers.len() > MCP_HTTP_MAX_HEADERS {
        return Err(invalid());
    }
    let mut total = 0usize;
    for (name, value) in headers {
        if value.len() > MCP_HTTP_MAX_HEADER_VALUE_BYTES {
            return Err(invalid());
        }
        total = total
            .checked_add(name.len())
            .and_then(|bytes| bytes.checked_add(value.len()))
            .ok_or_else(invalid)?;
        if total > MCP_HTTP_MAX_HEADER_BYTES {
            return Err(invalid());
        }
        http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
        http::HeaderValue::from_str(value).map_err(|_| invalid())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;

/// Admitted request bytes stay owned by the network body, including any retained
/// Hyper buffers. Hosts may borrow these bytes or transfer this owner intact.
pub struct McpHttpBody {
    bytes: Vec<u8>,
    retained: Arc<Allocation>,
}
impl McpHttpBody {
    pub(crate) fn new(bytes: Vec<u8>, retained: Arc<Allocation>) -> Self {
        Self { bytes, retained }
    }
    pub(crate) fn retention(&self) -> Arc<Allocation> {
        Arc::clone(&self.retained)
    }
}
impl AsRef<[u8]> for McpHttpBody {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// Response metadata preserves the guarded HTTP receive contract independently
/// of outgoing standard/custom request headers.
pub const MCP_HTTP_MAX_RESPONSE_HEADERS: usize = 128;
pub const MCP_HTTP_MAX_RESPONSE_HEADER_BYTES: usize = 64 * 1024;

pub(crate) fn validate_response_headers(headers: &[(String, String)]) -> Result<(), McpError> {
    let invalid = || McpError::Protocol("MCP HTTP response headers exceed their contract".into());
    if headers.len() > MCP_HTTP_MAX_RESPONSE_HEADERS {
        return Err(invalid());
    }
    let mut total = 0usize;
    for (name, value) in headers {
        total = total
            .checked_add(name.len())
            .and_then(|n| n.checked_add(value.len()))
            .ok_or_else(invalid)?;
        if total > MCP_HTTP_MAX_RESPONSE_HEADER_BYTES
            || value.len() > MCP_HTTP_MAX_HEADER_VALUE_BYTES
            || http::HeaderName::from_bytes(name.as_bytes()).is_err()
            || http::HeaderValue::from_str(value).is_err()
        {
            return Err(invalid());
        }
    }
    Ok(())
}
