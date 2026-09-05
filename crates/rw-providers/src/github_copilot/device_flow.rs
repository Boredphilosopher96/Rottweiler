use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use url::{Host, Url};

use crate::{
    Clock, Delay, ProviderError, ProviderErrorKind, ProxyAuthentication, Secret, TokioClock,
    TokioDelay,
    http::{
        build_client_with_proxy_auth, require_process_network, response_error, transport_error,
    },
};

/// Public GitHub device-authorization endpoint used by released clients.
pub const GITHUB_COPILOT_DEVICE_CODE_ENDPOINT: &str = "https://github.com/login/device/code";
/// Public GitHub device-token endpoint used by released clients.
pub const GITHUB_COPILOT_ACCESS_TOKEN_ENDPOINT: &str =
    "https://github.com/login/oauth/access_token";
/// Public native-client identity used by GitHub Copilot CLI-compatible device
/// flows.
///
/// OAuth device-flow client ids are public application identifiers, not
/// secrets. This application identity authorizes access to the Copilot catalog.
pub const GITHUB_COPILOT_CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";

const DEVICE_SCOPE: &str = "read:user";
const POLLING_SAFETY_MARGIN: Duration = Duration::from_secs(3);

/// One redacted device grant returned before the user approves access.
pub struct GitHubCopilotDeviceAuthorization {
    verification_uri: Url,
    user_code: Secret,
    device_code: Secret,
    interval: Duration,
    expires_in: Duration,
}

impl fmt::Debug for GitHubCopilotDeviceAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubCopilotDeviceAuthorization")
            .field("verification_uri", &self.verification_uri)
            .field("user_code", &"[REDACTED]")
            .field("device_code", &"[REDACTED]")
            .field("interval", &self.interval)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

impl GitHubCopilotDeviceAuthorization {
    /// Browser page where the user enters [`Self::user_code`].
    #[must_use]
    pub const fn verification_uri(&self) -> &Url {
        &self.verification_uri
    }

    /// Short code that must be shown directly to the user and never logged.
    #[must_use]
    pub fn user_code(&self) -> &str {
        self.user_code.expose_secret()
    }
}

/// Successful GitHub device-flow credential.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubCopilotAccessToken(Secret);

impl GitHubCopilotAccessToken {
    /// Consumes the typed token at the credential-storage boundary.
    #[must_use]
    pub fn into_secret(self) -> Secret {
        self.0
    }

    /// Borrows the token for redactor registration or provider composition.
    #[must_use]
    pub const fn secret(&self) -> &Secret {
        &self.0
    }
}

/// Result of one device-token polling attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitHubDevicePoll {
    /// User action is still pending.
    Pending,
    /// Server requires a slower interval. A supplied interval replaces the
    /// RFC 8628 additive five-second backoff.
    SlowDown { interval: Option<Duration> },
    /// User or organization denied the request.
    Denied,
    /// Device grant expired remotely.
    Expired,
    /// Authorization completed.
    Authorized(GitHubCopilotAccessToken),
}

/// Injectable GitHub device-flow boundary used by deterministic tests.
#[async_trait]
pub trait GitHubDeviceFlowTransport: Send + Sync + fmt::Debug {
    /// Starts a device grant using the supplied pinned public client id.
    async fn begin(
        &self,
        client_id: &str,
        user_agent: &str,
    ) -> Result<GitHubCopilotDeviceAuthorization, ProviderError>;

    /// Polls one grant without exposing the device code through diagnostics.
    async fn poll(
        &self,
        client_id: &str,
        device_code: &Secret,
        user_agent: &str,
    ) -> Result<GitHubDevicePoll, ProviderError>;
}

/// Cancellation signal for an in-progress device grant.
#[derive(Clone, Debug, Default)]
pub struct DeviceFlowCancellation {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl DeviceFlowCancellation {
    /// Cancels current and future waits using this signal.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.cancelled.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

/// GitHub Copilot device authorization using a caller-owned OAuth app.
pub struct GitHubCopilotDeviceFlow {
    client_id: String,
    user_agent: String,
    transport: Arc<dyn GitHubDeviceFlowTransport>,
    clock: Arc<dyn Clock>,
    delay: Arc<dyn Delay>,
}

impl fmt::Debug for GitHubCopilotDeviceFlow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubCopilotDeviceFlow")
            .field("client_id_configured", &true)
            .field("user_agent", &self.user_agent)
            .finish_non_exhaustive()
    }
}

