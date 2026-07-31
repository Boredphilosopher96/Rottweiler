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
    McpServerConfig, McpTransportConfig, OverflowReference, OverflowSpool, SecretToken, ServerId,
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
use rw_types::ToolCapability;
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

/// Trusted configuration for one public-client MCP OAuth authorization-code login.
#[derive(Clone, Debug)]
pub struct McpOAuthLoginConfig {
    pub server: ServerId,
    pub authorization_endpoint: Url,
    pub token_endpoint: Url,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub proxy: Option<Url>,
    pub credential_reference: CredentialReference,
    pub resource: String,
    pub audience: String,
    pub credentials_path: std::path::PathBuf,
}

/// In-progress browser login. Token, state, and PKCE verifier never cross this facade.
pub struct McpOAuthLogin {
    server: ServerId,
    session: OAuthLoginSession,
    authorization_url: String,
    redirect_uri: String,
    credential_reference: CredentialReference,
    resource: String,
    audience: String,
    token_endpoint: Url,
    client_id: String,
    scopes: Vec<String>,
    proxy: Option<Url>,
    credentials: CredentialManager,
}

impl fmt::Debug for McpOAuthLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpOAuthLogin")
            .field("server", &self.server)
            .field("authorization_url", &"[REDACTED]")
            .field("redirect_uri", &self.redirect_uri)
            .field(
                "credential_reference",
                &self.credential_reference.identifier(),
            )
            .field("resource", &self.resource)
            .field("audience", &self.audience)
            .finish_non_exhaustive()
    }
}

