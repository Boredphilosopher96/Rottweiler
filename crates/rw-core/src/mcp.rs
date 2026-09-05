mod oauth;
pub use oauth::*;

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    fmt,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures_util::{StreamExt as _, stream::BoxStream};
use http::{HeaderName, HeaderValue};
use rmcp::{
    ServiceExt as _,
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    transport::streamable_http_client::{
        SseError, StreamableHttpClient, StreamableHttpClientTransport,
        StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
    },
};
use rw_context::encode_toon;
use rw_mcp::{
    McpAuthorizationProvider, McpConnectionApprovalPolicy, McpConnector, McpError, McpManager,
    McpServerConfig, McpTransportConfig, OverflowReference, OverflowSpool, SecretToken,
    StructuredResponseEncoder, boxed_running_http_client,
};
use rw_providers::{
    AuthMaterial, AuthProvider, DEFAULT_OAUTH_CALLBACK_TIMEOUT, OAuthAuthorizationCode,
    OAuthAuthorizationCodeConfig, OAuthLoginSession, OAuthRefreshConfig, OAuthTokenSet,
    ProviderError, ProviderErrorKind, RefreshTokenSink, RefreshingOAuth, Secret as ProviderSecret,
};
use rw_store::credentials::{
    CredentialEnvironment, CredentialManager, CredentialReference, CredentialStore,
};
use rw_tools::{
    CapabilityManifest, EgressDecision, EgressPin, EgressPolicy, SupervisedEgressProxy, Tool,
    ToolContext, ToolDescriptor, ToolError, ToolRegistry, ToolResult, UpstreamProxy,
    WorkspaceBinding,
};
use rw_types::{McpServerId, ToolCapability};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sse_stream::{Sse, SseStream};
use url::Url;

const UNTRUSTED_OPEN: &str = "<rottweiler_untrusted_mcp_output_v1>\n";
const UNTRUSTED_CLOSE: &str = "\n</rottweiler_untrusted_mcp_output_v1>";
const MAX_TOOL_SEARCH_WIRE_BYTES: usize = 192 * 1024;
const MAX_OVERFLOW_READ_BYTES: usize = 192 * 1024;
const MCP_HTTP_MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
const MCP_HTTP_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MCP_HTTP_MAX_CUSTOM_HEADERS: usize = 32;
const MCP_HTTP_MAX_HEADER_BYTES: usize = 32 * 1024;
const MCP_HTTP_MAX_HEADER_VALUE_BYTES: usize = 8 * 1024;
const MCP_HTTP_MAX_SESSION_ID_BYTES: usize = 256;
const MCP_HTTP_MAX_EVENT_ID_BYTES: usize = 512;

/// Production rmcp HTTP client backed exclusively by the guarded provider
/// transport. Direct destinations are DNS-pinned after validating the complete
/// answer set; configured proxies must already be loopback policy proxies.
#[derive(Clone)]
pub struct ProductionMcpHttpClient {
    policy_proxies: BTreeMap<String, Arc<McpPolicyProxy>>,
    loopback_authorities: BTreeMap<String, LoopbackMcpAuthority>,
    response_deadline: Duration,
    frame_deadline: Duration,
}

#[derive(Clone)]
pub struct LoopbackMcpAuthority {
    origin: String,
}

impl LoopbackMcpAuthority {
    /// Mints authority for exactly one validated loopback origin.
    ///
    /// # Errors
    ///
    /// Returns an error for non-loopback or unsupported endpoint origins.
    pub fn for_endpoint(endpoint: &Url) -> Result<Self, ProductionMcpHttpError> {
        let host = endpoint.host_str().ok_or(ProductionMcpHttpError)?;
        let loopback = host.eq_ignore_ascii_case("localhost")
            || parse_mcp_url_ip(host).is_some_and(|address| address.is_loopback());
        if !loopback || !matches!(endpoint.scheme(), "http" | "https") {
            return Err(ProductionMcpHttpError);
        }
        Ok(Self {
            origin: mcp_origin(endpoint)?,
        })
    }
}

pub struct McpPolicyProxy {
    endpoint_origin: String,
    proxy: SupervisedEgressProxy,
}

impl McpPolicyProxy {
    /// Starts a policy proxy pinned to the complete public DNS answer set.
    ///
    /// # Errors
    ///
    /// Returns an error for non-HTTPS, private, unresolved, or unpinnable endpoints.
    pub async fn start(
        endpoint: &Url,
        upstream: Option<UpstreamProxy>,
    ) -> Result<Self, ProductionMcpHttpError> {
        if endpoint.scheme() != "https" {
            return Err(ProductionMcpHttpError);
        }
        let host = endpoint.host_str().ok_or(ProductionMcpHttpError)?;
        let port = endpoint
            .port_or_known_default()
            .ok_or(ProductionMcpHttpError)?;
        let addresses = tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| ProductionMcpHttpError)?
            .collect::<Vec<_>>();
        let ips = addresses.iter().map(SocketAddr::ip).collect::<Vec<_>>();
        let policy = EgressPolicy::new([host]);
        if policy.evaluate(host, &ips) != EgressDecision::Allowed {
            return Err(ProductionMcpHttpError);
        }
        let pin = EgressPin::new(host, port, addresses).map_err(|_| ProductionMcpHttpError)?;
        let proxy =
            SupervisedEgressProxy::start_with_upstream_and_pins(policy, upstream, vec![pin])
                .map_err(|_| ProductionMcpHttpError)?;
        Ok(Self {
            endpoint_origin: mcp_origin(endpoint)?,
            proxy,
        })
    }

    fn url_for(&self, endpoint: &Url) -> Result<Url, ProductionMcpHttpError> {
        if self.endpoint_origin != mcp_origin(endpoint)? {
            return Err(ProductionMcpHttpError);
        }
        Url::parse(&self.proxy.url()).map_err(|_| ProductionMcpHttpError)
    }
}

