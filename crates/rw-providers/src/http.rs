use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use futures_util::StreamExt as _;
use reqwest::{Response, StatusCode, header::RETRY_AFTER};
use thiserror::Error;
use url::Url;

use crate::{
    NetworkPolicy, ProviderError, ProviderErrorKind, ProxyAuthentication,
    types::MAX_PROVIDER_ERROR_BYTES,
};

static PROCESS_NETWORK_DENY_DEPTH: AtomicUsize = AtomicUsize::new(0);
const MAX_GUARDED_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_GUARDED_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_GUARDED_HEADERS: usize = 128;
const MAX_GUARDED_HEADER_BYTES: usize = 64 * 1024;
const MAX_GUARDED_HEADER_VALUE_BYTES: usize = 8 * 1024;

pub(crate) async fn bounded_error_json(response: Response) -> Option<serde_json::Value> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_ERROR_BYTES as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if bytes.len().saturating_add(chunk.len()) > MAX_PROVIDER_ERROR_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).ok()
}

/// Methods supported by the guarded provider-neutral HTTP boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardedHttpMethod {
    Get,
    Post,
    Delete,
}

/// One guarded request, including streaming limits used by MCP and OAuth.
#[derive(Clone)]
pub struct GuardedHttpRequest {
    pub method: GuardedHttpMethod,
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub proxy: Option<Url>,
    pub proxy_authentication: Option<ProxyAuthentication>,
    pub dns_pin: Option<(String, SocketAddr)>,
    pub allow_private_destinations: bool,
    pub response_deadline: Duration,
    pub frame_deadline: Duration,
    pub max_frame_bytes: usize,
    pub max_body_bytes: usize,
}

impl std::fmt::Debug for GuardedHttpRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardedHttpRequest")
            .field("method", &self.method)
            .field("url", &redacted_url(&self.url))
            .field(
                "header_names",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("body_bytes", &self.body.len())
            .field("proxy", &self.proxy.as_ref().map(redacted_url))
            .field("proxy_authentication", &self.proxy_authentication)
            .field("dns_pin", &self.dns_pin)
            .field(
                "allow_private_destinations",
                &self.allow_private_destinations,
            )
            .field("response_deadline", &self.response_deadline)
            .field("frame_deadline", &self.frame_deadline)
            .field("max_frame_bytes", &self.max_frame_bytes)
            .field("max_body_bytes", &self.max_body_bytes)
            .finish()
    }
}

pub type GuardedHttpByteStream =
    Pin<Box<dyn futures_util::Stream<Item = Result<Vec<u8>, GuardedHttpFetchError>> + Send>>;

/// Headers and a bounded, deadline-aware response stream.
pub struct GuardedHttpStreamResponse {
    pub status: u16,
    pub final_url: Url,
    pub headers: Vec<(String, String)>,
    pub body: GuardedHttpByteStream,
}

/// One already policy-validated HTTP GET passed to the guarded transport.
#[derive(Clone, Debug)]
pub struct GuardedHttpFetchRequest {
    /// Validated HTTP(S) target without user information.
    pub url: Url,
    /// Validated outbound header names and values.
    pub headers: Vec<(String, String)>,
    /// Explicitly resolved proxy; ambient proxy discovery is always disabled.
    pub proxy: Option<Url>,
    /// Optional Basic authentication for the explicit proxy. Debug output is
    /// redacted by [`ProxyAuthentication`].
    pub proxy_authentication: Option<ProxyAuthentication>,
    /// Validated DNS host/address pin for the target, when it used a hostname.
    pub dns_pin: Option<(String, SocketAddr)>,
    /// Maximum accepted response bytes.
    pub max_bytes: usize,
    /// Explicit connect and whole-response deadline.
    pub timeout: Duration,
}

/// Provider-neutral result of one guarded HTTP GET without redirect following.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardedHttpFetchResponse {
    /// Numeric HTTP status.
    pub status: u16,
    /// Effective URL reported by the HTTP client.
    pub final_url: Url,
    /// Parsed UTF-8 content type, when present.
    pub content_type: Option<String>,
    /// Raw response body bounded by the request limit.
    pub body: Vec<u8>,
    /// UTF-8 redirect target, when the response supplied one.
    pub location: Option<String>,
}

