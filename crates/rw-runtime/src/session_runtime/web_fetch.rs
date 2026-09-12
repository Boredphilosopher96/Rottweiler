use super::ResolvedToolProxy;
use async_trait::async_trait;
use rw_providers::{
    GuardedHttpFetchError, GuardedHttpFetchRequest, ProxyEnvironment, ProxySettings,
    guarded_http_fetch,
};
use rw_tools::{
    CancellationToken, EgressDecision, EgressPin, EgressPolicy, FetchRequest, FetchResponse,
    SupervisedEgressProxy, ToolError, UpstreamProxy, WebFetcher,
};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};
use url::{Host, Url};

pub(super) const MAX_REDIRECTS: usize = 5;

#[derive(Clone)]
pub(super) struct PolicyWebFetcher {
    pub(super) allow_loopback: bool,
    pub(super) proxies: ProxySettings,
    pub(super) corporate_proxy: Option<ResolvedToolProxy>,
}

pub(super) struct ValidatedWebTarget {
    pub(super) direct_pin: Option<(String, SocketAddr)>,
    pub(super) proxy_pin: EgressPin,
}

pub(super) struct OfflineWebFetcher;

#[async_trait]
impl WebFetcher for OfflineWebFetcher {
    async fn fetch(
        &self,
        _request: FetchRequest,
        _cancellation: CancellationToken,
    ) -> std::result::Result<FetchResponse, ToolError> {
        Err(ToolError::Network(
            "webfetch is disabled while replaying an offline fixture".to_owned(),
        ))
    }
}

impl PolicyWebFetcher {
    pub(super) fn new(allow_loopback: bool, global_proxy: Option<ResolvedToolProxy>) -> Self {
        let configured_url = global_proxy.as_ref().map(|proxy| proxy.url.clone());
        Self {
            allow_loopback,
            proxies: ProxySettings {
                global: configured_url,
                per_provider: BTreeMap::new(),
                environment: ProxyEnvironment::capture(),
            },
            corporate_proxy: global_proxy,
        }
    }

    pub(super) async fn validate_and_pin(
        &self,
        url: &Url,
        policy: &EgressPolicy,
    ) -> std::result::Result<ValidatedWebTarget, ToolError> {
        if !matches!(url.scheme(), "http" | "https")
            || url.username() != ""
            || url.password().is_some()
        {
            return Err(ToolError::Network(
                "webfetch requires an http(s) URL without userinfo".to_owned(),
            ));
        }
        if !url
            .host_str()
            .is_some_and(|host| policy.allows_domain(host))
        {
            return Err(ToolError::Network("network domain was not declared".into()));
        }
        let port = url
            .port_or_known_default()
            .ok_or_else(|| ToolError::Network("URL has no usable port".to_owned()))?;
        match url.host() {
            Some(Host::Ipv4(address)) => {
                self.validate_ip(IpAddr::V4(address))?;
                validate_egress_decision(
                    policy,
                    address.to_string().as_str(),
                    &[IpAddr::V4(address)],
                )?;
                let socket = SocketAddr::new(IpAddr::V4(address), port);
                Ok(ValidatedWebTarget {
                    direct_pin: None,
                    proxy_pin: EgressPin::new(&address.to_string(), port, vec![socket])
                        .map_err(|error| ToolError::Network(error.to_string()))?,
                })
            }
            Some(Host::Ipv6(address)) => {
                self.validate_ip(IpAddr::V6(address))?;
                validate_egress_decision(
                    policy,
                    address.to_string().as_str(),
                    &[IpAddr::V6(address)],
                )?;
                let socket = SocketAddr::new(IpAddr::V6(address), port);
                Ok(ValidatedWebTarget {
                    direct_pin: None,
                    proxy_pin: EgressPin::new(&address.to_string(), port, vec![socket])
                        .map_err(|error| ToolError::Network(error.to_string()))?,
                })
            }
            Some(Host::Domain(host)) => {
                let addresses = tokio::net::lookup_host((host, port))
                    .await
                    .map_err(|error| ToolError::Network(format!("DNS lookup failed: {error}")))?
                    .collect::<Vec<_>>();
                if addresses.is_empty() {
                    return Err(ToolError::Network("DNS returned no addresses".to_owned()));
                }
                for address in &addresses {
                    self.validate_ip(address.ip())?;
                }
                let ips = addresses.iter().map(SocketAddr::ip).collect::<Vec<_>>();
                validate_egress_decision(policy, host, &ips)?;
                Ok(ValidatedWebTarget {
                    direct_pin: Some((host.to_owned(), addresses[0])),
                    proxy_pin: EgressPin::new(host, port, addresses)
                        .map_err(|error| ToolError::Network(error.to_string()))?,
                })
            }
            None => Err(ToolError::Network("URL has no host".to_owned())),
        }
    }