/// Production remote-MCP connector. This is the only non-test constructor for
/// rmcp's Streamable HTTP transport in the workspace.
pub struct ProductionMcpHttpConnector {
    client: ProductionMcpHttpClient,
    authorization: Arc<dyn McpAuthorizationProvider>,
    approval: Arc<dyn McpConnectionApprovalPolicy>,
    channel_capacity: usize,
}

impl ProductionMcpHttpConnector {
    #[must_use]
    pub fn new(
        client: ProductionMcpHttpClient,
        authorization: Arc<dyn McpAuthorizationProvider>,
        approval: Arc<dyn McpConnectionApprovalPolicy>,
    ) -> Self {
        Self {
            client,
            authorization,
            approval,
            channel_capacity: 16,
        }
    }

    #[must_use]
    pub fn with_channel_capacity(mut self, capacity: usize) -> Self {
        self.channel_capacity = capacity.clamp(1, 256);
        self
    }
}

#[async_trait]
impl McpConnector for ProductionMcpHttpConnector {
    async fn connect(
        &self,
        config: &McpServerConfig,
    ) -> Result<Arc<dyn rw_mcp::McpClient>, McpError> {
        let McpTransportConfig::StreamableHttp { endpoint, oauth } = &config.transport else {
            return Err(McpError::Policy(
                "remote MCP connector requires HTTP transport".to_owned(),
            ));
        };
        self.approval.approve(config).await?;
        let token = if *oauth {
            self.authorization.token(&config.id, endpoint).await?
        } else {
            None
        };
        let mut transport_config = StreamableHttpClientTransportConfig::with_uri(endpoint.clone());
        transport_config.channel_buffer_capacity = self.channel_capacity;
        if let Some(token) = token {
            transport_config = transport_config.auth_header(token.expose().to_owned());
        }
        let transport =
            StreamableHttpClientTransport::with_client(self.client.clone(), transport_config);
        let service = ().serve(transport).await.map_err(|_| {
            McpError::Protocol("remote MCP protocol initialization failed".to_owned())
        })?;
        Ok(boxed_running_http_client(config.id.clone(), service))
    }
}

impl ProductionMcpHttpClient {
    #[must_use]
    pub fn new() -> Self {
        Self {
            policy_proxies: BTreeMap::new(),
            loopback_authorities: BTreeMap::new(),
            response_deadline: Duration::from_secs(30),
            frame_deadline: Duration::from_secs(10),
        }
    }

    #[must_use]
    pub fn with_policy_proxy(mut self, proxy: Arc<McpPolicyProxy>) -> Self {
        self.policy_proxies
            .insert(proxy.endpoint_origin.clone(), proxy);
        self
    }

    #[must_use]
    pub fn with_loopback_authority(mut self, authority: LoopbackMcpAuthority) -> Self {
        self.loopback_authorities
            .insert(authority.origin.clone(), authority);
        self
    }

