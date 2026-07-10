use std::{
    fmt,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use url::{Host, Url};

use crate::{
    AuthMaterial, AuthProvider, DEFAULT_OAUTH_CALLBACK_TIMEOUT, KnownSecretRegistrar,
    OAuthAuthorizationCode, OAuthAuthorizationCodeConfig, ProviderError, ProviderErrorKind,
    ProxyAuthentication, Secret,
};

/// Public native-client id used by `OpenAI`'s Codex browser authorization flow.
pub const OPENAI_SUBSCRIPTION_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Browser authorization endpoint for the `ChatGPT` subscription flow.
pub const OPENAI_SUBSCRIPTION_AUTHORIZATION_ENDPOINT: &str =
    "https://auth.openai.com/oauth/authorize";
/// Token exchange and refresh endpoint for the `ChatGPT` subscription flow.
pub const OPENAI_SUBSCRIPTION_TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
/// Raw Responses endpoint backed by a `ChatGPT` Codex subscription.
pub const OPENAI_SUBSCRIPTION_RESPONSES_ENDPOINT: &str =
    "https://chatgpt.com/backend-api/codex/responses";
/// Exact callback required by the public native-client registration.
pub const OPENAI_SUBSCRIPTION_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

/// Builds the fixed `ChatGPT` subscription browser flow.
///
/// # Errors
///
/// Returns an error when the official endpoint or explicit proxy client is invalid.
pub fn openai_subscription_oauth_flow(
    proxy: Option<&Url>,
    proxy_authentication: Option<&ProxyAuthentication>,
) -> Result<OAuthAuthorizationCode, ProviderError> {
    let authorization_endpoint =
        Url::parse(OPENAI_SUBSCRIPTION_AUTHORIZATION_ENDPOINT).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                "invalid built-in ChatGPT authorization endpoint",
            )
        })?;
    let token_endpoint = Url::parse(OPENAI_SUBSCRIPTION_TOKEN_ENDPOINT).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            "invalid built-in ChatGPT token endpoint",
        )
    })?;
    openai_subscription_oauth_flow_with_endpoints(
        authorization_endpoint,
        token_endpoint,
        proxy,
        proxy_authentication,
    )
}

/// Injectable-endpoint form used by deterministic loopback protocol tests.
/// Production composition always calls [`openai_subscription_oauth_flow`].
///
/// # Errors
///
/// Returns an error when the explicit proxy client cannot be constructed.
pub fn openai_subscription_oauth_flow_with_endpoints(
    authorization_endpoint: Url,
    token_endpoint: Url,
    proxy: Option<&Url>,
    proxy_authentication: Option<&ProxyAuthentication>,
) -> Result<OAuthAuthorizationCode, ProviderError> {
    OAuthAuthorizationCode::with_proxy(
        OAuthAuthorizationCodeConfig {
            authorization_endpoint,
            token_endpoint,
            client_id: OPENAI_SUBSCRIPTION_CLIENT_ID.to_owned(),
            scopes: ["openid", "profile", "email", "offline_access"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            callback_timeout: DEFAULT_OAUTH_CALLBACK_TIMEOUT,
        },
        proxy,
        proxy_authentication,
    )
    .map(|flow| {
        flow.with_fixed_localhost_callback(1455, "/auth/callback")
            .with_authorization_parameters([
                ("id_token_add_organizations".to_owned(), "true".to_owned()),
                ("codex_cli_simplified_flow".to_owned(), "true".to_owned()),
                ("originator".to_owned(), "rottweiler".to_owned()),
            ])
    })
}

/// Persistence boundary for refreshed `ChatGPT` subscription credentials.
#[async_trait]
pub trait OpenAiSubscriptionTokenSink: Send + Sync + fmt::Debug {
    /// Persists a rotated refresh token first, then the current access token and
    /// account id, before bearer material may be returned.
    async fn persist(
        &self,
        access_token: &Secret,
        rotated_refresh_token: Option<&Secret>,
        account_id: &Secret,
    ) -> Result<(), ProviderError>;
}

/// Runtime configuration for `ChatGPT` subscription refresh authentication.
pub struct OpenAiSubscriptionAuthConfig {
    /// `OAuth` token endpoint; production uses [`OPENAI_SUBSCRIPTION_TOKEN_ENDPOINT`].
    pub token_endpoint: Url,
    /// Public native-client id.
    pub client_id: String,
    /// Previously persisted access token, when available.
    pub access_token: Option<Secret>,
    /// Long-lived refresh token.
    pub refresh_token: Secret,
    /// Previously derived account id, when available.
    pub account_id: Option<Secret>,
    /// Client origin header.
    pub originator: String,
    /// Versioned user-agent header.
    pub user_agent: String,
    /// Provider-session header.
    pub session_id: String,
}

impl fmt::Debug for OpenAiSubscriptionAuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiSubscriptionAuthConfig")
            .field("token_endpoint", &self.token_endpoint)
            .field("client_id", &self.client_id)
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("refresh_token", &"[REDACTED]")
            .field(
                "account_id",
                &self.account_id.as_ref().map(|_| "[REDACTED]"),
            )
            .field("originator", &self.originator)
            .field("user_agent", &self.user_agent)
            .field("session_id", &self.session_id)
            .finish()
    }
}