impl GitHubCopilotDeviceFlow {
    /// Builds the public GitHub.com device flow with redirects and ambient
    /// proxy discovery disabled.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for an empty client id or invalid proxy.
    pub fn new(
        client_id: impl Into<String>,
        proxy: Option<&Url>,
        proxy_authentication: Option<&ProxyAuthentication>,
    ) -> Result<Self, ProviderError> {
        let client_id = client_id.into();
        let client_id = validated_client_id(&client_id)?;
        let client = build_client_with_proxy_auth(proxy, proxy_authentication)?;
        let device_code_endpoint = fixed_url(GITHUB_COPILOT_DEVICE_CODE_ENDPOINT)?;
        let access_token_endpoint = fixed_url(GITHUB_COPILOT_ACCESS_TOKEN_ENDPOINT)?;
        Ok(Self::with_transport(
            client_id,
            Arc::new(ReqwestGitHubDeviceFlowTransport {
                client,
                device_code_endpoint,
                access_token_endpoint,
            }),
            Arc::new(TokioClock),
            Arc::new(TokioDelay),
        ))
    }

    /// Uses Rottweiler's built-in public GitHub OAuth app id.
    ///
    /// # Errors
    ///
    /// Fails closed if the built-in client id is malformed.
    pub fn from_compiled(
        proxy: Option<&Url>,
        proxy_authentication: Option<&ProxyAuthentication>,
    ) -> Result<Self, ProviderError> {
        Self::new(GITHUB_COPILOT_CLIENT_ID, proxy, proxy_authentication)
    }

    /// Builds a flow around injected I/O, time, and delay boundaries.
    ///
    /// This is public so downstream acceptance harnesses can remain offline.
    #[must_use]
    pub fn with_transport(
        client_id: String,
        transport: Arc<dyn GitHubDeviceFlowTransport>,
        clock: Arc<dyn Clock>,
        delay: Arc<dyn Delay>,
    ) -> Self {
        Self {
            client_id,
            user_agent: format!("rottweiler/{}", env!("CARGO_PKG_VERSION")),
            transport,
            clock,
            delay,
        }
    }

    /// Builds a deterministic HTTP flow whose two endpoints must be loopback.
    /// Production integrations must use [`Self::new`] or [`Self::from_compiled`].
    ///
    /// # Errors
    ///
    /// Rejects empty client ids and non-loopback endpoints.
    #[doc(hidden)]
    pub fn with_test_endpoints(
        client_id: &str,
        device_code_endpoint: Url,
        access_token_endpoint: Url,
        clock: Arc<dyn Clock>,
        delay: Arc<dyn Delay>,
    ) -> Result<Self, ProviderError> {
        let client_id = validated_client_id(client_id)?;
        if !is_loopback_http(&device_code_endpoint) || !is_loopback_http(&access_token_endpoint) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "GitHub device-flow test endpoints must be HTTP loopback URLs",
            ));
        }
        let client = build_client_with_proxy_auth(None, None)?;
        Ok(Self::with_transport(
            client_id,
            Arc::new(ReqwestGitHubDeviceFlowTransport {
                client,
                device_code_endpoint,
                access_token_endpoint,
            }),
            clock,
            delay,
        ))
    }

    /// Starts a device grant.
    ///
    /// # Errors
    ///
    /// Returns a sanitized configuration, transport, or protocol error.
    pub async fn begin(&self) -> Result<GitHubCopilotDeviceSession, ProviderError> {
        validated_client_id(&self.client_id)?;
        let authorization = self
            .transport
            .begin(&self.client_id, &self.user_agent)
            .await?;
        validate_authorization(&authorization)?;
        let started_at = self.clock.now();
        Ok(GitHubCopilotDeviceSession {
            authorization,
            client_id: self.client_id.clone(),
            user_agent: self.user_agent.clone(),
            transport: Arc::clone(&self.transport),
            clock: Arc::clone(&self.clock),
            delay: Arc::clone(&self.delay),
            started_at,
        })
    }
}