    async fn request(
        &self,
        method: rw_providers::GuardedHttpMethod,
        uri: &str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<rw_providers::GuardedHttpStreamResponse, ProductionMcpHttpError> {
        let url = Url::parse(uri).map_err(|_| ProductionMcpHttpError)?;
        let host = url.host_str().ok_or(ProductionMcpHttpError)?;
        let port = url.port_or_known_default().ok_or(ProductionMcpHttpError)?;
        let addresses = tokio::net::lookup_host((host.trim_matches(['[', ']']), port))
            .await
            .map_err(|_| ProductionMcpHttpError)?
            .collect::<Vec<_>>();
        if addresses.is_empty() {
            return Err(ProductionMcpHttpError);
        }
        let ips = addresses.iter().map(SocketAddr::ip).collect::<Vec<_>>();
        let loopback = ips.iter().all(IpAddr::is_loopback);
        let origin = mcp_origin(&url)?;
        if loopback {
            if self
                .loopback_authorities
                .get(&origin)
                .is_none_or(|authority| authority.origin != origin)
                || url.scheme() != "http" && url.scheme() != "https"
            {
                return Err(ProductionMcpHttpError);
            }
        } else {
            if url.scheme() != "https" {
                return Err(ProductionMcpHttpError);
            }
            let policy = EgressPolicy::new([host]);
            if policy.evaluate(host, &ips) != EgressDecision::Allowed {
                return Err(ProductionMcpHttpError);
            }
        }
        let proxy = self
            .policy_proxies
            .get(&origin)
            .map(|proxy| proxy.url_for(&url))
            .transpose()?;
        let dns_pin = proxy.is_none().then(|| (host.to_owned(), addresses[0]));
        rw_providers::guarded_http_request(rw_providers::GuardedHttpRequest {
            method,
            url,
            headers,
            body,
            proxy,
            proxy_authentication: None,
            dns_pin,
            allow_private_destinations: loopback,
            response_deadline: self.response_deadline,
            frame_deadline: self.frame_deadline,
            max_frame_bytes: MCP_HTTP_MAX_FRAME_BYTES,
            max_body_bytes: MCP_HTTP_MAX_BODY_BYTES,
        })
        .await
        .map_err(|_| ProductionMcpHttpError)
    }
}

impl Default for ProductionMcpHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_mcp_url_ip(host: &str) -> Option<IpAddr> {
    host.trim_matches(['[', ']']).parse().ok()
}

fn mcp_origin(url: &Url) -> Result<String, ProductionMcpHttpError> {
    let host = url.host_str().ok_or(ProductionMcpHttpError)?;
    let port = url.port_or_known_default().ok_or(ProductionMcpHttpError)?;
    Ok(format!(
        "{}://{}:{port}",
        url.scheme(),
        host.to_ascii_lowercase()
    ))
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("guarded MCP HTTP request failed")]
pub struct ProductionMcpHttpError;

fn mcp_http_headers(
    auth: Option<String>,
    session: Option<&str>,
    last_event: Option<String>,
    custom: HashMap<HeaderName, HeaderValue>,
    json_body: bool,
) -> Result<Vec<(String, String)>, ProductionMcpHttpError> {
    if custom.len() > MCP_HTTP_MAX_CUSTOM_HEADERS {
        return Err(ProductionMcpHttpError);
    }
    let mut headers = Vec::with_capacity(custom.len() + 5);
    headers.push((
        "accept".to_owned(),
        "text/event-stream, application/json".to_owned(),
    ));
    if json_body {
        headers.push(("content-type".to_owned(), "application/json".to_owned()));
    }
    if let Some(token) = auth {
        if token.is_empty() || token.len() > MCP_HTTP_MAX_HEADER_VALUE_BYTES {
            return Err(ProductionMcpHttpError);
        }
        HeaderValue::from_str(&token).map_err(|_| ProductionMcpHttpError)?;
        headers.push(("authorization".to_owned(), format!("Bearer {token}")));
    }
    if let Some(session) = session {
        if !valid_mcp_header_id(session, MCP_HTTP_MAX_SESSION_ID_BYTES) {
            return Err(ProductionMcpHttpError);
        }
        HeaderValue::from_str(session).map_err(|_| ProductionMcpHttpError)?;
        headers.push(("mcp-session-id".to_owned(), session.to_owned()));
    }
    if let Some(last_event) = last_event {
        if !valid_mcp_header_id(&last_event, MCP_HTTP_MAX_EVENT_ID_BYTES) {
            return Err(ProductionMcpHttpError);
        }
        HeaderValue::from_str(&last_event).map_err(|_| ProductionMcpHttpError)?;
        headers.push(("last-event-id".to_owned(), last_event));
    }
    for (name, value) in custom {
        if matches!(
            name.as_str(),
            "authorization" | "accept" | "content-type" | "mcp-session-id" | "last-event-id"
        ) {
            return Err(ProductionMcpHttpError);
        }
        if value.as_bytes().len() > MCP_HTTP_MAX_HEADER_VALUE_BYTES {
            return Err(ProductionMcpHttpError);
        }
        headers.push((
            name.to_string(),
            value
                .to_str()
                .map_err(|_| ProductionMcpHttpError)?
                .to_owned(),
        ));
    }
    if headers.iter().fold(0_usize, |total, (name, value)| {
        total.saturating_add(name.len()).saturating_add(value.len())
    }) > MCP_HTTP_MAX_HEADER_BYTES
    {
        return Err(ProductionMcpHttpError);
    }
    Ok(headers)
}

fn valid_mcp_header_id(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value.bytes().all(|byte| matches!(byte, 0x21..=0x7e))
}

fn response_header(
    response: &rw_providers::GuardedHttpStreamResponse,
    name: &str,
) -> Option<String> {
    response
        .headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

async fn collect_guarded_body(
    mut body: rw_providers::GuardedHttpByteStream,
) -> Result<Vec<u8>, ProductionMcpHttpError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        bytes.extend(chunk.map_err(|_| ProductionMcpHttpError)?);
    }
    Ok(bytes)
}

