use std::{
    net::SocketAddr,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use futures_util::StreamExt as _;
use reqwest::{Response, StatusCode, header::RETRY_AFTER};
use thiserror::Error;
use url::Url;

use crate::{NetworkPolicy, ProviderError, ProviderErrorKind, ProxyAuthentication};

static PROCESS_NETWORK_DENY_DEPTH: AtomicUsize = AtomicUsize::new(0);

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
        .timeout(Duration::from_secs(60));
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
    require_process_network()?;
    // Never let reqwest's ambient system-proxy discovery create an undocumented
    // precedence path. ProxySettings has already resolved explicit/env/NO_PROXY.
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300));
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
        })
        .await
        .expect("fetch through proxy");
        server.await.expect("server task");
        assert_eq!(response.body, b"ok");
    }
}