/// One in-progress GitHub device grant.
pub struct GitHubCopilotDeviceSession {
    authorization: GitHubCopilotDeviceAuthorization,
    client_id: String,
    user_agent: String,
    transport: Arc<dyn GitHubDeviceFlowTransport>,
    clock: Arc<dyn Clock>,
    delay: Arc<dyn Delay>,
    started_at: tokio::time::Instant,
}

impl fmt::Debug for GitHubCopilotDeviceSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubCopilotDeviceSession")
            .field("authorization", &self.authorization)
            .field("client_id_configured", &true)
            .finish_non_exhaustive()
    }
}

impl GitHubCopilotDeviceSession {
    /// Public OAuth application id that created this grant.
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Browser page for this grant.
    #[must_use]
    pub const fn verification_uri(&self) -> &Url {
        self.authorization.verification_uri()
    }

    /// Short user-entered code. Do not log or persist it.
    #[must_use]
    pub fn user_code(&self) -> &str {
        self.authorization.user_code()
    }

    /// Server-mandated polling interval before the safety margin.
    #[must_use]
    pub const fn polling_interval(&self) -> Duration {
        self.authorization.interval
    }

    /// Lifetime of the device grant measured from [`GitHubCopilotDeviceFlow::begin`].
    #[must_use]
    pub const fn expires_in(&self) -> Duration {
        self.authorization.expires_in
    }

    /// Polls until authorization, denial, expiry, or cancellation.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error and never includes codes or tokens.
    pub async fn complete(
        self,
        cancellation: &DeviceFlowCancellation,
    ) -> Result<GitHubCopilotAccessToken, ProviderError> {
        let deadline = self.started_at + self.authorization.expires_in;
        let mut interval = self.authorization.interval;
        loop {
            if cancellation.cancelled.load(Ordering::Acquire) {
                return Err(device_cancelled());
            }
            let remaining = deadline.saturating_duration_since(self.clock.now());
            if remaining.is_zero() {
                return Err(device_expired());
            }
            let wait = interval
                .saturating_add(POLLING_SAFETY_MARGIN)
                .min(remaining);
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(device_cancelled()),
                () = self.delay.sleep(wait) => {}
            }
            if self.clock.now() >= deadline {
                return Err(device_expired());
            }
            let poll = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(device_cancelled()),
                () = tokio::time::sleep_until(deadline) => return Err(device_expired()),
                result = self.transport.poll(
                    &self.client_id,
                    &self.authorization.device_code,
                    &self.user_agent,
                ) => result?,
            };
            match poll {
                GitHubDevicePoll::Authorized(token) => return Ok(token),
                GitHubDevicePoll::Denied => {
                    return Err(ProviderError::new(
                        ProviderErrorKind::Authentication,
                        "GitHub device authorization was denied",
                    ));
                }
                GitHubDevicePoll::Expired => return Err(device_expired()),
                GitHubDevicePoll::SlowDown {
                    interval: server_interval,
                } => {
                    interval = server_interval
                        .filter(|value| !value.is_zero())
                        .unwrap_or_else(|| interval.saturating_add(Duration::from_secs(5)));
                }
                GitHubDevicePoll::Pending => {}
            }
        }
    }
}

#[derive(Debug)]
struct ReqwestGitHubDeviceFlowTransport {
    client: reqwest::Client,
    device_code_endpoint: Url,
    access_token_endpoint: Url,
}

#[derive(Serialize)]
struct DeviceCodeRequest<'a> {
    client_id: &'a str,
    scope: &'static str,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    verification_uri: String,
    user_code: String,
    device_code: String,
    interval: u64,
    expires_in: u64,
}

#[derive(Serialize)]
struct AccessTokenRequest<'a> {
    client_id: &'a str,
    device_code: &'a str,
    grant_type: &'static str,
}

#[derive(Deserialize)]
struct AccessTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    interval: Option<u64>,
}