impl McpOAuthLogin {
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    #[must_use]
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Completes state validation and PKCE exchange, then atomically stores the
    /// bearer token together with its exact MCP resource and audience binding.
    ///
    /// # Errors
    ///
    /// Returns a sanitized MCP error when the callback, exchange, encoding, or
    /// credential-vault operation fails.
    pub async fn complete(self) -> Result<McpOAuthLoginResult, McpError> {
        let tokens = self.session.complete().await.map_err(|error| {
            McpError::Protocol(format!("MCP OAuth login did not complete: {error}"))
        })?;
        let encoded = encode_mcp_oauth_token_set(
            &tokens,
            self.resource,
            self.audience,
            &self.token_endpoint,
            &self.client_id,
            &self.scopes,
            self.proxy.as_ref(),
        )?;
        let stored = self
            .credentials
            .store(&self.credential_reference, &encoded)
            .map_err(|error| {
                McpError::Policy(format!("MCP OAuth credential could not be stored: {error}"))
            })?;
        Ok(McpOAuthLoginResult {
            server: self.server,
            warnings: stored.warnings().iter().map(ToString::to_string).collect(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpOAuthLoginResult {
    pub server: ServerId,
    pub warnings: Vec<String>,
}

/// Starts a standards-based Authorization Code + PKCE S256 flow for an MCP
/// public client. Ambient proxy discovery remains disabled.
///
/// # Errors
///
/// Returns a sanitized MCP error for invalid endpoints/bindings, unsupported
/// credential references, unavailable entropy, or loopback bind failure.
pub async fn begin_mcp_oauth_login(config: McpOAuthLoginConfig) -> Result<McpOAuthLogin, McpError> {
    let resource_url = Url::parse(&config.resource)
        .map_err(|_| McpError::Policy("MCP OAuth resource must be an absolute URL".to_owned()))?;
    let loopback_resource = resource_url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if config.credential_reference.environment_variable().is_some()
        || config.resource.is_empty()
        || config.resource.len() > 4096
        || config.audience.is_empty()
        || config.audience.len() > 4096
        || config
            .audience
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || resource_url.host().is_none()
        || !resource_url.username().is_empty()
        || resource_url.password().is_some()
        || resource_url.fragment().is_some()
        || (resource_url.scheme() != "https"
            && !(resource_url.scheme() == "http" && loopback_resource))
        || [&config.authorization_endpoint, &config.token_endpoint]
            .into_iter()
            .any(|endpoint| {
                endpoint.query_pairs().any(|(name, _)| {
                    name.eq_ignore_ascii_case("resource") || name.eq_ignore_ascii_case("audience")
                })
            })
        || config.proxy.as_ref().is_some_and(|proxy| {
            !matches!(proxy.scheme(), "http" | "https")
                || proxy.host().is_none()
                || !proxy.username().is_empty()
                || proxy.password().is_some()
                || proxy.query().is_some()
                || proxy.fragment().is_some()
        })
    {
        return Err(McpError::Policy(
            "MCP OAuth login configuration or resource/audience binding is invalid".to_owned(),
        ));
    }
    let binding_parameters = [
        ("resource".to_owned(), config.resource.clone()),
        ("audience".to_owned(), config.audience.clone()),
    ];
    let token_endpoint = config.token_endpoint.clone();
    let client_id = config.client_id.clone();
    let scopes = config.scopes.clone();
    let proxy = config.proxy.clone();
    let flow = OAuthAuthorizationCode::with_proxy(
        OAuthAuthorizationCodeConfig {
            authorization_endpoint: config.authorization_endpoint,
            token_endpoint: config.token_endpoint,
            client_id: config.client_id,
            scopes: config.scopes,
            callback_timeout: DEFAULT_OAUTH_CALLBACK_TIMEOUT,
        },
        config.proxy.as_ref(),
        None,
    )
    .map_err(|error| McpError::Policy(format!("MCP OAuth configuration is invalid: {error}")))?
    .with_authorization_parameters(binding_parameters.clone())
    .with_token_parameters(binding_parameters);
    let session = flow
        .begin()
        .await
        .map_err(|error| McpError::Protocol(format!("MCP OAuth login could not begin: {error}")))?;
    let authorization_url = session.authorization_url().to_string();
    let redirect_uri = session.redirect_uri().to_string();
    Ok(McpOAuthLogin {
        server: config.server,
        session,
        authorization_url,
        redirect_uri,
        credential_reference: config.credential_reference,
        resource: config.resource,
        audience: config.audience,
        token_endpoint,
        client_id,
        scopes,
        proxy,
        credentials: CredentialManager::system(config.credentials_path),
    })
}

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
    bindings: BTreeMap<ServerId, McpOAuthBinding>,
    refreshers: tokio::sync::Mutex<BTreeMap<ServerId, Arc<RefreshingOAuth>>>,
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
        bindings: BTreeMap<ServerId, McpOAuthBinding>,
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
                "legacy MCP OAuth credential cannot rotate",
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
        server: &ServerId,
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
        server: &ServerId,
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
        server: &ServerId,
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
            .map(ServerId::new)
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
                    .allows(&definition.server.0, &definition.name)
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
        let server = ServerId::new(input.server).map_err(mcp_tool_error)?;
        Ok(self.manager.tool_capabilities(&server, &input.name))
    }

    async fn execute(
        &self,
        invocation: &ToolContext,
        input: Value,
    ) -> Result<ToolResult, ToolError> {
        let input: McpCallInput = parse(input)?;
        validate_wire_name(&input.name, "MCP tool")?;
        let server = ServerId::new(input.server).map_err(mcp_tool_error)?;
        if !invocation.mcp_tool_policy().allows(&server.0, &input.name) {
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
        let server = ServerId::new(input.server).map_err(mcp_tool_error)?;
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
        let server = ServerId::new(input.server).map_err(mcp_tool_error)?;
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
    server: &ServerId,
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
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    };

    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };
    use rw_mcp::{
        BridgeError, EngineMcpBridge, EngineTool, McpServerAuthority, RottweilerMcpServer,
        SessionSummary,
    };
    use rw_store::credentials::{
        CredentialError, CredentialStore, CredentialStoreUnavailable, Secret as StoredSecret,
    };

    const HTTP_BEARER_CANARY: &str = "mcp-http-bearer-canary-never-log";

    #[derive(Clone, Copy)]
    struct EmptyCredentialEnvironment;

    impl CredentialEnvironment for EmptyCredentialEnvironment {
        fn get(&self, _name: &str) -> Result<Option<String>, CredentialError> {
            Ok(None)
        }
    }

    #[derive(Clone, Default)]
    struct MemoryCredentialStore(Arc<StdMutex<BTreeMap<String, String>>>);

    impl CredentialStore for MemoryCredentialStore {
        fn get(
            &self,
            identifier: &str,
        ) -> Result<Option<StoredSecret<String>>, CredentialStoreUnavailable> {
            Ok(self
                .0
                .lock()
                .map_err(|_| CredentialStoreUnavailable)?
                .get(identifier)
                .cloned()
                .map(StoredSecret::new))
        }

        fn set(
            &self,
            identifier: &str,
            secret: &StoredSecret<String>,
        ) -> Result<(), CredentialStoreUnavailable> {
            self.0
                .lock()
                .map_err(|_| CredentialStoreUnavailable)?
                .insert(identifier.to_owned(), secret.expose_secret().clone());
            Ok(())
        }
    }

    struct PolicyClient {
        server: String,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl rw_mcp::McpClient for PolicyClient {
        async fn list_tools(&self) -> Result<Vec<Value>, McpError> {
            let names = if self.server == "github" {
                vec!["get_issue", "delete_issue"]
            } else {
                vec!["search_messages"]
            };
            Ok(names
                .into_iter()
                .map(|name| {
                    json!({
                        "name": name,
                        "description": format!("fixture {name}"),
                        "inputSchema": {"type": "object"}
                    })
                })
                .collect())
        }

        async fn list_resources(&self) -> Result<Vec<Value>, McpError> {
            Ok(Vec::new())
        }

        async fn list_prompts(&self) -> Result<Vec<Value>, McpError> {
            Ok(Vec::new())
        }

        async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"server": self.server, "name": name, "arguments": arguments}))
        }

        async fn read_resource(&self, _uri: &str) -> Result<Value, McpError> {
            unreachable!("policy fixture has no resources")
        }

        async fn get_prompt(&self, _name: &str, _arguments: Value) -> Result<Value, McpError> {
            unreachable!("policy fixture has no prompts")
        }

        async fn close(&self, _timeout: Duration) -> Result<(), McpError> {
            Ok(())
        }
    }