#[derive(Clone, Debug)]
enum TokenExpiry {
    Unix(u64),
    Monotonic(tokio::time::Instant),
}

#[derive(Clone, Debug)]
struct CachedAccess {
    value: Secret,
    expiry: TokenExpiry,
}

#[derive(Debug)]
struct SubscriptionState {
    access: Option<CachedAccess>,
    refresh: Secret,
    account_id: Option<Secret>,
}

/// Deduplicated `ChatGPT` subscription bearer refresh source.
pub struct OpenAiSubscriptionAuth {
    config: OpenAiSubscriptionAuthConfig,
    client: reqwest::Client,
    sink: Arc<dyn OpenAiSubscriptionTokenSink>,
    registrar: Arc<dyn KnownSecretRegistrar>,
    state: Mutex<SubscriptionState>,
}

impl fmt::Debug for OpenAiSubscriptionAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiSubscriptionAuth")
            .field("token_endpoint", &self.config.token_endpoint)
            .field("client_id", &self.config.client_id)
            .field("originator", &self.config.originator)
            .field("user_agent", &self.config.user_agent)
            .field("session_id", &self.config.session_id)
            .finish_non_exhaustive()
    }
}

impl OpenAiSubscriptionAuth {
    /// Builds the refresh source with explicit proxy handling and no ambient proxy path.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe token endpoint or invalid proxy client.
    pub fn with_proxy(
        config: OpenAiSubscriptionAuthConfig,
        proxy: Option<&Url>,
        proxy_authentication: Option<&ProxyAuthentication>,
        sink: Arc<dyn OpenAiSubscriptionTokenSink>,
        registrar: Arc<dyn KnownSecretRegistrar>,
    ) -> Result<Self, ProviderError> {
        validate_token_endpoint(&config.token_endpoint)?;
        if config.client_id.trim().is_empty()
            || config.originator.trim().is_empty()
            || config.user_agent.trim().is_empty()
            || config.session_id.trim().is_empty()
        {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "ChatGPT subscription client and request identity must not be empty",
            ));
        }
        for (name, value) in [
            ("originator", config.originator.as_str()),
            ("user agent", config.user_agent.as_str()),
            ("session id", config.session_id.as_str()),
        ] {
            validate_identity_value(name, value)?;
        }
        if let Some(account_id) = &config.account_id {
            validate_account_id(account_id.expose_secret())?;
        }
        registrar.register(&config.refresh_token);
        if let Some(access) = &config.access_token {
            registrar.register(access);
        }
        if let Some(account_id) = &config.account_id {
            registrar.register(account_id);
        }
        let access = config.access_token.as_ref().and_then(|value| {
            jwt_expiry(value.expose_secret()).map(|expiry| CachedAccess {
                value: value.clone(),
                expiry: TokenExpiry::Unix(expiry),
            })
        });
        let state = SubscriptionState {
            access,
            refresh: config.refresh_token.clone(),
            account_id: config.account_id.clone(),
        };
        let client = crate::http::build_client_with_proxy_auth(proxy, proxy_authentication)?;
        Ok(Self {
            config,
            client,
            sink,
            registrar,
            state: Mutex::new(state),
        })
    }
}