#[async_trait]
impl GitHubDeviceFlowTransport for ReqwestGitHubDeviceFlowTransport {
    async fn begin(
        &self,
        client_id: &str,
        user_agent: &str,
    ) -> Result<GitHubCopilotDeviceAuthorization, ProviderError> {
        require_process_network()?;
        let response = self
            .client
            .post(self.device_code_endpoint.clone())
            .headers(github_json_headers(user_agent)?)
            .json(&DeviceCodeRequest {
                client_id,
                scope: DEVICE_SCOPE,
            })
            .send()
            .await
            .map_err(transport_error)?;
        if let Some(error) = response_error(&response) {
            return Err(error);
        }
        let data: DeviceCodeResponse = crate::token_response::read_json(
            response,
            "GitHub device authorization returned an invalid response",
        )
        .await?;
        let expires_in = crate::token_response::expiry_duration(data.expires_in)?;
        Ok(GitHubCopilotDeviceAuthorization {
            verification_uri: Url::parse(&data.verification_uri).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "GitHub device authorization returned an invalid verification URI",
                )
            })?,
            user_code: Secret::new(data.user_code),
            device_code: Secret::new(data.device_code),
            interval: Duration::from_secs(data.interval),
            expires_in,
        })
    }

    async fn poll(
        &self,
        client_id: &str,
        device_code: &Secret,
        user_agent: &str,
    ) -> Result<GitHubDevicePoll, ProviderError> {
        require_process_network()?;
        let response = self
            .client
            .post(self.access_token_endpoint.clone())
            .headers(github_json_headers(user_agent)?)
            .json(&AccessTokenRequest {
                client_id,
                device_code: device_code.expose_secret(),
                grant_type: "urn:ietf:params:oauth:grant-type:device_code",
            })
            .send()
            .await
            .map_err(transport_error)?;
        if let Some(error) = response_error(&response) {
            return Err(error);
        }
        let data: AccessTokenResponse = crate::token_response::read_json(
            response,
            "GitHub device authorization returned an invalid polling response",
        )
        .await?;
        interpret_access_token_response(data)
    }
}

fn interpret_access_token_response(
    data: AccessTokenResponse,
) -> Result<GitHubDevicePoll, ProviderError> {
    if let Some(token) = data.access_token.filter(|token| !token.is_empty()) {
        return Ok(GitHubDevicePoll::Authorized(GitHubCopilotAccessToken(
            Secret::new(token),
        )));
    }
    Ok(match data.error.as_deref() {
        Some("authorization_pending") => GitHubDevicePoll::Pending,
        Some("slow_down") => GitHubDevicePoll::SlowDown {
            interval: data
                .interval
                .filter(|value| *value > 0)
                .map(Duration::from_secs),
        },
        Some("expired_token") => GitHubDevicePoll::Expired,
        Some("access_denied") => GitHubDevicePoll::Denied,
        Some(_) | None => {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "GitHub device authorization returned an unknown polling result",
            ));
        }
    })
}

fn github_json_headers(user_agent: &str) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(user_agent).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "GitHub Copilot user agent contains invalid bytes",
            )
        })?,
    );
    Ok(headers)
}

fn validated_client_id(client_id: &str) -> Result<String, ProviderError> {
    let trimmed = client_id.trim();
    if trimmed.is_empty() || trimmed.len() > 512 || trimmed.chars().any(char::is_control) {
        return Err(missing_client_id());
    }
    Ok(trimmed.to_owned())
}

fn fixed_url(value: &str) -> Result<Url, ProviderError> {
    Url::parse(value).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            "built-in GitHub device-flow endpoint is invalid",
        )
    })
}

fn is_loopback_http(url: &Url) -> bool {
    url.scheme() == "http"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && match url.host() {
            Some(Host::Ipv4(address)) => address.is_loopback(),
            Some(Host::Ipv6(address)) => address.is_loopback(),
            Some(Host::Domain("localhost")) => true,
            Some(Host::Domain(_)) | None => false,
        }
}

fn validate_authorization(
    authorization: &GitHubCopilotDeviceAuthorization,
) -> Result<(), ProviderError> {
    let uri = &authorization.verification_uri;
    let exact_public_origin = uri.scheme() == "https"
        && uri.host_str() == Some("github.com")
        && uri.port().is_none()
        && uri.path() == "/login/device"
        && uri.query().is_none()
        && uri.fragment().is_none()
        && uri.username().is_empty()
        && uri.password().is_none();
    if !exact_public_origin
        || authorization.user_code.expose_secret().is_empty()
        || authorization.device_code.expose_secret().is_empty()
        || authorization.interval.is_zero()
        || authorization.expires_in.is_zero()
    {
        return Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            "GitHub device authorization returned incomplete or unsafe grant data",
        ));
    }
    Ok(())
}

fn missing_client_id() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Authentication,
        "this Rottweiler build has no compatible GitHub Copilot OAuth client id",
    )
}