/// Minimal no-body provider endpoint probe used by `rw doctor`.
#[derive(Clone)]
pub struct ProviderReachabilityRequest {
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub proxy: Option<Url>,
    pub proxy_authentication: Option<ProxyAuthentication>,
    pub timeout: Duration,
}

impl std::fmt::Debug for ProviderReachabilityRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderReachabilityRequest")
            .field("url", &redacted_url(&self.url))
            .field(
                "header_names",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("proxy", &self.proxy.as_ref().map(redacted_url))
            .field("proxy_authentication", &self.proxy_authentication)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Sends one timeout-bounded, redirect-free HEAD request through the shared
/// provider proxy/authentication boundary and returns only its status code.
///
/// # Errors
///
/// Returns a sanitized invalid-request, network-disabled, timeout, or transport error.
pub async fn provider_reachability_probe(
    request: ProviderReachabilityRequest,
) -> Result<u16, ProviderError> {
    require_process_network()?;
    if request.timeout.is_zero()
        || !matches!(request.url.scheme(), "http" | "https")
        || request.url.host().is_none()
        || !request.url.username().is_empty()
        || request.url.password().is_some()
        || request.url.query().is_some()
        || request.url.fragment().is_some()
        || request.headers.len() > MAX_GUARDED_HEADERS
        || request
            .headers
            .iter()
            .any(|(_, value)| value.len() > MAX_GUARDED_HEADER_VALUE_BYTES)
    {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "provider reachability probe is invalid",
        ));
    }
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(request.timeout)
        .timeout(request.timeout);
    if request.proxy.is_none() && request.proxy_authentication.is_some() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "proxy authentication requires a configured proxy URL",
        ));
    }
    if let Some(proxy) = request.proxy.as_ref() {
        let mut configured = reqwest::Proxy::all(proxy.as_str()).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "configured provider proxy URL is invalid",
            )
        })?;
        if let Some(authentication) = request.proxy_authentication.as_ref() {
            configured =
                configured.basic_auth(authentication.username(), authentication.password());
        }
        builder = builder.proxy(configured);
    }
    let client = builder.build().map_err(transport_error)?;
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in request.headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "provider reachability header name is invalid",
            )
        })?;
        let value = reqwest::header::HeaderValue::from_str(&value).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "provider reachability header value is invalid",
            )
        })?;
        headers.append(name, value);
    }
    client
        .head(request.url)
        .headers(headers)
        .send()
        .await
        .map(|response| response.status().as_u16())
        .map_err(transport_error)
}

/// Guarded HTTP fetch failure without exposing transport implementation types.
#[derive(Debug, Error)]
pub enum GuardedHttpFetchError {
    /// Provider transport, guard, or request-construction failure.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// Response exceeded the caller's explicit bound.
    #[error("HTTP response exceeded the {limit}-byte limit")]
    SizeLimit {
        /// Configured response bound.
        limit: usize,
    },
    #[error("HTTP response frame exceeded the {limit}-byte limit")]
    FrameLimit { limit: usize },
    #[error("HTTP response deadline expired")]
    Deadline,
}