#[allow(clippy::too_many_lines)]
impl StreamableHttpClient for ProductionMcpHttpClient {
    type Error = ProductionMcpHttpError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let body = serde_json::to_vec(&message)
            .map_err(|_| StreamableHttpError::Client(ProductionMcpHttpError))?;
        let response = self
            .request(
                rw_providers::GuardedHttpMethod::Post,
                &uri,
                mcp_http_headers(
                    auth_header,
                    session_id.as_deref().map(AsRef::as_ref),
                    None,
                    custom_headers,
                    true,
                )
                .map_err(StreamableHttpError::Client)?,
                body,
            )
            .await
            .map_err(StreamableHttpError::Client)?;
        let status = response.status;
        let content_type = response_header(&response, "content-type");
        let returned_session = response_header(&response, "mcp-session-id");
        if returned_session
            .as_deref()
            .is_some_and(|session| !valid_mcp_header_id(session, MCP_HTTP_MAX_SESSION_ID_BYTES))
        {
            return Err(StreamableHttpError::Client(ProductionMcpHttpError));
        }
        if matches!(status, 202 | 204) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == 404 && session_id.is_some() {
            return Err(StreamableHttpError::SessionExpired);
        }
        if !(200..300).contains(&status) {
            return Err(StreamableHttpError::UnexpectedServerResponse(
                Cow::Borrowed("MCP HTTP server returned an unsuccessful status"),
            ));
        }
        if content_type
            .as_deref()
            .is_some_and(|value| value.starts_with("text/event-stream"))
        {
            let bytes = response.body.map(|chunk| chunk.map(bytes::Bytes::from));
            let stream: BoxStream<'static, Result<Sse, SseError>> =
                SseStream::from_bytes_stream(bytes).boxed();
            return Ok(StreamableHttpPostResponse::Sse(stream, returned_session));
        }
        if content_type
            .as_deref()
            .is_some_and(|value| value.starts_with("application/json"))
        {
            let bytes = collect_guarded_body(response.body)
                .await
                .map_err(StreamableHttpError::Client)?;
            let message = serde_json::from_slice::<ServerJsonRpcMessage>(&bytes)?;
            return Ok(StreamableHttpPostResponse::Json(message, returned_session));
        }
        Err(StreamableHttpError::UnexpectedContentType(content_type))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let response = self
            .request(
                rw_providers::GuardedHttpMethod::Delete,
                &uri,
                mcp_http_headers(auth_header, Some(&session_id), None, custom_headers, false)
                    .map_err(StreamableHttpError::Client)?,
                Vec::new(),
            )
            .await
            .map_err(StreamableHttpError::Client)?;
        if response.status == 405 || (200..300).contains(&response.status) {
            Ok(())
        } else {
            Err(StreamableHttpError::UnexpectedServerResponse(
                Cow::Borrowed("MCP HTTP session deletion failed"),
            ))
        }
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let response = self
            .request(
                rw_providers::GuardedHttpMethod::Get,
                &uri,
                mcp_http_headers(
                    auth_header,
                    session_id.as_deref(),
                    last_event_id,
                    custom_headers,
                    false,
                )
                .map_err(StreamableHttpError::Client)?,
                Vec::new(),
            )
            .await
            .map_err(StreamableHttpError::Client)?;
        if response.status == 405 {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        if !(200..300).contains(&response.status) {
            return Err(StreamableHttpError::UnexpectedServerResponse(
                Cow::Borrowed("MCP HTTP stream request failed"),
            ));
        }
        let content_type = response_header(&response, "content-type");
        if !content_type
            .as_deref()
            .is_some_and(|value| value.starts_with("text/event-stream"))
        {
            return Err(StreamableHttpError::UnexpectedContentType(content_type));
        }
        let bytes = response.body.map(|chunk| chunk.map(bytes::Bytes::from));
        Ok(SseStream::from_bytes_stream(bytes).boxed())
    }
}