    pub(super) fn validate_ip(&self, address: IpAddr) -> std::result::Result<(), ToolError> {
        if self.allow_loopback && address.is_loopback() {
            return Ok(());
        }
        if is_public_ip(address) {
            Ok(())
        } else {
            Err(ToolError::Network(
                "local, private, reserved, and non-routable targets are blocked".to_owned(),
            ))
        }
    }
}

#[async_trait]
impl WebFetcher for PolicyWebFetcher {
    #[allow(clippy::too_many_lines)]
    async fn fetch(
        &self,
        mut request: FetchRequest,
        cancellation: CancellationToken,
    ) -> std::result::Result<FetchResponse, ToolError> {
        let original_origin = origin(&request.url);
        let restricted = request.allowed_domains.is_some();
        let mut policy = request
            .allowed_domains
            .as_ref()
            .map_or_else(EgressPolicy::default, |domains| {
                EgressPolicy::new(domains.iter())
            })
            .with_private_destinations(self.allow_loopback);
        let original_host = request
            .url
            .host_str()
            .ok_or_else(|| ToolError::Network("URL has no host".to_owned()))?;
        if !restricted && !policy.allow_domain(original_host) {
            return Err(ToolError::Network(
                "webfetch requested an invalid network domain".to_owned(),
            ));
        }
        for redirect in 0..=MAX_REDIRECTS {
            if cancellation.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let validated = self.validate_and_pin(&request.url, &policy).await?;
            let mut outgoing = Vec::with_capacity(request.headers.len());
            for (name, value) in &request.headers {
                let lower = name.to_ascii_lowercase();
                if matches!(
                    lower.as_str(),
                    "host" | "connection" | "proxy-authorization"
                ) {
                    return Err(ToolError::Network(format!(
                        "webfetch header {name:?} is not allowed"
                    )));
                }
                if origin(&request.url) != original_origin
                    && !cross_origin_webfetch_header_is_safe(&lower)
                {
                    continue;
                }
                outgoing.push((name.clone(), value.clone()));
            }
            let proxy_resolution = self.proxies.resolve_global(&request.url);
            let mut supervised_proxy = None;
            let (proxy, dns_pin) = if let Some(resolution) = proxy_resolution {
                let upstream = self
                    .corporate_proxy
                    .as_ref()
                    .filter(|configured| configured.url == resolution.url)
                    .map_or_else(
                        || UpstreamProxy::new(resolution.url.clone()),
                        |configured| Ok(configured.upstream.clone()),
                    )
                    .map_err(|error| ToolError::Network(error.to_string()))?;
                let local = SupervisedEgressProxy::start_with_upstream_and_pins(
                    policy.clone(),
                    Some(upstream),
                    vec![validated.proxy_pin],
                )
                .map_err(|error| ToolError::Network(error.to_string()))?;
                let url = Url::parse(&local.url())
                    .map_err(|error| ToolError::Network(error.to_string()))?;
                supervised_proxy = Some(local);
                (Some(url), None)
            } else {
                (None, validated.direct_pin)
            };
            let response = tokio::select! {
                response = guarded_http_fetch(GuardedHttpFetchRequest {
                    url: request.url.clone(),
                    headers: outgoing,
                    proxy,
                    proxy_authentication: None,
                    dns_pin,
                    max_bytes: request.max_bytes,
                    timeout: std::time::Duration::from_mins(1),
                }) => {
                    response.map_err(|error| match error {
                        GuardedHttpFetchError::Provider(error) => {
                            ToolError::Network(error.to_string())
                        }
                        GuardedHttpFetchError::SizeLimit { limit }
                        | GuardedHttpFetchError::FrameLimit { limit } => {
                            ToolError::SizeLimit { limit }
                        }
                        GuardedHttpFetchError::Deadline => {
                            ToolError::Network("HTTP response deadline expired".to_owned())
                        }
                    })?
                },
                () = cancellation.cancelled() => return Err(ToolError::Cancelled),
            };
            drop(supervised_proxy);
            if is_redirect(response.status) {
                if redirect == MAX_REDIRECTS {
                    return Err(ToolError::Network(
                        "webfetch redirect limit exceeded".to_owned(),
                    ));
                }
                let location = response
                    .location
                    .as_deref()
                    .ok_or_else(|| ToolError::Network("redirect omitted Location".to_owned()))?
                    .to_owned();
                request.url = request
                    .url
                    .join(&location)
                    .map_err(|error| ToolError::Network(format!("invalid redirect: {error}")))?;
                continue;
            }
            return Ok(FetchResponse {
                status: response.status,
                final_url: response.final_url,
                content_type: response.content_type,
                body: response.body,
            });
        }
        Err(ToolError::Network("webfetch redirect loop".to_owned()))
    }
}