/// Execute a method/body-capable no-redirect request and return a bounded stream.
///
/// # Errors
///
/// Returns a sanitized guard, request, transport, deadline, frame-limit, or
/// total-body-limit error. Redirect responses are returned and never followed.
#[allow(clippy::too_many_lines)]
pub async fn guarded_http_request(
    request: GuardedHttpRequest,
) -> Result<GuardedHttpStreamResponse, GuardedHttpFetchError> {
    require_process_network()?;
    validate_guarded_destination(&request)?;
    if request.proxy.is_some() && request.dns_pin.is_some() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "target DNS pin cannot be enforced through a forward proxy",
        )
        .into());
    }
    if request.proxy.is_none() && request.proxy_authentication.is_some() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "proxy authentication requires an explicit proxy",
        )
        .into());
    }
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(request.response_deadline);
    if let Some((host, address)) = &request.dns_pin {
        builder = builder.resolve(host, *address);
    }
    if let Some(proxy_url) = &request.proxy {
        let mut proxy = reqwest::Proxy::all(proxy_url.as_str()).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "configured HTTP proxy URL is invalid",
            )
        })?;
        if let Some(authentication) = &request.proxy_authentication {
            proxy = proxy.basic_auth(authentication.username(), authentication.password());
        }
        builder = builder.proxy(proxy);
    }
    let client = builder.build().map_err(transport_error)?;
    let method = match request.method {
        GuardedHttpMethod::Get => reqwest::Method::GET,
        GuardedHttpMethod::Post => reqwest::Method::POST,
        GuardedHttpMethod::Delete => reqwest::Method::DELETE,
    };
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in request.headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "guarded HTTP header name is invalid",
            )
        })?;
        let value = reqwest::header::HeaderValue::from_str(&value).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "guarded HTTP header value is invalid",
            )
        })?;
        headers.append(name, value);
    }
    let response = client
        .request(method, request.url)
        .headers(headers)
        .body(request.body)
        .send()
        .await
        .map_err(transport_error)?;
    if response
        .content_length()
        .is_some_and(|length| length > request.max_body_bytes as u64)
    {
        return Err(GuardedHttpFetchError::SizeLimit {
            limit: request.max_body_bytes,
        });
    }
    let status = response.status().as_u16();
    let final_url = response.url().clone();
    let inbound_headers = response.headers();
    if inbound_headers.len() > MAX_GUARDED_HEADERS
        || inbound_headers
            .iter()
            .any(|(_, value)| value.as_bytes().len() > MAX_GUARDED_HEADER_VALUE_BYTES)
        || inbound_headers
            .iter()
            .fold(0_usize, |total, (name, value)| {
                total
                    .saturating_add(name.as_str().len())
                    .saturating_add(value.as_bytes().len())
            })
            > MAX_GUARDED_HEADER_BYTES
    {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "guarded HTTP response headers exceeded their size cap",
        )
        .into());
    }
    let response_headers = inbound_headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.to_string(), value.to_owned()))
        })
        .collect();
    let frame_deadline = request.frame_deadline;
    let max_frame = request.max_frame_bytes;
    let max_body = request.max_body_bytes;
    let stream = futures_util::stream::unfold(
        (response.bytes_stream(), 0_usize, false),
        move |(mut stream, total, done)| async move {
            if done {
                return None;
            }
            let Ok(next) = tokio::time::timeout(frame_deadline, stream.next()).await else {
                return Some((Err(GuardedHttpFetchError::Deadline), (stream, total, true)));
            };
            match next {
                None => None,
                Some(Err(error)) => Some((
                    Err(GuardedHttpFetchError::Provider(transport_error(error))),
                    (stream, total, true),
                )),
                Some(Ok(bytes)) if bytes.len() > max_frame => Some((
                    Err(GuardedHttpFetchError::FrameLimit { limit: max_frame }),
                    (stream, total, true),
                )),
                Some(Ok(bytes)) if total.saturating_add(bytes.len()) > max_body => Some((
                    Err(GuardedHttpFetchError::SizeLimit { limit: max_body }),
                    (stream, total, true),
                )),
                Some(Ok(bytes)) => {
                    let total = total + bytes.len();
                    Some((Ok(bytes.to_vec()), (stream, total, false)))
                }
            }
        },
    );
    Ok(GuardedHttpStreamResponse {
        status,
        final_url,
        headers: response_headers,
        body: Box::pin(stream),
    })
}