/// Vault reference and OAuth resource binding for one MCP server.
#[derive(Clone, Eq, PartialEq)]
pub struct McpOAuthBinding {
    pub token_reference: CredentialReference,
    /// Expected MCP resource for this server.
    pub resource: String,
    /// Expected OAuth audience for this server.
    pub audience: String,
    /// Refresh configuration captured from the same trusted server config.
    pub refresh: Option<McpOAuthRefreshBinding>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct McpOAuthRefreshBinding {
    pub token_endpoint: Url,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub proxy: Option<Url>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredMcpOAuthCredential {
    version: u16,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at_unix_seconds: Option<u64>,
    resource: String,
    audience: String,
    #[serde(default)]
    token_endpoint: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    proxy: Option<String>,
}

/// Encodes token plus issuance metadata as one opaque vault value. Storing this
/// single value makes the token and its resource/audience binding atomic.
///
/// # Errors
///
/// Returns an error for empty/oversized metadata or encoding failure.
pub fn encode_mcp_oauth_credential(
    access_token: &SecretToken,
    resource: impl Into<String>,
    audience: impl Into<String>,
) -> Result<rw_store::credentials::Secret<String>, McpError> {
    let resource = resource.into();
    let audience = audience.into();
    if access_token.expose().is_empty()
        || access_token.expose().len() > 64 * 1024
        || resource.is_empty()
        || resource.len() > 4 * 1024
        || audience.is_empty()
        || audience.len() > 4 * 1024
    {
        return Err(McpError::Policy(
            "MCP OAuth credential metadata is invalid".to_owned(),
        ));
    }
    let encoded = serde_json::to_string(&StoredMcpOAuthCredential {
        version: 2,
        access_token: access_token.expose().to_owned(),
        refresh_token: None,
        expires_at_unix_seconds: None,
        resource,
        audience,
        token_endpoint: None,
        client_id: None,
        scopes: Vec::new(),
        proxy: None,
    })
    .map_err(|_| McpError::Encoding("MCP OAuth credential encoding failed".to_owned()))?;
    Ok(rw_store::credentials::Secret::new(encoded))
}

fn encode_mcp_oauth_token_set(
    tokens: &OAuthTokenSet,
    resource: String,
    audience: String,
    token_endpoint: &Url,
    client_id: &str,
    scopes: &[String],
    proxy: Option<&Url>,
) -> Result<rw_store::credentials::Secret<String>, McpError> {
    let access_token = tokens.access_token().expose_secret();
    let refresh_token = tokens
        .refresh_token()
        .map(ProviderSecret::expose_secret)
        .map(str::to_owned);
    if access_token.is_empty()
        || access_token.len() > 64 * 1024
        || refresh_token
            .as_ref()
            .is_some_and(|token| token.is_empty() || token.len() > 64 * 1024)
        || resource.is_empty()
        || resource.len() > 4 * 1024
        || audience.is_empty()
        || audience.len() > 4 * 1024
        || client_id.is_empty()
        || client_id.len() > 4 * 1024
        || scopes.len() > 64
        || scopes
            .iter()
            .any(|scope| scope.is_empty() || scope.len() > 1024)
    {
        return Err(McpError::Policy(
            "MCP OAuth credential metadata is invalid".to_owned(),
        ));
    }
    let issued_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| McpError::Policy("system clock is before the Unix epoch".to_owned()))?
        .as_secs();
    let expires_at_unix_seconds = tokens
        .expires_in()
        .map(|lifetime| {
            issued_at
                .checked_add(lifetime)
                .ok_or_else(|| McpError::Policy("MCP OAuth token expiry overflowed".to_owned()))
        })
        .transpose()?;
    let encoded = serde_json::to_string(&StoredMcpOAuthCredential {
        version: 2,
        access_token: access_token.to_owned(),
        refresh_token,
        expires_at_unix_seconds,
        resource,
        audience,
        token_endpoint: Some(token_endpoint.as_str().to_owned()),
        client_id: Some(client_id.to_owned()),
        scopes: scopes.to_vec(),
        proxy: proxy.map(|url| url.as_str().to_owned()),
    })
    .map_err(|_| McpError::Encoding("MCP OAuth credential encoding failed".to_owned()))?;
    Ok(rw_store::credentials::Secret::new(encoded))
}

impl fmt::Debug for McpOAuthBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpOAuthBinding")
            .field("token_reference", &self.token_reference.identifier())
            .field("resource", &self.resource)
            .field("audience", &self.audience)
            .field("refresh", &self.refresh)
            .finish()
    }
}

impl fmt::Debug for McpOAuthRefreshBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpOAuthRefreshBinding")
            .field("token_endpoint", &self.token_endpoint)
            .field("client_id", &self.client_id)
            .field("scopes", &self.scopes)
            .field("proxy", &self.proxy)
            .finish()
    }
}

/// CredentialManager-backed MCP token resolver. Tokens never implement Debug
/// or serialization and are returned only to the authenticated transport edge.
pub struct VaultMcpTokenProvider<E, K> {
    credentials: Arc<CredentialManager<E, K>>,
    bindings: BTreeMap<McpServerId, McpOAuthBinding>,
    refreshers: tokio::sync::Mutex<BTreeMap<McpServerId, Arc<RefreshingOAuth>>>,
}

impl<E, K> fmt::Debug for VaultMcpTokenProvider<E, K> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VaultMcpTokenProvider")
            .field("servers", &self.bindings.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl<E, K> VaultMcpTokenProvider<E, K> {
    #[must_use]
    pub fn new(
        credentials: Arc<CredentialManager<E, K>>,
        bindings: BTreeMap<McpServerId, McpOAuthBinding>,
    ) -> Self {
        Self {
            credentials,
            bindings,
            refreshers: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }
}

struct McpRefreshTokenSink<E, K> {
    credentials: Arc<CredentialManager<E, K>>,
    reference: CredentialReference,
}

impl<E, K> fmt::Debug for McpRefreshTokenSink<E, K> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpRefreshTokenSink")
            .field("reference", &self.reference.identifier())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl<E, K> RefreshTokenSink for McpRefreshTokenSink<E, K>
where
    E: CredentialEnvironment + Send + Sync + 'static,
    K: CredentialStore + Send + Sync + 'static,
{
    async fn persist(&self, refresh_token: &ProviderSecret) -> Result<(), ProviderError> {
        let resolved = self.credentials.resolve(&self.reference).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "MCP OAuth credential could not be reopened for rotation",
            )
        })?;
        let mut stored: StoredMcpOAuthCredential =
            serde_json::from_str(resolved.secret().expose_secret()).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "MCP OAuth credential could not be decoded for rotation",
                )
            })?;
        if stored.version != 2 {
            return Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                "unsupported MCP OAuth credential version",
            ));
        }
        stored.refresh_token = Some(refresh_token.expose_secret().to_owned());
        let encoded = serde_json::to_string(&stored).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "MCP OAuth credential rotation could not be encoded",
            )
        })?;
        self.credentials
            .store(
                &self.reference,
                &rw_store::credentials::Secret::new(encoded),
            )
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "MCP OAuth credential rotation could not be stored",
                )
            })?;
        Ok(())
    }
}