    struct PolicyConnector {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl McpConnector for PolicyConnector {
        async fn connect(
            &self,
            config: &McpServerConfig,
        ) -> Result<Arc<dyn rw_mcp::McpClient>, McpError> {
            Ok(Arc::new(PolicyClient {
                server: config.id.0.clone(),
                calls: Arc::clone(&self.calls),
            }))
        }
    }

    #[derive(Default)]
    struct PolicySpool;

    #[async_trait]
    impl OverflowSpool for PolicySpool {
        async fn write(
            &self,
            _server: &ServerId,
            _operation: &str,
            _bytes: &[u8],
        ) -> Result<OverflowReference, McpError> {
            unreachable!("policy fixture responses remain below the overflow limit")
        }

        async fn read(&self, _reference: &OverflowReference) -> Result<Vec<u8>, McpError> {
            unreachable!("policy fixture never creates overflow references")
        }

        async fn remove(&self, _reference: &OverflowReference) -> Result<(), McpError> {
            Ok(())
        }
    }

    struct EchoBridge;

    #[async_trait]
    impl EngineMcpBridge for EchoBridge {
        async fn tools(&self) -> Result<Vec<EngineTool>, BridgeError> {
            Ok(vec![EngineTool {
                name: "echo".to_owned(),
                description: "Echo one bounded test message".to_owned(),
                input_schema: json!({"type":"object"}),
            }])
        }