fn validate_guarded_destination(request: &GuardedHttpRequest) -> Result<(), GuardedHttpFetchError> {
    if !matches!(request.url.scheme(), "http" | "https")
        || request.url.username() != ""
        || request.url.password().is_some()
        || request.url.query().is_some()
        || request.url.fragment().is_some()
        || request.body.len() > MAX_GUARDED_REQUEST_BYTES
        || request.max_frame_bytes == 0
        || request.max_body_bytes == 0
        || request.max_frame_bytes > request.max_body_bytes
        || request.max_body_bytes > MAX_GUARDED_RESPONSE_BYTES
        || request.response_deadline.is_zero()
        || request.frame_deadline.is_zero()
        || request.headers.len() > MAX_GUARDED_HEADERS
        || request
            .headers
            .iter()
            .fold(0_usize, |total, (name, value)| {
                total.saturating_add(name.len()).saturating_add(value.len())
            })
            > MAX_GUARDED_HEADER_BYTES
        || request
            .headers
            .iter()
            .any(|(_, value)| value.len() > MAX_GUARDED_HEADER_VALUE_BYTES)
    {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "guarded HTTP URL is invalid",
        )
        .into());
    }
    if let Some(proxy) = &request.proxy
        && (!matches!(proxy.scheme(), "http" | "https")
            || proxy.username() != ""
            || proxy.password().is_some()
            || proxy.query().is_some()
            || proxy.fragment().is_some()
            || !proxy.host_str().is_some_and(|host| {
                host.eq_ignore_ascii_case("localhost")
                    || parse_url_ip(host).is_some_and(|address| address.is_loopback())
            }))
    {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "guarded HTTP proxy must be a credential-free loopback policy proxy",
        )
        .into());
    }
    if request.allow_private_destinations {
        return Ok(());
    }
    if request.proxy.is_none()
        && request.dns_pin.is_none()
        && request
            .url
            .host_str()
            .is_some_and(|host| parse_url_ip(host).is_none())
    {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "direct guarded HTTP hostname requires a DNS address pin",
        )
        .into());
    }
    let address = if let Some((host, address)) = &request.dns_pin {
        if request.url.host_str() != Some(host.as_str()) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "DNS pin host does not match request host",
            )
            .into());
        }
        Some(address.ip())
    } else {
        request.url.host_str().and_then(parse_url_ip)
    };
    if address.is_some_and(is_local_or_private) {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "private or local HTTP destination is forbidden",
        )
        .into());
    }
    Ok(())
}

fn is_local_or_private(address: IpAddr) -> bool {
    !is_public_ip(address)
}

fn parse_url_ip(host: &str) -> Option<IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => is_public_v4(value),
        IpAddr::V6(value) => is_public_v6(value),
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || address.is_multicast()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..=19).contains(&b))
        || a >= 240)
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_v4(mapped);
    }
    if segments[..6] == [0, 0, 0, 0, 0, 0] {
        return is_public_v4(embedded_ipv4(segments[6], segments[7]));
    }
    if segments[0] == 0x0064 && segments[1] == 0xff9b {
        return segments[2..6] == [0, 0, 0, 0]
            && is_public_v4(embedded_ipv4(segments[6], segments[7]));
    }
    if segments[0] == 0x2002 {
        return is_public_v4(embedded_ipv4(segments[1], segments[2]));
    }
    if matches!(segments[4], 0 | 0x0200) && segments[5] == 0x5efe {
        return is_public_v4(embedded_ipv4(segments[6], segments[7]));
    }
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || address.is_unique_local()
        || address.is_unicast_link_local()
        || (segments[0] == 0x2001 && matches!(segments[1], 0 | 0x0db8)))
}

fn embedded_ipv4(high: u16, low: u16) -> Ipv4Addr {
    let [a, b] = high.to_be_bytes();
    let [c, d] = low.to_be_bytes();
    Ipv4Addr::new(a, b, c, d)
}

fn redacted_url(url: &Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    redacted.set_query(url.query().map(|_| "[REDACTED]"));
    redacted.set_fragment(None);
    redacted.to_string()
}