#[derive(Deserialize)]
struct SubscriptionTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    #[serde(default = "default_expiry")]
    expires_in: u64,
    token_type: Option<String>,
}

const fn default_expiry() -> u64 {
    3600
}

#[async_trait]
impl AuthProvider for OpenAiSubscriptionAuth {
    async fn material(&self) -> Result<AuthMaterial, ProviderError> {
        let mut state = self.state.lock().await;
        if let (Some(access), Some(account_id)) = (&state.access, &state.account_id)
            && token_is_fresh(access)
        {
            return Ok(subscription_material(
                &self.config,
                &access.value,
                account_id,
            ));
        }

        let token = self.request_refresh(&state.refresh).await?;
        self.install_refresh(&mut state, token).await
    }
}

impl OpenAiSubscriptionAuth {
    async fn request_refresh(
        &self,
        refresh_token: &Secret,
    ) -> Result<SubscriptionTokenResponse, ProviderError> {
        let form = [
            ("grant_type", "refresh_token".to_owned()),
            ("refresh_token", refresh_token.expose_secret().to_owned()),
            ("client_id", self.config.client_id.clone()),
        ];
        crate::http::require_process_network()?;
        let response = self
            .client
            .post(self.config.token_endpoint.clone())
            .form(&form)
            .send()
            .await
            .map_err(|error| {
                ProviderError::new(
                    if error.is_timeout() {
                        ProviderErrorKind::Timeout
                    } else {
                        ProviderErrorKind::Network
                    },
                    "ChatGPT subscription token refresh failed",
                )
            })?;
        if !response.status().is_success() {
            return Err(ProviderError::new(
                if matches!(response.status().as_u16(), 400 | 401 | 403) {
                    ProviderErrorKind::Authentication
                } else if response.status().is_server_error() {
                    ProviderErrorKind::Server
                } else {
                    ProviderErrorKind::InvalidRequest
                },
                format!(
                    "ChatGPT subscription token endpoint returned HTTP {}",
                    response.status()
                ),
            ));
        }
        let token: SubscriptionTokenResponse = response.json().await.map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                "ChatGPT subscription token endpoint returned invalid JSON",
            )
        })?;
        if token.access_token.is_empty()
            || token
                .token_type
                .as_deref()
                .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
        {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "ChatGPT subscription refresh did not return a bearer access token",
            ));
        }

        Ok(token)
    }

    async fn install_refresh(
        &self,
        state: &mut SubscriptionState,
        token: SubscriptionTokenResponse,
    ) -> Result<AuthMaterial, ProviderError> {
        let access = Secret::new(token.access_token);
        let id_token = token.id_token.map(Secret::new);
        let rotated = token.refresh_token.map(Secret::new);
        let derived_account_id = id_token
            .as_ref()
            .and_then(|token| extract_openai_subscription_account_id(token.expose_secret()))
            .or_else(|| extract_openai_subscription_account_id(access.expose_secret()))
            .map(Secret::new);
        if let (Some(previous), Some(derived)) = (&state.account_id, &derived_account_id)
            && previous.expose_secret() != derived.expose_secret()
        {
            return Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                "refreshed ChatGPT subscription account id changed unexpectedly",
            ));
        }
        let account_id = derived_account_id
            .or_else(|| state.account_id.clone())
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "ChatGPT subscription token omitted its account identifier",
                )
            })?;
        validate_account_id(account_id.expose_secret())?;

        self.sink
            .persist(&access, rotated.as_ref(), &account_id)
            .await
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "could not persist refreshed ChatGPT subscription credentials",
                )
            })?;
        self.registrar.register(&access);
        self.registrar.register(&account_id);
        if let Some(id_token) = &id_token {
            self.registrar.register(id_token);
        }
        if let Some(rotated) = &rotated {
            self.registrar.register(rotated);
            state.refresh = rotated.clone();
        }
        state.account_id = Some(account_id.clone());
        state.access = Some(CachedAccess {
            value: access.clone(),
            expiry: TokenExpiry::Monotonic(
                tokio::time::Instant::now() + Duration::from_secs(token.expires_in),
            ),
        });
        Ok(subscription_material(&self.config, &access, &account_id))
    }
}