impl<E, K> VaultMcpTokenProvider<E, K>
where
    E: CredentialEnvironment + Send + Sync + 'static,
    K: CredentialStore + Send + Sync + 'static,
{
    fn load_credential(
        &self,
        server: &McpServerId,
        resource: &str,
        binding: &McpOAuthBinding,
    ) -> Result<StoredMcpOAuthCredential, McpError> {
        let pending = || McpError::PendingLogin {
            server: server.clone(),
            resource: resource.to_owned(),
        };
        let resolved = self
            .credentials
            .resolve(&binding.token_reference)
            .map_err(|_| pending())?;
        let stored: StoredMcpOAuthCredential =
            serde_json::from_str(resolved.secret().expose_secret()).map_err(|_| pending())?;
        if !matches!(stored.version, 1 | 2)
            || stored.access_token.is_empty()
            || stored.resource != binding.resource
            || stored.audience != binding.audience
        {
            return Err(McpError::Policy(
                "MCP OAuth credential metadata does not match its resource binding".to_owned(),
            ));
        }
        Ok(stored)
    }

    async fn refresher(
        &self,
        server: &McpServerId,
        binding: &McpOAuthBinding,
        refresh: &McpOAuthRefreshBinding,
        refresh_token: &str,
    ) -> Result<Arc<RefreshingOAuth>, McpError> {
        let mut refreshers = self.refreshers.lock().await;
        if let Some(existing) = refreshers.get(server) {
            return Ok(existing.clone());
        }
        let sink: Arc<dyn RefreshTokenSink> = Arc::new(McpRefreshTokenSink {
            credentials: self.credentials.clone(),
            reference: binding.token_reference.clone(),
        });
        let source = RefreshingOAuth::with_proxy_and_sink(
            OAuthRefreshConfig {
                token_endpoint: refresh.token_endpoint.clone(),
                client_id: refresh.client_id.clone(),
                client_secret: None,
                refresh_token: ProviderSecret::new(refresh_token),
                scope: (!refresh.scopes.is_empty()).then(|| refresh.scopes.join(" ")),
            },
            refresh.proxy.as_ref(),
            None,
            sink,
        )
        .map_err(|_| McpError::Policy("MCP OAuth refresh configuration is invalid".to_owned()))?;
        let source = Arc::new(source);
        refreshers.insert(server.clone(), source.clone());
        Ok(source)
    }
}

#[async_trait]
impl<E, K> McpAuthorizationProvider for VaultMcpTokenProvider<E, K>
where
    E: CredentialEnvironment + Send + Sync + 'static,
    K: CredentialStore + Send + Sync + 'static,
{
    async fn token(
        &self,
        server: &McpServerId,
        resource: &str,
    ) -> Result<Option<SecretToken>, McpError> {
        let binding = self
            .bindings
            .get(server)
            .ok_or_else(|| McpError::PendingLogin {
                server: server.clone(),
                resource: resource.to_owned(),
            })?;
        if binding.resource != resource
            || binding.audience.is_empty()
            || binding.token_reference.environment_variable().is_some()
        {
            return Err(McpError::Policy(
                "MCP OAuth token resource/audience binding is invalid".to_owned(),
            ));
        }
        let stored = self.load_credential(server, resource, binding)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| McpError::Policy("system clock is before the Unix epoch".to_owned()))?
            .as_secs();
        if stored
            .expires_at_unix_seconds
            .is_none_or(|expiry| expiry > now.saturating_add(30))
        {
            return Ok(Some(SecretToken::new(stored.access_token)));
        }
        let refresh_token =
            stored
                .refresh_token
                .as_deref()
                .ok_or_else(|| McpError::PendingLogin {
                    server: server.clone(),
                    resource: resource.to_owned(),
                })?;
        let refresh = binding
            .refresh
            .as_ref()
            .ok_or_else(|| McpError::PendingLogin {
                server: server.clone(),
                resource: resource.to_owned(),
            })?;
        if stored.token_endpoint.as_deref() != Some(refresh.token_endpoint.as_str())
            || stored.client_id.as_deref() != Some(refresh.client_id.as_str())
            || stored.scopes != refresh.scopes
            || stored.proxy.as_deref() != refresh.proxy.as_ref().map(Url::as_str)
        {
            return Err(McpError::Policy(
                "MCP OAuth refresh metadata does not match trusted configuration".to_owned(),
            ));
        }
        let refresher = self
            .refresher(server, binding, refresh, refresh_token)
            .await?;
        let material = refresher.material().await.map_err(|error| {
            McpError::Protocol(format!("MCP OAuth token refresh failed: {error}"))
        })?;
        match material {
            AuthMaterial::Bearer(token) => Ok(Some(SecretToken::new(token.expose_secret()))),
            _ => Err(McpError::Protocol(
                "MCP OAuth refresh returned non-bearer material".to_owned(),
            )),
        }
    }
}

/// Pinned structured encoder used by production MCP responses.
pub struct ToonMcpEncoder;