/// Performs one guarded, non-redirecting HTTP GET.
///
/// The caller owns URL, redirect, SSRF, header, proxy-precedence, and DNS-pin
/// policy. This transport always checks the process network guard, disables
/// ambient proxies and redirects, applies the supplied DNS pin and proxy, and
/// bounds the streamed response body.
///
/// # Errors
///
/// Returns a sanitized provider error for a denied or failed request, or a
/// size-limit error before returning an oversized body.
pub async fn guarded_http_fetch(
    request: GuardedHttpFetchRequest,
) -> Result<GuardedHttpFetchResponse, GuardedHttpFetchError> {
    require_process_network()?;
    validate_fetch_timeout(request.timeout)?;
    if request.proxy.is_some() && request.dns_pin.is_some() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "target DNS pin cannot be enforced through a forward proxy",
        )
        .into());
    }
    if request.proxy.is_none() && request.proxy_authentication.is_some() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "proxy authentication requires an explicit proxy",
        )
        .into());
    }
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(request.timeout.min(Duration::from_secs(15)))
        .timeout(request.timeout);
    if let Some((host, address)) = &request.dns_pin {
        builder = builder.resolve(host, *address);
    }
    if let Some(proxy) = &request.proxy {
        let mut proxy = reqwest::Proxy::all(proxy.as_str()).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "configured HTTP proxy URL is invalid",
            )
        })?;
        if let Some(authentication) = &request.proxy_authentication {
            proxy = proxy.basic_auth(authentication.username(), authentication.password());
        }
        builder = builder.proxy(proxy);
    }
    let client = builder.build().map_err(transport_error)?;
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in request.headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "guarded HTTP header name is invalid",
            )
        })?;
        let value = reqwest::header::HeaderValue::from_str(&value).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "guarded HTTP header value is invalid",
            )
        })?;
        headers.insert(name, value);
    }
    let response = client
        .get(request.url)
        .headers(headers)
        .send()
        .await
        .map_err(transport_error)?;
    if response
        .content_length()
        .is_some_and(|length| length > request.max_bytes as u64)
    {
        return Err(GuardedHttpFetchError::SizeLimit {
            limit: request.max_bytes,
        });
    }
    let status = response.status().as_u16();
    let final_url = response.url().clone();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport_error)?;
        if body.len().saturating_add(chunk.len()) > request.max_bytes {
            return Err(GuardedHttpFetchError::SizeLimit {
                limit: request.max_bytes,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(GuardedHttpFetchResponse {
        status,
        final_url,
        content_type,
        body,
        location,
    })
}

fn validate_fetch_timeout(timeout: Duration) -> Result<(), GuardedHttpFetchError> {
    if timeout.is_zero() || timeout > Duration::from_mins(2) {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "guarded HTTP fetch timeout is invalid",
        )
        .into());
    }
    Ok(())
}

/// Process-local outbound-network denial used by replay and offline test
/// harnesses.
///
/// Every production HTTP client in `rw-providers` is built through this module,
/// and every request boundary checks the same counter. Keeping the guard alive
/// therefore makes an accidental live call fail with [`ProviderErrorKind::NetworkDisabled`]
/// before a socket is opened, even when an adapter was configured with
/// [`NetworkPolicy::Allow`].
#[derive(Debug)]
pub struct ProcessNetworkDenyGuard {
    active: bool,
}

impl Drop for ProcessNetworkDenyGuard {
    fn drop(&mut self) {
        if self.active {
            let previous = PROCESS_NETWORK_DENY_DEPTH.fetch_sub(1, Ordering::AcqRel);
            debug_assert!(previous > 0, "process network-deny guard underflow");
            self.active = false;
        }
    }
}

/// Denies outbound networking in this process until the returned guard drops.
///
/// The guard is reference-counted so nested replay/test harnesses compose.
#[must_use]
pub fn deny_outbound_network_for_process() -> ProcessNetworkDenyGuard {
    if PROCESS_NETWORK_DENY_DEPTH
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |depth| {
            depth.checked_add(1)
        })
        .is_err()
    {
        // An overflow would require more simultaneously live guards than the
        // address space can hold, so continuing with networking accidentally
        // re-enabled is less safe than terminating.
        std::process::abort();
    }
    ProcessNetworkDenyGuard { active: true }
}

fn process_network_is_denied() -> bool {
    PROCESS_NETWORK_DENY_DEPTH.load(Ordering::Acquire) > 0
}

pub(crate) fn build_client_with_proxy_auth(
    proxy: Option<&Url>,
    proxy_authentication: Option<&ProxyAuthentication>,
) -> Result<reqwest::Client, ProviderError> {
    // Client construction is local and does not open a socket. Enforcing the
    // process guard here makes unrelated parallel tests/runtime composition
    // fail while an offline guard is alive. Every actual request boundary
    // must enforce the guard immediately before sending instead.
    // Never let reqwest's ambient system-proxy discovery create an undocumented
    // precedence path. ProxySettings has already resolved explicit/env/NO_PROXY.
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_mins(5));
    if proxy.is_none() && proxy_authentication.is_some() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "proxy authentication requires a configured proxy URL",
        ));
    }
    if let Some(proxy) = proxy {
        let mut configured_proxy = reqwest::Proxy::all(proxy.as_str()).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "configured provider proxy URL is invalid",
            )
        })?;
        if let Some(authentication) = proxy_authentication {
            configured_proxy =
                configured_proxy.basic_auth(authentication.username(), authentication.password());
        }
        builder = builder.proxy(configured_proxy);
    }
    builder.build().map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!("could not build provider HTTP client: {error}"),
        )
    })
}