fn subscription_material(
    config: &OpenAiSubscriptionAuthConfig,
    access_token: &Secret,
    account_id: &Secret,
) -> AuthMaterial {
    AuthMaterial::OpenAiSubscription {
        access_token: access_token.clone(),
        account_id: account_id.clone(),
        originator: config.originator.clone(),
        user_agent: config.user_agent.clone(),
        session_id: config.session_id.clone(),
    }
}

fn token_is_fresh(access: &CachedAccess) -> bool {
    match access.expiry {
        TokenExpiry::Unix(expiry) => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .is_some_and(|now| expiry > now.as_secs().saturating_add(30)),
        TokenExpiry::Monotonic(expiry) => {
            expiry > tokio::time::Instant::now() + Duration::from_secs(30)
        }
    }
}

fn validate_token_endpoint(endpoint: &Url) -> Result<(), ProviderError> {
    let loopback = match endpoint.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if endpoint.host().is_none()
        || (endpoint.scheme() != "https" && !(endpoint.scheme() == "http" && loopback))
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "ChatGPT subscription token endpoint must use HTTPS (loopback HTTP is test-only)",
        ));
    }
    Ok(())
}

fn validate_identity_value(name: &str, value: &str) -> Result<(), ProviderError> {
    if value.trim().is_empty() || reqwest::header::HeaderValue::from_str(value).is_err() {
        return Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            format!("ChatGPT subscription {name} is invalid"),
        ));
    }
    Ok(())
}

fn validate_account_id(value: &str) -> Result<(), ProviderError> {
    validate_identity_value("account id", value)?;
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        return Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "ChatGPT subscription account id is invalid",
        ));
    }
    Ok(())
}

/// Extracts the `ChatGPT` account id from a trusted ID/access JWT payload.
/// The token is decoded only after TLS/OAuth acquisition and is never logged.
#[must_use]
pub fn extract_openai_subscription_account_id(token: &str) -> Option<String> {
    let payload = jwt_payload(token)?;
    payload
        .get("chatgpt_account_id")
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("chatgpt_account_id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            payload
                .get("organizations")
                .and_then(Value::as_array)
                .and_then(|organizations| organizations.first())
                .and_then(|organization| organization.get("id"))
                .and_then(Value::as_str)
        })
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn jwt_expiry(token: &str) -> Option<u64> {
    jwt_payload(token)?.get("exp")?.as_u64()
}