impl StructuredResponseEncoder for ToonMcpEncoder {
    fn encode(&self, value: &Value) -> Result<Vec<u8>, McpError> {
        encode_toon(value)
            .map(String::into_bytes)
            .map_err(|error| McpError::Encoding(error.to_string()))
    }

    fn format(&self) -> &'static str {
        "toon"
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ToolSearchInput {
    query: String,
    #[serde(default)]
    server: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct McpCallInput {
    server: String,
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct McpResourceInput {
    server: String,
    uri: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct McpPromptInput {
    server: String,
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct McpOverflowInput {
    id: String,
    bytes: usize,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    offset: usize,
}

/// Registers the complete built-in MCP model surface through the ordinary tool API.
///
/// # Errors
///
/// Returns when a tool name is already registered or a descriptor is invalid.
pub fn register_mcp_tools(
    registry: &mut ToolRegistry,
    manager: Arc<McpManager>,
    spool: Arc<dyn OverflowSpool>,
) -> Result<(), ToolError> {
    registry.register(Arc::new(ToolSearchTool {
        manager: Arc::clone(&manager),
    }))?;
    registry.register(Arc::new(McpCallTool {
        manager: Arc::clone(&manager),
    }))?;
    registry.register(Arc::new(McpResourceTool {
        manager: Arc::clone(&manager),
    }))?;
    registry.register(Arc::new(McpPromptTool { manager }))?;
    registry.register(Arc::new(McpOverflowReadTool { spool }))?;
    Ok(())
}

struct ToolSearchTool {
    manager: Arc<McpManager>,
}

#[async_trait]
impl Tool for ToolSearchTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        self.manager.settle_effects().await.map_err(|_| {
            ToolError::EffectsUnsettled("MCP invocation effects remain owned".to_owned())
        })
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "tool_search".to_owned(),
            description: "Search deferred MCP tools and load only matching full schemas."
                .to_owned(),
            input_schema: schema::<ToolSearchInput>(),
            capabilities: CapabilityManifest::default(),
        }
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    async fn execute(
        &self,
        invocation: &ToolContext,
        input: Value,
    ) -> Result<ToolResult, ToolError> {
        let input: ToolSearchInput = parse(input)?;
        if input.query.len() > 512 {
            return Err(ToolError::InvalidInput(
                "tool_search query exceeds 512 bytes".to_owned(),
            ));
        }
        let server = input
            .server
            .map(McpServerId::new)
            .transpose()
            .map_err(mcp_tool_error)?;
        let matches = self
            .manager
            .tool_search(&input.query, server.as_ref())
            .await
            .into_iter()
            .filter(|definition| {
                invocation
                    .mcp_tool_policy()
                    .allows(definition.server.as_str(), &definition.name)
            })
            .collect::<Vec<_>>();
        let total_matches = matches.len();
        let mut retained = Vec::new();
        for definition in matches {
            retained.push(definition);
            if serde_json::to_vec(&retained)
                .is_ok_and(|encoded| encoded.len() > MAX_TOOL_SEARCH_WIRE_BYTES)
            {
                retained.pop();
                break;
            }
        }
        let truncated = retained.len() < total_matches;
        let data = json!({
            "matches": retained,
            "truncated": truncated,
        });
        let content = encode_toon(&data).map_err(|error| ToolError::Output(error.to_string()))?;
        Ok(untrusted_result(&content, data))
    }
}

struct McpCallTool {
    manager: Arc<McpManager>,
}

#[async_trait]
impl Tool for McpCallTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        self.manager.settle_effects().await.map_err(|_| {
            ToolError::EffectsUnsettled("MCP invocation effects remain owned".to_owned())
        })
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "mcp_call".to_owned(),
            description: "Call one exact approved MCP tool loaded through tool_search.".to_owned(),
            input_schema: schema::<McpCallInput>(),
            capabilities: CapabilityManifest::default(),
        }
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    fn invocation_capabilities(&self, input: &Value) -> Result<CapabilityManifest, ToolError> {
        let input: McpCallInput = parse(input.clone())?;
        validate_wire_name(&input.name, "MCP tool")?;
        let server = McpServerId::new(input.server).map_err(mcp_tool_error)?;
        Ok(self.manager.tool_capabilities(&server, &input.name))
    }

    async fn execute(
        &self,
        invocation: &ToolContext,
        input: Value,
    ) -> Result<ToolResult, ToolError> {
        let input: McpCallInput = parse(input)?;
        validate_wire_name(&input.name, "MCP tool")?;
        let server = McpServerId::new(input.server).map_err(mcp_tool_error)?;
        if !invocation
            .mcp_tool_policy()
            .allows(server.as_str(), &input.name)
        {
            return Err(ToolError::InvalidInput(
                "MCP tool is not allowed for the active agent".to_owned(),
            ));
        }
        let response = self
            .manager
            .call_tool(&server, &input.name, input.arguments)
            .await
            .map_err(mcp_tool_error)?;
        Ok(capped_result(&server, &input.name, response))
    }
}

struct McpResourceTool {
    manager: Arc<McpManager>,
}

#[async_trait]
impl Tool for McpResourceTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        self.manager.settle_effects().await.map_err(|_| {
            ToolError::EffectsUnsettled("MCP invocation effects remain owned".to_owned())
        })
    }

    fn descriptor(&self) -> ToolDescriptor {
        restrictive_descriptor(
            "mcp_read_resource",
            "Read an MCP resource through the configured server.",
            schema::<McpResourceInput>(),
        )
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    async fn execute(&self, _context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let input: McpResourceInput = parse(input)?;
        if input.uri.len() > 4 * 1024 {
            return Err(ToolError::InvalidInput(
                "MCP resource URI exceeds 4096 bytes".to_owned(),
            ));
        }
        let server = McpServerId::new(input.server).map_err(mcp_tool_error)?;
        let response = self
            .manager
            .read_resource(&server, &input.uri)
            .await
            .map_err(mcp_tool_error)?;
        Ok(capped_result(&server, &input.uri, response))
    }
}