        async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, BridgeError> {
            if name != "echo" {
                return Err(BridgeError::safe("unknown test tool"));
            }
            Ok(arguments)
        }

        async fn create_session(
            &self,
            _title: Option<String>,
        ) -> Result<SessionSummary, BridgeError> {
            Err(BridgeError::safe("not used by test"))
        }

        async fn list_sessions(&self) -> Result<Vec<SessionSummary>, BridgeError> {
            Ok(Vec::new())
        }

        async fn send_message(
            &self,
            _session_id: &str,
            _message: &str,
        ) -> Result<Value, BridgeError> {
            Err(BridgeError::safe("not used by test"))
        }
    }

    struct AllowConnection;

    #[async_trait]
    impl McpConnectionApprovalPolicy for AllowConnection {
        async fn approve(&self, _config: &McpServerConfig) -> Result<(), McpError> {
            Ok(())
        }
    }

    struct CanaryAuthorization;

    #[async_trait]
    impl McpAuthorizationProvider for CanaryAuthorization {
        async fn token(
            &self,
            _server: &ServerId,
            _resource: &str,
        ) -> Result<Option<SecretToken>, McpError> {
            Ok(Some(SecretToken::new(HTTP_BEARER_CANARY)))
        }
    }

    fn oauth_login_config(token_endpoint: Url) -> McpOAuthLoginConfig {
        McpOAuthLoginConfig {
            server: ServerId("oauth-fixture".to_owned()),
            authorization_endpoint: Url::parse("https://auth.example/authorize")
                .expect("authorization URL"),
            token_endpoint,
            client_id: "public-client".to_owned(),
            scopes: vec!["mcp:tools".to_owned()],
            proxy: None,
            credential_reference: CredentialReference::new("mcp.oauth-fixture.oauth"),
            resource: "https://mcp.example/mcp".to_owned(),
            audience: "mcp.example".to_owned(),
            credentials_path: std::env::temp_dir().join("unused-oauth-credentials.toml"),
        }
    }

    #[test]
    fn toon_encoder_is_structured_and_deterministic() {
        let encoded = ToonMcpEncoder
            .encode(&json!({"items":[{"name":"alpha"}]}))
            .expect("TOON");
        let encoded = String::from_utf8(encoded).expect("UTF-8");
        assert!(encoded.contains("items[1]"));
        assert_eq!(ToonMcpEncoder.format(), "toon");
    }

    #[test]
    fn protected_mcp_framing_and_utf8_truncation_are_stable() {
        let result = untrusted_result("remote instructions", json!({}));
        assert!(result.content.starts_with(UNTRUSTED_OPEN));
        assert!(result.content.ends_with(UNTRUSTED_CLOSE));
        assert_eq!(truncate_utf8("🐕🐕", 5), "🐕");
    }