fn device_expired() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Authentication,
        "GitHub device authorization expired; start a new login",
    )
}

fn device_cancelled() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Cancelled,
        "GitHub device authorization was cancelled",
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::Future,
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::Notify,
        time::Instant,
    };
    use url::Url;

    use crate::{Clock, Delay, ProviderError, ProviderErrorKind, Secret, TokioClock, TokioDelay};

    use super::{
        AccessTokenResponse, DeviceFlowCancellation, GitHubCopilotAccessToken,
        GitHubCopilotDeviceAuthorization, GitHubCopilotDeviceFlow, GitHubDeviceFlowTransport,
        GitHubDevicePoll, interpret_access_token_response,
    };

    #[test]
    fn production_client_identity_matches_the_reviewed_copilot_compatibility_profile() {
        assert_eq!(super::GITHUB_COPILOT_CLIENT_ID, "Ov23li8tweQw6odWQebz");
    }

    #[derive(Debug)]
    struct FakeClock(Mutex<Instant>);

    impl FakeClock {
        fn new() -> Self {
            Self(Mutex::new(Instant::now()))
        }

        fn advance(&self, duration: Duration) {
            let mut now = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *now += duration;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    struct FakeDelay {
        clock: Arc<FakeClock>,
        waits: Arc<Mutex<Vec<Duration>>>,
    }

    impl Delay for FakeDelay {
        fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async move {
                self.waits
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(duration);
                self.clock.advance(duration);
            })
        }
    }

    #[derive(Debug)]
    struct FakeTransport {
        clock: Arc<FakeClock>,
        authorization: Mutex<Option<GitHubCopilotDeviceAuthorization>>,
        polls: Mutex<VecDeque<GitHubDevicePoll>>,
        poll_times: Mutex<Vec<Instant>>,
    }

    #[derive(Debug)]
    struct HangingTransport {
        authorization: Mutex<Option<GitHubCopilotDeviceAuthorization>>,
        entered: Arc<Notify>,
        polls: AtomicUsize,
    }

    #[async_trait]
    impl GitHubDeviceFlowTransport for HangingTransport {
        async fn begin(
            &self,
            _client_id: &str,
            _user_agent: &str,
        ) -> Result<GitHubCopilotDeviceAuthorization, ProviderError> {
            self.authorization
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .ok_or_else(|| {
                    ProviderError::new(ProviderErrorKind::Protocol, "fixture grant exhausted")
                })
        }

        async fn poll(
            &self,
            _client_id: &str,
            _device_code: &Secret,
            _user_agent: &str,
        ) -> Result<GitHubDevicePoll, ProviderError> {
            self.polls.fetch_add(1, Ordering::Relaxed);
            self.entered.notify_one();
            std::future::pending().await
        }
    }

    #[async_trait]
    impl GitHubDeviceFlowTransport for FakeTransport {
        async fn begin(
            &self,
            _client_id: &str,
            _user_agent: &str,
        ) -> Result<GitHubCopilotDeviceAuthorization, ProviderError> {
            self.authorization
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .ok_or_else(|| {
                    ProviderError::new(ProviderErrorKind::Protocol, "fixture grant exhausted")
                })
        }

        async fn poll(
            &self,
            _client_id: &str,
            _device_code: &Secret,
            _user_agent: &str,
        ) -> Result<GitHubDevicePoll, ProviderError> {
            self.poll_times
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(self.clock.now());
            self.polls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .ok_or_else(|| {
                    ProviderError::new(ProviderErrorKind::Protocol, "fixture polls exhausted")
                })
        }
    }

    fn authorization(expires_in: Duration) -> GitHubCopilotDeviceAuthorization {
        authorization_with_interval(expires_in, Duration::from_secs(5))
    }

    fn authorization_with_interval(
        expires_in: Duration,
        interval: Duration,
    ) -> GitHubCopilotDeviceAuthorization {
        GitHubCopilotDeviceAuthorization {
            verification_uri: Url::parse("https://github.com/login/device")
                .unwrap_or_else(|error| panic!("fixture URL must parse: {error}")),
            user_code: Secret::new("USER-CODE-PRIVATE"),
            device_code: Secret::new("DEVICE-CODE-PRIVATE"),
            interval,
            expires_in,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn hung_poll_is_interrupted_by_deadline_and_cancellation() {
        let entered = Arc::new(Notify::new());
        let transport = Arc::new(HangingTransport {
            authorization: Mutex::new(Some(authorization_with_interval(
                Duration::from_secs(10),
                Duration::from_secs(1),
            ))),
            entered,
            polls: AtomicUsize::new(0),
        });
        let flow = GitHubCopilotDeviceFlow::with_transport(
            "rottweiler-client".to_owned(),
            transport.clone(),
            Arc::new(TokioClock),
            Arc::new(TokioDelay),
        );
        let started = Instant::now();
        let result = flow
            .begin()
            .await
            .unwrap_or_else(|error| panic!("grant must start: {error}"))
            .complete(&DeviceFlowCancellation::default())
            .await;
        let Err(error) = result else {
            panic!("hung poll must expire");
        };
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        assert_eq!(Instant::now() - started, Duration::from_secs(10));
        assert_eq!(transport.polls.load(Ordering::Relaxed), 1);

        let entered = Arc::new(Notify::new());
        let transport = Arc::new(HangingTransport {
            authorization: Mutex::new(Some(authorization_with_interval(
                Duration::from_secs(100),
                Duration::from_secs(1),
            ))),
            entered: Arc::clone(&entered),
            polls: AtomicUsize::new(0),
        });
        let flow = GitHubCopilotDeviceFlow::with_transport(
            "rottweiler-client".to_owned(),
            transport.clone(),
            Arc::new(TokioClock),
            Arc::new(TokioDelay),
        );
        let session = flow
            .begin()
            .await
            .unwrap_or_else(|error| panic!("grant must start: {error}"));
        let cancellation = DeviceFlowCancellation::default();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move { session.complete(&task_cancellation).await });
        entered.notified().await;
        let cancelled_at = Instant::now();
        cancellation.cancel();
        let result = task
            .await
            .unwrap_or_else(|error| panic!("completion task must join: {error}"));
        let Err(error) = result else {
            panic!("hung poll must cancel");
        };
        assert_eq!(error.kind, ProviderErrorKind::Cancelled);
        assert_eq!(Instant::now(), cancelled_at);
        assert_eq!(transport.polls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn waits_before_first_poll_and_persists_slow_down_interval() {
        let clock = Arc::new(FakeClock::new());
        let start = clock.now();
        let waits = Arc::new(Mutex::new(Vec::new()));
        let transport = Arc::new(FakeTransport {
            clock: Arc::clone(&clock),
            authorization: Mutex::new(Some(authorization(Duration::from_secs(100)))),
            polls: Mutex::new(VecDeque::from([
                GitHubDevicePoll::SlowDown { interval: None },
                GitHubDevicePoll::Pending,
                GitHubDevicePoll::Authorized(GitHubCopilotAccessToken(Secret::new(
                    "ACCESS-TOKEN-PRIVATE",
                ))),
            ])),
            poll_times: Mutex::new(Vec::new()),
        });
        let flow = GitHubCopilotDeviceFlow::with_transport(
            "rottweiler-client".to_owned(),
            transport.clone(),
            clock.clone(),
            Arc::new(FakeDelay {
                clock,
                waits: Arc::clone(&waits),
            }),
        );
        let session = flow
            .begin()
            .await
            .unwrap_or_else(|error| panic!("grant must start: {error}"));
        assert_eq!(session.polling_interval(), Duration::from_secs(5));
        assert_eq!(session.expires_in(), Duration::from_secs(100));
        let token = session
            .complete(&DeviceFlowCancellation::default())
            .await
            .unwrap_or_else(|error| panic!("grant must complete: {error}"));
        assert_eq!(token.secret().expose_secret(), "ACCESS-TOKEN-PRIVATE");
        assert_eq!(
            *waits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![
                Duration::from_secs(8),
                Duration::from_secs(13),
                Duration::from_secs(13)
            ]
        );
        assert_eq!(
            *transport
                .poll_times
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![
                start + Duration::from_secs(8),
                start + Duration::from_secs(21),
                start + Duration::from_secs(34)
            ]
        );
    }

    #[tokio::test]
    async fn cancellation_and_expiry_never_poll() {
        let clock = Arc::new(FakeClock::new());
        let transport = Arc::new(FakeTransport {
            clock: Arc::clone(&clock),
            authorization: Mutex::new(Some(authorization(Duration::from_secs(7)))),
            polls: Mutex::new(VecDeque::new()),
            poll_times: Mutex::new(Vec::new()),
        });
        let flow = GitHubCopilotDeviceFlow::with_transport(
            "rottweiler-client".to_owned(),
            transport.clone(),
            clock.clone(),
            Arc::new(FakeDelay {
                clock,
                waits: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let result = flow
            .begin()
            .await
            .unwrap_or_else(|error| panic!("grant must start: {error}"))
            .complete(&DeviceFlowCancellation::default())
            .await;
        let Err(error) = result else {
            panic!("expired grant must fail");
        };
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        assert!(
            transport
                .poll_times
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );

        let cancellation = DeviceFlowCancellation::default();
        cancellation.cancel();
        let clock = Arc::new(FakeClock::new());
        let transport = Arc::new(FakeTransport {
            clock: Arc::clone(&clock),
            authorization: Mutex::new(Some(authorization(Duration::from_secs(100)))),
            polls: Mutex::new(VecDeque::new()),
            poll_times: Mutex::new(Vec::new()),
        });
        let flow = GitHubCopilotDeviceFlow::with_transport(
            "rottweiler-client".to_owned(),
            transport.clone(),
            clock.clone(),
            Arc::new(FakeDelay {
                clock,
                waits: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let result = flow
            .begin()
            .await
            .unwrap_or_else(|error| panic!("grant must start: {error}"))
            .complete(&cancellation)
            .await;
        let Err(error) = result else {
            panic!("cancelled grant must fail");
        };
        assert_eq!(error.kind, ProviderErrorKind::Cancelled);
        assert!(
            transport
                .poll_times
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn server_slow_down_override_and_denial_are_deterministic() {
        let clock = Arc::new(FakeClock::new());
        let waits = Arc::new(Mutex::new(Vec::new()));
        let transport = Arc::new(FakeTransport {
            clock: Arc::clone(&clock),
            authorization: Mutex::new(Some(authorization(Duration::from_secs(100)))),
            polls: Mutex::new(VecDeque::from([
                GitHubDevicePoll::SlowDown {
                    interval: Some(Duration::from_secs(17)),
                },
                GitHubDevicePoll::Authorized(GitHubCopilotAccessToken(Secret::new(
                    "ACCESS-TOKEN-PRIVATE",
                ))),
            ])),
            poll_times: Mutex::new(Vec::new()),
        });
        let flow = GitHubCopilotDeviceFlow::with_transport(
            "rottweiler-client".to_owned(),
            transport,
            clock.clone(),
            Arc::new(FakeDelay {
                clock,
                waits: Arc::clone(&waits),
            }),
        );
        let _token = flow
            .begin()
            .await
            .unwrap_or_else(|error| panic!("grant must start: {error}"))
            .complete(&DeviceFlowCancellation::default())
            .await
            .unwrap_or_else(|error| panic!("grant must complete: {error}"));
        assert_eq!(
            *waits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![Duration::from_secs(8), Duration::from_secs(20)]
        );

        let clock = Arc::new(FakeClock::new());
        let transport = Arc::new(FakeTransport {
            clock: Arc::clone(&clock),
            authorization: Mutex::new(Some(authorization(Duration::from_secs(100)))),
            polls: Mutex::new(VecDeque::from([GitHubDevicePoll::Denied])),
            poll_times: Mutex::new(Vec::new()),
        });
        let flow = GitHubCopilotDeviceFlow::with_transport(
            "rottweiler-client".to_owned(),
            transport,
            clock.clone(),
            Arc::new(FakeDelay {
                clock,
                waits: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let result = flow
            .begin()
            .await
            .unwrap_or_else(|error| panic!("grant must start: {error}"))
            .complete(&DeviceFlowCancellation::default())
            .await;
        let Err(error) = result else {
            panic!("denied grant must fail");
        };
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        assert_eq!(error.message, "GitHub device authorization was denied");
        assert!(!error.message.contains("USER-CODE-PRIVATE"));

        let clock = Arc::new(FakeClock::new());
        let transport = Arc::new(FakeTransport {
            clock: Arc::clone(&clock),
            authorization: Mutex::new(Some(authorization(Duration::from_secs(100)))),
            polls: Mutex::new(VecDeque::from([GitHubDevicePoll::Expired])),
            poll_times: Mutex::new(Vec::new()),
        });
        let flow = GitHubCopilotDeviceFlow::with_transport(
            "rottweiler-client".to_owned(),
            transport,
            clock.clone(),
            Arc::new(FakeDelay {
                clock,
                waits: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let result = flow
            .begin()
            .await
            .unwrap_or_else(|error| panic!("grant must start: {error}"))
            .complete(&DeviceFlowCancellation::default())
            .await;
        let Err(error) = result else {
            panic!("remote-expired grant must fail");
        };
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        assert_eq!(
            error.message,
            "GitHub device authorization expired; start a new login"
        );
    }

    #[tokio::test]
    async fn production_transport_matches_copilot_json_device_flow_without_client_secret() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("listener must bind: {error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("listener address must resolve: {error}"));
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for body in [
                r#"{"verification_uri":"https://github.com/login/device","user_code":"USER-CODE","device_code":"DEVICE-CODE","interval":1,"expires_in":60}"#,
                r#"{"access_token":"ACCESS-TOKEN"}"#,
            ] {
                let (mut stream, _) = listener
                    .accept()
                    .await
                    .unwrap_or_else(|error| panic!("request must connect: {error}"));
                requests.push(read_request(&mut stream).await);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .unwrap_or_else(|error| panic!("response must write: {error}"));
            }
            requests
        });
        let clock = Arc::new(FakeClock::new());
        let base = Url::parse(&format!("http://{address}/"))
            .unwrap_or_else(|error| panic!("loopback URL must parse: {error}"));
        let flow = GitHubCopilotDeviceFlow::with_test_endpoints(
            "rottweiler-test-client",
            base.join("device")
                .unwrap_or_else(|error| panic!("device URL: {error}")),
            base.join("token")
                .unwrap_or_else(|error| panic!("token URL: {error}")),
            clock.clone(),
            Arc::new(FakeDelay {
                clock,
                waits: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .unwrap_or_else(|error| panic!("test flow must build: {error}"));
        let _token = flow
            .begin()
            .await
            .unwrap_or_else(|error| panic!("grant must start: {error}"))
            .complete(&DeviceFlowCancellation::default())
            .await
            .unwrap_or_else(|error| panic!("grant must complete: {error}"));
        let requests = server
            .await
            .unwrap_or_else(|error| panic!("server task must finish: {error}"));
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| {
            request
                .to_ascii_lowercase()
                .contains("content-type: application/json")
        }));
        assert!(
            requests[0].ends_with(r#"{"client_id":"rottweiler-test-client","scope":"read:user"}"#)
        );
        assert!(requests[1].contains(r#""client_id":"rottweiler-test-client""#));
        assert!(requests[1].contains(r#""device_code":"DEVICE-CODE""#));
        assert!(
            requests[1].contains(r#""grant_type":"urn:ietf:params:oauth:grant-type:device_code""#)
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("client_secret"))
        );
    }

    #[test]
    fn debug_output_redacts_codes_and_tokens() {
        let grant = authorization(Duration::from_secs(10));
        let rendered = format!("{grant:?}");
        assert!(!rendered.contains("USER-CODE-PRIVATE"));
        assert!(!rendered.contains("DEVICE-CODE-PRIVATE"));
        let token = GitHubCopilotAccessToken(Secret::new("ACCESS-TOKEN-PRIVATE"));
        assert!(!format!("{token:?}").contains("ACCESS-TOKEN-PRIVATE"));
    }

    #[test]
    fn unknown_remote_polling_error_is_protocol_not_denial() {
        let result = interpret_access_token_response(AccessTokenResponse {
            access_token: None,
            error: Some("future_private_error".to_owned()),
            interval: None,
        });
        let Err(error) = result else {
            panic!("unknown polling error must fail");
        };
        assert_eq!(error.kind, ProviderErrorKind::Protocol);
        assert!(!error.message.contains("future_private_error"));
    }

    async fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream
                .read(&mut buffer)
                .await
                .unwrap_or_else(|error| panic!("request must read: {error}"));
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or_default();
            if bytes.len() >= header_end + 4 + content_length {
                break;
            }
        }
        String::from_utf8(bytes).unwrap_or_else(|error| panic!("request must be UTF-8: {error}"))
    }
}