pub(crate) fn require_network(policy: NetworkPolicy) -> Result<(), ProviderError> {
    if policy == NetworkPolicy::Deny || process_network_is_denied() {
        return Err(network_disabled_error());
    }
    Ok(())
}

/// Enforces the process-wide guard for injected clients that were not created
/// by [`build_client_with_proxy_auth`].
pub(crate) fn require_process_network() -> Result<(), ProviderError> {
    if process_network_is_denied() {
        return Err(network_disabled_error());
    }
    Ok(())
}

fn network_disabled_error() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::NetworkDisabled,
        "live provider networking is disabled; use a replay fixture",
    )
}

pub(crate) fn response_error(response: &Response) -> Option<ProviderError> {
    let status = response.status();
    if status.is_success() {
        return None;
    }
    let kind = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ProviderErrorKind::Authentication,
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => ProviderErrorKind::Timeout,
        StatusCode::TOO_MANY_REQUESTS => ProviderErrorKind::RateLimited,
        status if status.is_server_error() => ProviderErrorKind::Server,
        _ => ProviderErrorKind::InvalidRequest,
    };
    let mut error = ProviderError::new(kind, format!("provider returned HTTP {status}"));
    if let Some(value) = response.headers().get(RETRY_AFTER)
        && let Ok(value) = value.to_str()
        && let Ok(seconds) = value.parse::<u64>()
    {
        error.retry_after_ms = Some(seconds.saturating_mul(1_000));
    }
    Some(error)
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn transport_error(error: reqwest::Error) -> ProviderError {
    let kind = if error.is_timeout() {
        ProviderErrorKind::Timeout
    } else if error.is_builder() {
        ProviderErrorKind::InvalidRequest
    } else {
        ProviderErrorKind::Network
    };
    ProviderError::new(kind, format!("provider request failed: {error}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };

    use super::*;

    #[test]
    fn guarded_destination_rejects_reserved_and_mapped_private_addresses() {
        for address in [
            "100.64.0.1",
            "198.18.0.1",
            "::ffff:127.0.0.1",
            "2002:7f00:1::",
        ] {
            let address: IpAddr = address.parse().expect("IP");
            assert!(is_local_or_private(address), "{address} must be denied");
        }
    }

    #[test]
    fn guarded_request_debug_redacts_url_queries_and_proxy_userinfo() {
        let request = GuardedHttpRequest {
            method: GuardedHttpMethod::Get,
            url: Url::parse("https://example.com/mcp?token=secret-canary").expect("URL"),
            headers: Vec::new(),
            body: Vec::new(),
            proxy: Some(Url::parse("http://user:proxy-canary@127.0.0.1:8080").expect("proxy")),
            proxy_authentication: None,
            dns_pin: None,
            allow_private_destinations: false,
            response_deadline: Duration::from_secs(1),
            frame_deadline: Duration::from_secs(1),
            max_frame_bytes: 1,
            max_body_bytes: 1,
        };
        let debug = format!("{request:?}");
        assert!(!debug.contains("secret-canary"));
        assert!(!debug.contains("proxy-canary"));
    }

    #[tokio::test]
    async fn guarded_stream_request_denies_private_destinations_by_default() {
        let error = guarded_http_request(GuardedHttpRequest {
            method: GuardedHttpMethod::Get,
            url: Url::parse("http://127.0.0.1:9/private").expect("URL"),
            headers: Vec::new(),
            body: Vec::new(),
            proxy: None,
            proxy_authentication: None,
            dns_pin: None,
            allow_private_destinations: false,
            response_deadline: Duration::from_secs(1),
            frame_deadline: Duration::from_secs(1),
            max_frame_bytes: 1024,
            max_body_bytes: 1024,
        })
        .await
        .err()
        .expect("private destination must fail");
        assert!(matches!(
            error,
            GuardedHttpFetchError::Provider(ProviderError {
                kind: ProviderErrorKind::InvalidRequest,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn guarded_stream_request_supports_post_body_with_bounded_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.ends_with(b"payload") {
                let length = socket.read(&mut buffer).await.expect("request");
                assert_ne!(length, 0, "request ended before body");
                request.extend_from_slice(&buffer[..length]);
            }
            let request = String::from_utf8_lossy(&request);
            assert!(request.starts_with("POST /mcp HTTP/1.1"));
            assert!(request.ends_with("payload"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 9\r\nConnection: close\r\n\r\ndata: x\n\n").await.expect("response");
        });
        let mut response = guarded_http_request(GuardedHttpRequest {
            method: GuardedHttpMethod::Post,
            url: Url::parse(&format!("http://{address}/mcp")).expect("URL"),
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: b"payload".to_vec(),
            proxy: None,
            proxy_authentication: None,
            dns_pin: None,
            allow_private_destinations: true,
            response_deadline: Duration::from_secs(2),
            frame_deadline: Duration::from_secs(1),
            max_frame_bytes: 64,
            max_body_bytes: 64,
        })
        .await
        .expect("guarded POST");
        assert_eq!(response.status, 200);
        let body = response.body.next().await.expect("frame").expect("body");
        assert_eq!(body, b"data: x\n\n");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn guarded_fetch_never_follows_redirects_and_returns_bounded_neutral_parts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.expect("request");
            socket
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: /private\r\nContent-Type: text/plain\r\nContent-Length: 8\r\nConnection: close\r\n\r\nredirect",
                )
                .await
                .expect("response");
        });
        let url = Url::parse(&format!("http://{address}/start")).expect("URL");
        let response = guarded_http_fetch(GuardedHttpFetchRequest {
            url: url.clone(),
            headers: vec![("accept".to_owned(), "text/plain".to_owned())],
            proxy: None,
            proxy_authentication: None,
            dns_pin: None,
            max_bytes: 8,
            timeout: Duration::from_mins(1),
        })
        .await
        .expect("fetch");
        server.await.expect("server task");

        assert_eq!(response.status, 302);
        assert_eq!(response.final_url, url);
        assert_eq!(response.content_type.as_deref(), Some("text/plain"));
        assert_eq!(response.location.as_deref(), Some("/private"));
        assert_eq!(response.body, b"redirect");
    }

    #[tokio::test]
    async fn guarded_fetch_rejects_a_forward_proxy_combined_with_target_dns_pinning() {
        let error = guarded_http_fetch(GuardedHttpFetchRequest {
            url: Url::parse("https://example.invalid/").expect("target URL"),
            headers: Vec::new(),
            proxy: Some(Url::parse("http://127.0.0.1:8080").expect("proxy URL")),
            proxy_authentication: None,
            dns_pin: Some((
                "example.invalid".to_owned(),
                "1.1.1.1:443".parse().expect("pin"),
            )),
            max_bytes: 8,
            timeout: Duration::from_mins(1),
        })
        .await
        .expect_err("proxy plus target pin must fail closed");
        assert!(matches!(
            error,
            GuardedHttpFetchError::Provider(ProviderError {
                kind: ProviderErrorKind::InvalidRequest,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn guarded_fetch_chains_through_authenticated_explicit_proxy() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let length = socket.read(&mut buffer).await.expect("request");
                if length == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..length]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(bytes).expect("UTF-8 request");
            assert!(
                request.starts_with("GET http://example.invalid/through-proxy HTTP/1.1\r\n"),
                "{request:?}"
            );
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("\r\nproxy-authorization: basic dxnlcjpzzwnyzxqty2fuyxj5\r\n"),
                "{request:?}"
            );
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .expect("response");
        });
        let authentication = ProxyAuthentication::new("user", crate::Secret::new("secret-canary"));
        assert!(!format!("{authentication:?}").contains("secret-canary"));
        let response = guarded_http_fetch(GuardedHttpFetchRequest {
            url: Url::parse("http://example.invalid/through-proxy").expect("target URL"),
            headers: Vec::new(),
            proxy: Some(Url::parse(&format!("http://{address}")).expect("proxy URL")),
            proxy_authentication: Some(authentication),
            dns_pin: None,
            max_bytes: 8,
            timeout: Duration::from_mins(1),
        })
        .await
        .expect("fetch through proxy");
        server.await.expect("server task");
        assert_eq!(response.body, b"ok");
    }
}
