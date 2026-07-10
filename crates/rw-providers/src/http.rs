use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use reqwest::{Response, StatusCode, header::RETRY_AFTER};
use url::Url;

use crate::{NetworkPolicy, ProviderError, ProviderErrorKind, ProxyAuthentication};

static PROCESS_NETWORK_DENY_DEPTH: AtomicUsize = AtomicUsize::new(0);

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