    #[tokio::test]
    async fn expired_mcp_oauth_refreshes_once_and_persists_rotation() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        const INITIAL_REFRESH: &str = "mcp-initial-refresh-canary";
        const ROTATED_REFRESH: &str = "mcp-rotated-refresh-canary";
        const REFRESHED_ACCESS: &str = "mcp-refreshed-access-canary";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("token listener");
        let token_endpoint = Url::parse(&format!(
            "http://{}/token",
            listener.local_addr().expect("token address")
        ))
        .expect("token endpoint");
        let reference = CredentialReference::new("mcp.oauth-refresh.oauth");
        let credential_store = MemoryCredentialStore::default();
        let manager = Arc::new(CredentialManager::with_backends(
            EmptyCredentialEnvironment,
            credential_store,
            std::env::temp_dir().join("unused-mcp-refresh-fixture.toml"),
        ));
        let stored = StoredMcpOAuthCredential {
            version: 2,
            access_token: "expired-access-canary".to_owned(),
            refresh_token: Some(INITIAL_REFRESH.to_owned()),
            expires_at_unix_seconds: Some(0),
            resource: "https://mcp.example/mcp".to_owned(),
            audience: "mcp.example".to_owned(),
            token_endpoint: Some(token_endpoint.as_str().to_owned()),
            client_id: Some("public-client".to_owned()),
            scopes: vec!["mcp:tools".to_owned()],
            proxy: None,
        };
        manager
            .store(
                &reference,
                &StoredSecret::new(serde_json::to_string(&stored).expect("stored JSON")),
            )
            .expect("seed credential");
        let server = ServerId("oauth-refresh-fixture".to_owned());
        let provider = VaultMcpTokenProvider::new(
            manager.clone(),
            BTreeMap::from([(
                server.clone(),
                McpOAuthBinding {
                    token_reference: reference.clone(),
                    resource: stored.resource.clone(),
                    audience: stored.audience.clone(),
                    refresh: Some(McpOAuthRefreshBinding {
                        token_endpoint: token_endpoint.clone(),
                        client_id: "public-client".to_owned(),
                        scopes: vec!["mcp:tools".to_owned()],
                        proxy: None,
                    }),
                },
            )]),
        );
        let responder = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("refresh request");
            let mut request = vec![0_u8; 16 * 1024];
            let count = stream.read(&mut request).await.expect("read refresh");
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.contains("grant_type=refresh_token"));
            assert!(request.contains(INITIAL_REFRESH));
            let body = format!(
                r#"{{"access_token":"{REFRESHED_ACCESS}","refresh_token":"{ROTATED_REFRESH}","expires_in":3600,"token_type":"Bearer"}}"#
            );
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("write refresh");
        });
        let first = provider
            .token(&server, "https://mcp.example/mcp")
            .await
            .expect("refresh token")
            .expect("bearer");
        assert_eq!(first.expose(), REFRESHED_ACCESS);
        responder.await.expect("refresh responder");
        let second = provider
            .token(&server, "https://mcp.example/mcp")
            .await
            .expect("cached token")
            .expect("cached bearer");
        assert_eq!(second.expose(), REFRESHED_ACCESS);
        let resolved = manager.resolve(&reference).expect("rotated credential");
        let rotated: StoredMcpOAuthCredential =
            serde_json::from_str(resolved.secret().expose_secret()).expect("rotated JSON");
        assert_eq!(rotated.refresh_token.as_deref(), Some(ROTATED_REFRESH));
        let debug = format!("{provider:?}");
        assert!(!debug.contains(INITIAL_REFRESH));
        assert!(!debug.contains(ROTATED_REFRESH));
        assert!(!debug.contains(REFRESHED_ACCESS));
    }

    #[test]
    fn mcp_http_headers_reject_oversized_and_control_ids() {
        assert!(mcp_http_headers(None, Some("bad id"), None, HashMap::new(), false).is_err());
        assert!(
            mcp_http_headers(
                None,
                Some("ok-session"),
                Some("x".repeat(MCP_HTTP_MAX_EVENT_ID_BYTES + 1)),
                HashMap::new(),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn loopback_authority_is_scoped_to_one_origin() {
        let endpoint = Url::parse("http://127.0.0.1:8123/mcp").expect("URL");
        let authority = LoopbackMcpAuthority::for_endpoint(&endpoint).expect("authority");
        assert_eq!(authority.origin, "http://127.0.0.1:8123");
        assert!(
            LoopbackMcpAuthority::for_endpoint(
                &Url::parse("https://example.com/mcp").expect("URL")
            )
            .is_err()
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn agent_mcp_policy_hides_schemas_and_denies_direct_calls_without_narrowing_main() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let manager = Arc::new(McpManager::new(
            Arc::new(PolicyConnector {
                calls: Arc::clone(&calls),
            }),
            Arc::new(PolicySpool),
            Arc::new(rw_mcp::CompactJsonEncoder),
            rw_mcp::McpLimits {
                response_bytes: 64 * 1024,
                request_timeout: Duration::from_secs(1),
                shutdown_timeout: Duration::from_secs(1),
            },
        ));
        for server in ["github", "slack"] {
            let tool_capabilities = if server == "github" {
                rw_mcp::McpToolCapabilityOverrides {
                    server_default: Some(CapabilityManifest::new([ToolCapability::ReadFilesystem])),
                    tools: BTreeMap::from([(
                        "delete_issue".to_owned(),
                        CapabilityManifest::default(),
                    )]),
                }
            } else {
                rw_mcp::McpToolCapabilityOverrides::default()
            };
            manager
                .register(McpServerConfig {
                    id: ServerId::new(server).expect("server id"),
                    transport: McpTransportConfig::Stdio {
                        executable: "fixture".into(),
                        args: Vec::new(),
                        working_directory: None,
                        environment: Vec::new(),
                        sandbox: rw_mcp::McpStdioSandboxPolicy::default(),
                    },
                    enabled: true,
                    defer_tools: true,
                    tool_capabilities,
                })
                .await
                .expect("register");
        }
        assert!(
            manager
                .connect_all()
                .await
                .into_iter()
                .all(|(_, result)| result.is_ok())
        );

        let workspace = tempfile::tempdir().expect("workspace");
        let restricted = ToolContext::new(workspace.path())
            .expect("context")
            .with_mcp_tool_policy(
                rw_tools::McpToolPolicy::restricted(["mcp:github/get_issue".to_owned()])
                    .expect("policy"),
            );
        let search = ToolSearchTool {
            manager: Arc::clone(&manager),
        };
        let result = search
            .execute(&restricted, json!({"query": ""}))
            .await
            .expect("restricted search");
        let encoded = result.data.to_string();
        assert!(encoded.contains("get_issue"));
        assert!(!encoded.contains("delete_issue"));
        assert!(!encoded.contains("search_messages"));
        assert!(!result.content.contains("delete_issue"));
        assert!(!result.content.contains("search_messages"));

        let call = McpCallTool {
            manager: Arc::clone(&manager),
        };
        assert_eq!(
            call.invocation_capabilities(
                &json!({"server":"github", "name":"get_issue", "arguments":{}})
            )
            .expect("server classification"),
            CapabilityManifest::new([ToolCapability::ReadFilesystem])
        );
        assert_eq!(
            call.invocation_capabilities(
                &json!({"server":"github", "name":"delete_issue", "arguments":{}})
            )
            .expect("tool classification"),
            CapabilityManifest::default()
        );
        assert_eq!(
            call.invocation_capabilities(
                &json!({"server":"slack", "name":"search_messages", "arguments":{}})
            )
            .expect("restrictive default"),
            CapabilityManifest::new([ToolCapability::Network, ToolCapability::Execute])
        );
        call.execute(
            &restricted,
            json!({"server":"github", "name":"get_issue", "arguments":{}}),
        )
        .await
        .expect("permitted call");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let denied = call
            .execute(
                &restricted,
                json!({"server":"github", "name":"delete_issue", "arguments":{}}),
            )
            .await
            .expect_err("direct ungranted call must fail before the manager");
        assert!(
            denied
                .to_string()
                .contains("not allowed for the active agent")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let main = ToolContext::new(workspace.path()).expect("main context");
        let result = search
            .execute(&main, json!({"query": ""}))
            .await
            .expect("main search remains unrestricted");
        let encoded = result.data.to_string();
        assert!(encoded.contains("get_issue"));
        assert!(encoded.contains("delete_issue"));
        assert!(encoded.contains("search_messages"));
        call.execute(
            &main,
            json!({"server":"github", "name":"delete_issue", "arguments":{}}),
        )
        .await
        .expect("main approved MCP config remains callable");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn mcp_oauth_sends_resource_and_audience_at_both_protocol_boundaries() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let token_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("token listener");
        let token_endpoint = Url::parse(&format!(
            "http://{}/token",
            token_listener.local_addr().expect("token address")
        ))
        .expect("token URL");
        let login = begin_mcp_oauth_login(oauth_login_config(token_endpoint))
            .await
            .expect("login begins");
        let authorization_url = Url::parse(login.authorization_url()).expect("authorization URL");
        let query = authorization_url.query_pairs().collect::<BTreeMap<_, _>>();
        assert_eq!(
            query.get("resource").map(AsRef::as_ref),
            Some("https://mcp.example/mcp")
        );
        assert_eq!(
            query.get("audience").map(AsRef::as_ref),
            Some("mcp.example")
        );
        assert_eq!(
            query.get("code_challenge_method").map(AsRef::as_ref),
            Some("S256")
        );
        let state = query.get("state").expect("state").to_string();
        let redirect = Url::parse(login.redirect_uri()).expect("redirect URL");
        let debug = format!("{login:?}");
        assert!(!debug.contains(&state));

        let completion = tokio::spawn(login.complete());
        let mut callback = tokio::net::TcpStream::connect((
            redirect.host_str().expect("redirect host"),
            redirect.port().expect("redirect port"),
        ))
        .await
        .expect("callback connection");
        callback
            .write_all(
                format!(
                    "GET {}?code=fixture-code&state={state} HTTP/1.1\r\nHost: {}:{}\r\n\r\n",
                    redirect.path(),
                    redirect.host_str().expect("redirect host"),
                    redirect.port().expect("redirect port")
                )
                .as_bytes(),
            )
            .await
            .expect("callback write");
        let mut callback_response = Vec::new();
        callback
            .read_to_end(&mut callback_response)
            .await
            .expect("callback response");

        let (mut token_stream, _) = token_listener.accept().await.expect("token request");
        let mut request = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let mut chunk = [0_u8; 4096];
                let count = token_stream.read(&mut chunk).await.expect("token read");
                assert!(count > 0, "token request ended before its body");
                request.extend_from_slice(&chunk[..count]);
                assert!(
                    request.len() <= 16 * 1024,
                    "token request exceeded test cap"
                );
                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .expect("content length");
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
        })
        .await
        .expect("bounded token request");
        let request = String::from_utf8_lossy(&request);
        assert!(request.contains("resource=https%3A%2F%2Fmcp.example%2Fmcp"));
        assert!(request.contains("audience=mcp.example"));
        token_stream
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("token rejection");
        drop(token_stream);
        let error = completion
            .await
            .expect("completion joins")
            .expect_err("rejected token exchange fails");
        let diagnostic = error.to_string();
        assert!(!diagnostic.contains("fixture-code"));
        assert!(!diagnostic.contains(&state));
    }

    #[tokio::test]
    async fn dropping_mcp_oauth_login_releases_the_loopback_listener() {
        let login = begin_mcp_oauth_login(oauth_login_config(
            Url::parse("http://127.0.0.1:1/token").expect("token URL"),
        ))
        .await
        .expect("login begins");
        let redirect = Url::parse(login.redirect_uri()).expect("redirect URL");
        let address = (
            redirect.host_str().expect("redirect host"),
            redirect.port().expect("redirect port"),
        );
        drop(login);
        // A connect-after-drop assertion is racy under the parallel test suite:
        // the OS may immediately recycle this ephemeral port for another OAuth
        // fixture. Rebinding the exact address proves that this session released
        // its listener without sending traffic to an unrelated recycled port.
        let rebound = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match tokio::net::TcpListener::bind(address).await {
                    Ok(listener) => break listener,
                    Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                        tokio::task::yield_now().await;
                    }
                    Err(error) => panic!("loopback address could not rebind: {error}"),
                }
            }
        })
        .await
        .expect("dropped login must release its callback address");
        drop(rebound);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn production_connector_drives_real_rmcp_http_with_bearer_canary() {
        use hyper::service::service_fn;
        use hyper_util::rt::TokioIo;
        use tower_service::Service as _;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("HTTP listener");
        let address = listener.local_addr().expect("HTTP address");
        let endpoint = Url::parse(&format!("http://{address}/mcp")).expect("MCP URL");
        let bearer_seen = Arc::new(AtomicBool::new(false));
        let bearer_rejected = Arc::new(AtomicBool::new(false));
        let service: StreamableHttpService<RottweilerMcpServer, LocalSessionManager> =
            StreamableHttpService::new(
                || {
                    Ok(RottweilerMcpServer::new(
                        Arc::new(EchoBridge),
                        McpServerAuthority::new(["echo".to_owned()], []),
                    ))
                },
                Arc::default(),
                StreamableHttpServerConfig::default().with_sse_keep_alive(None),
            );
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn({
            let bearer_seen = bearer_seen.clone();
            let bearer_rejected = bearer_rejected.clone();
            async move {
                loop {
                    let (stream, _) = tokio::select! {
                        accepted = listener.accept() => accepted.expect("HTTP accept"),
                        changed = shutdown_rx.changed() => {
                            let _ = changed;
                            break;
                        }
                    };
                    let mcp = service.clone();
                    let bearer_seen = bearer_seen.clone();
                    let bearer_rejected = bearer_rejected.clone();
                    tokio::spawn(async move {
                        let guarded =
                            service_fn(move |request: http::Request<hyper::body::Incoming>| {
                                let mut mcp = mcp.clone();
                                let bearer_seen = bearer_seen.clone();
                                let bearer_rejected = bearer_rejected.clone();
                                async move {
                                    let authorization = request
                                        .headers()
                                        .get(http::header::AUTHORIZATION)
                                        .and_then(|value| value.to_str().ok());
                                    if authorization.is_some_and(|value| {
                                        value == format!("Bearer {HTTP_BEARER_CANARY}")
                                    }) {
                                        bearer_seen.store(true, Ordering::SeqCst);
                                    } else {
                                        bearer_rejected.store(true, Ordering::SeqCst);
                                    }
                                    mcp.call(request).await
                                }
                            });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), guarded)
                            .await;
                    });
                }
            }
        });

        let http_client = ProductionMcpHttpClient::new().with_loopback_authority(
            LoopbackMcpAuthority::for_endpoint(&endpoint).expect("loopback authority"),
        );
        let connector = ProductionMcpHttpConnector::new(
            http_client,
            Arc::new(CanaryAuthorization),
            Arc::new(AllowConnection),
        );
        let config = McpServerConfig {
            id: ServerId("http-canary".to_owned()),
            transport: McpTransportConfig::StreamableHttp {
                endpoint: endpoint.to_string(),
                oauth: true,
            },
            enabled: true,
            defer_tools: true,
            tool_capabilities: rw_mcp::McpToolCapabilityOverrides::default(),
        };
        let client = connector.connect(&config).await.expect("MCP initialize");
        let catalog = client.list_tools().await.expect("MCP tool catalog");
        assert!(
            catalog
                .iter()
                .any(|tool| tool.get("name") == Some(&json!("rottweiler_tools_call")))
        );
        let result = client
            .call_tool(
                "rottweiler_tools_call",
                json!({"name":"echo","arguments":{"message":"hello over guarded HTTP"}}),
            )
            .await
            .expect("MCP tool call");
        assert!(result.to_string().contains("hello over guarded HTTP"));
        client
            .close(Duration::from_secs(2))
            .await
            .expect("MCP shutdown");
        assert!(bearer_seen.load(Ordering::SeqCst));
        assert!(!bearer_rejected.load(Ordering::SeqCst));
        let diagnostics = format!("{config:?} {result:?}");
        assert!(!diagnostics.contains(HTTP_BEARER_CANARY));

        let _ = shutdown_tx.send(true);
        server.await.expect("HTTP server joins");
    }
}