pub(super) fn cross_origin_webfetch_header_is_safe(name: &str) -> bool {
    matches!(name, "accept" | "accept-language" | "user-agent")
}

pub(super) fn validate_egress_decision(
    policy: &EgressPolicy,
    host: &str,
    addresses: &[IpAddr],
) -> std::result::Result<(), ToolError> {
    match policy.evaluate(host, addresses) {
        EgressDecision::Allowed => Ok(()),
        EgressDecision::ApprovalRequired => Err(ToolError::Network(format!(
            "network domain {host:?} was not declared for this request"
        ))),
        EgressDecision::HardDenied => Err(ToolError::Network(
            "local, private, reserved, and non-routable targets are blocked".to_owned(),
        )),
    }
}

pub(super) fn origin(url: &Url) -> (String, String, Option<u16>) {
    (
        url.scheme().to_owned(),
        url.host_str().unwrap_or_default().to_ascii_lowercase(),
        url.port_or_known_default(),
    )
}

pub(super) fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

pub(super) fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => is_public_v6(address),
    }
}

pub(super) fn is_public_v4(address: Ipv4Addr) -> bool {
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

pub(super) fn is_public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_v4(mapped);
    }
    if segments[..6] == [0, 0, 0, 0, 0, 0] {
        return is_public_v4(embedded_ipv4(segments[6], segments[7]));
    }
    if segments[0] == 0x0064 && segments[1] == 0xff9b {
        if segments[2..6] == [0, 0, 0, 0] {
            return is_public_v4(embedded_ipv4(segments[6], segments[7]));
        }
        return false;
    }
    if segments[0] == 0x2002 {
        return is_public_v4(embedded_ipv4(segments[1], segments[2]));
    }
    if segments[0] == 0x2001 && segments[1] == 0 {
        return false;
    }
    if matches!(segments[4], 0 | 0x0200) && segments[5] == 0x5efe {
        return is_public_v4(embedded_ipv4(segments[6], segments[7]));
    }
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || address.is_unique_local()
        || address.is_unicast_link_local()
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

pub(super) fn embedded_ipv4(high: u16, low: u16) -> Ipv4Addr {
    let [a, b] = high.to_be_bytes();
    let [c, d] = low.to_be_bytes();
    Ipv4Addr::new(a, b, c, d)
}