struct McpPromptTool {
    manager: Arc<McpManager>,
}

#[async_trait]
impl Tool for McpPromptTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        self.manager.settle_effects().await.map_err(|_| {
            ToolError::EffectsUnsettled("MCP invocation effects remain owned".to_owned())
        })
    }

    fn descriptor(&self) -> ToolDescriptor {
        restrictive_descriptor(
            "mcp_get_prompt",
            "Load one MCP server prompt as untrusted context.",
            schema::<McpPromptInput>(),
        )
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    async fn execute(&self, _context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let input: McpPromptInput = parse(input)?;
        validate_wire_name(&input.name, "MCP prompt")?;
        let server = McpServerId::new(input.server).map_err(mcp_tool_error)?;
        let response = self
            .manager
            .get_prompt(&server, &input.name, input.arguments)
            .await
            .map_err(mcp_tool_error)?;
        Ok(capped_result(&server, &input.name, response))
    }
}

struct McpOverflowReadTool {
    spool: Arc<dyn OverflowSpool>,
}

#[async_trait]
impl Tool for McpOverflowReadTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "mcp_overflow_read".to_owned(),
            description: "Read or grep a bounded window of an opaque MCP overflow artifact."
                .to_owned(),
            input_schema: schema::<McpOverflowInput>(),
            capabilities: CapabilityManifest::new([ToolCapability::ReadFilesystem]),
        }
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    async fn execute(&self, _context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let input: McpOverflowInput = parse(input)?;
        if input.id.len() > 256 || input.query.as_ref().is_some_and(|query| query.len() > 512) {
            return Err(ToolError::InvalidInput(
                "MCP overflow reference or query is oversized".to_owned(),
            ));
        }
        let reference = OverflowReference {
            id: input.id,
            bytes: input.bytes,
        };
        let bytes = self.spool.read(&reference).await.map_err(mcp_tool_error)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| ToolError::Output("MCP overflow payload is not UTF-8".to_owned()))?;
        let selected = if let Some(query) = input.query {
            text.lines()
                .filter(|line| line.contains(&query))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            text.get(input.offset..).unwrap_or_default().to_owned()
        };
        let selected = truncate_utf8(&selected, MAX_OVERFLOW_READ_BYTES);
        let data = json!({
            "artifact_id": reference.id,
            "original_bytes": reference.bytes,
            "offset": input.offset,
            "returned_bytes": selected.len(),
            "truncated": selected.len() < text.len().saturating_sub(input.offset),
        });
        Ok(untrusted_result(&selected, data))
    }
}

fn restrictive_descriptor(name: &str, description: &str, input_schema: Value) -> ToolDescriptor {
    ToolDescriptor {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema,
        capabilities: CapabilityManifest::new([ToolCapability::Network, ToolCapability::Execute]),
    }
}

fn capped_result(
    server: &McpServerId,
    operation: &str,
    response: rw_mcp::CappedResponse,
) -> ToolResult {
    let rw_mcp::CappedResponse {
        encoded,
        format,
        truncated,
        overflow,
    } = response;
    let data = json!({
        "server": server,
        "operation": operation,
        "format": format,
        "truncated": truncated,
        "overflow": overflow,
    });
    untrusted_result(&encoded, data)
}

fn untrusted_result(content: &str, data: Value) -> ToolResult {
    ToolResult::new(format!("{UNTRUSTED_OPEN}{content}{UNTRUSTED_CLOSE}"), data)
        .with_protected_framing(UNTRUSTED_OPEN, UNTRUSTED_CLOSE)
}

fn validate_wire_name(name: &str, kind: &str) -> Result<(), ToolError> {
    if name.is_empty()
        || name.len() > 256
        || name
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(ToolError::InvalidInput(format!(
            "{kind} name is empty, oversized, or contains whitespace"
        )));
    }
    Ok(())
}

fn schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ToolError> {
    serde_json::from_value(value).map_err(|error| ToolError::InvalidInput(error.to_string()))
}

fn mcp_tool_error(_error: impl std::fmt::Display) -> ToolError {
    // Remote protocol errors are untrusted and may contain prompt injections or
    // secrets echoed by a server. Detailed diagnostics remain in bounded host
    // status; the model-facing tool error stays constant.
    ToolError::Output("MCP operation failed; inspect /mcp status for details".to_owned())
}

fn truncate_utf8(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests;