fn jwt_payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use std::{
        fmt,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use reqwest::header::HeaderMap;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };
    use url::Url;

    use crate::{
        AuthProvider, FixtureRedactor, OpenAiSubscriptionTokenSink, ProviderError, Secret,
        openai_subscription_oauth_flow_with_endpoints,
    };

    use super::{
        OPENAI_SUBSCRIPTION_CLIENT_ID, OPENAI_SUBSCRIPTION_REDIRECT_URI, OpenAiSubscriptionAuth,
        OpenAiSubscriptionAuthConfig, extract_openai_subscription_account_id,
    };

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<(String, Option<String>, String)>>);

    impl fmt::Debug for RecordingSink {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("RecordingSink([REDACTED])")
        }
    }

    #[async_trait]
    impl OpenAiSubscriptionTokenSink for RecordingSink {
        async fn persist(
            &self,
            access_token: &Secret,
            rotated_refresh_token: Option<&Secret>,
            account_id: &Secret,
        ) -> Result<(), ProviderError> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((
                    access_token.expose_secret().to_owned(),
                    rotated_refresh_token.map(|token| token.expose_secret().to_owned()),
                    account_id.expose_secret().to_owned(),
                ));
            Ok(())
        }
    }

    fn jwt(account_id: &str, expiry: u64) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": account_id},
                "exp": expiry,
            }))
            .unwrap_or_else(|error| panic!("JWT fixture must encode: {error}")),
        );
        format!("{header}.{payload}.signature")
    }

    async fn spawn_token_server(body: String) -> (Url, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("token fixture must bind: {error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("token fixture address must resolve: {error}"));
        let endpoint = Url::parse(&format!("http://{address}/oauth/token"))
            .unwrap_or_else(|error| panic!("token URL must parse: {error}"));
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener
                .accept()
                .await
                .unwrap_or_else(|error| panic!("token request must arrive: {error}"));
            let request = read_request(&mut socket).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .unwrap_or_else(|error| panic!("token response must write: {error}"));
            request
        });
        (endpoint, task)
    }

    async fn read_request(socket: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 2048];
        let header_end = loop {
            let read = socket
                .read(&mut chunk)
                .await
                .unwrap_or_else(|error| panic!("request must read: {error}"));
            assert_ne!(read, 0);
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        while bytes.len() < header_end + length {
            let read = socket
                .read(&mut chunk)
                .await
                .unwrap_or_else(|error| panic!("request body must read: {error}"));
            assert_ne!(read, 0);
            bytes.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn browser_flow_uses_fixed_callback_pkce_and_openai_parameters() {
        let account = "acct-browser-fixture";
        let id_token = jwt(account, u64::MAX / 2);
        let response = json!({
            "id_token": id_token,
            "access_token": jwt(account, u64::MAX / 2),
            "refresh_token": "browser-refresh-canary",
            "expires_in": 3600,
            "token_type": "Bearer",
        })
        .to_string();
        let (token_endpoint, token_task) = spawn_token_server(response).await;
        let authorization_endpoint = Url::parse("http://127.0.0.1:9/oauth/authorize")
            .unwrap_or_else(|error| panic!("authorization URL must parse: {error}"));
        let flow = openai_subscription_oauth_flow_with_endpoints(
            authorization_endpoint,
            token_endpoint,
            None,
            None,
        )
        .unwrap_or_else(|error| panic!("subscription OAuth flow must build: {error}"));
        let session = flow
            .begin()
            .await
            .unwrap_or_else(|error| panic!("subscription OAuth must begin: {error}"));
        assert_eq!(
            session.redirect_uri().as_str(),
            OPENAI_SUBSCRIPTION_REDIRECT_URI
        );
        let query = session
            .authorization_url()
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            query.get("client_id").map(AsRef::as_ref),
            Some(OPENAI_SUBSCRIPTION_CLIENT_ID)
        );
        assert_eq!(
            query.get("code_challenge_method").map(AsRef::as_ref),
            Some("S256")
        );
        assert_eq!(
            query.get("id_token_add_organizations").map(AsRef::as_ref),
            Some("true")
        );
        assert_eq!(
            query.get("codex_cli_simplified_flow").map(AsRef::as_ref),
            Some("true")
        );
        assert_eq!(
            query.get("originator").map(AsRef::as_ref),
            Some("rottweiler")
        );
        let state = query
            .get("state")
            .unwrap_or_else(|| panic!("state must exist"))
            .to_string();
        let callback = tokio::spawn(async move {
            let mut socket = TcpStream::connect("127.0.0.1:1455")
                .await
                .unwrap_or_else(|error| panic!("callback must connect: {error}"));
            let request = format!(
                "GET /auth/callback?code=fixture-code&state={state} HTTP/1.1\r\nHost: localhost:1455\r\nConnection: close\r\n\r\n"
            );
            socket
                .write_all(request.as_bytes())
                .await
                .unwrap_or_else(|error| panic!("callback must write: {error}"));
        });
        let tokens = session
            .complete()
            .await
            .unwrap_or_else(|error| panic!("subscription OAuth must complete: {error}"));
        callback
            .await
            .unwrap_or_else(|error| panic!("callback task must join: {error}"));
        assert_eq!(
            tokens
                .id_token()
                .and_then(|token| extract_openai_subscription_account_id(token.expose_secret())),
            Some(account.to_owned())
        );
        let request = token_task
            .await
            .unwrap_or_else(|error| panic!("token task must join: {error}"));
        assert!(request.contains("grant_type=authorization_code"));
        assert!(request.contains("code_verifier="));
        assert!(request.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
    }

    #[tokio::test]
    async fn refresh_is_deduplicated_and_material_has_account_headers_without_leaks() {
        let account = "acct-refresh-fixture";
        let access = jwt(account, u64::MAX / 2);
        let id_token = jwt(account, u64::MAX / 2);
        let response = json!({
            "id_token": id_token,
            "access_token": access,
            "refresh_token": "rotated-refresh-canary",
            "expires_in": 3600,
            "token_type": "Bearer",
        })
        .to_string();
        let (token_endpoint, token_task) = spawn_token_server(response).await;
        let sink = Arc::new(RecordingSink::default());
        let redactor = FixtureRedactor::default();
        let auth = Arc::new(
            OpenAiSubscriptionAuth::with_proxy(
                OpenAiSubscriptionAuthConfig {
                    token_endpoint,
                    client_id: OPENAI_SUBSCRIPTION_CLIENT_ID.to_owned(),
                    access_token: None,
                    refresh_token: Secret::new("initial-refresh-canary"),
                    account_id: Some(Secret::new(account)),
                    originator: "rottweiler".to_owned(),
                    user_agent: "rottweiler/test".to_owned(),
                    session_id: "rw-session-fixture".to_owned(),
                },
                None,
                None,
                sink.clone(),
                Arc::new(redactor.clone()),
            )
            .unwrap_or_else(|error| panic!("subscription auth must build: {error}")),
        );
        let (first, second) = tokio::join!(auth.material(), auth.material());
        let first = first.unwrap_or_else(|error| panic!("first refresh must work: {error}"));
        second.unwrap_or_else(|error| panic!("deduplicated refresh must work: {error}"));
        let mut headers = HeaderMap::new();
        first
            .apply_openai(&mut headers)
            .unwrap_or_else(|error| panic!("subscription headers must apply: {error}"));
        assert_eq!(headers["chatgpt-account-id"], account);
        assert_eq!(headers["originator"], "rottweiler");
        assert_eq!(headers["user-agent"], "rottweiler/test");
        assert_eq!(headers["session-id"], "rw-session-fixture");
        assert_eq!(
            sink.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
        token_task
            .await
            .unwrap_or_else(|error| panic!("token task must join: {error}"));
        for secret in [
            "initial-refresh-canary".to_owned(),
            "rotated-refresh-canary".to_owned(),
            account.to_owned(),
            jwt(account, u64::MAX / 2),
        ] {
            assert!(redactor.contains_registered_secret(&secret));
        }
        let debug = format!("{auth:?} {redactor:?}");
        for secret in ["initial-refresh-canary", "rotated-refresh-canary", account] {
            assert!(!debug.contains(secret));
        }
    }
}
