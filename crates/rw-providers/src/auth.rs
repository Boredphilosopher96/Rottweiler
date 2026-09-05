use std::{fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};
use url::{Host, Url};

use crate::{ProviderError, ProviderErrorKind};

/// A secret whose debug representation never exposes its contents.
#[derive(Clone, Eq, PartialEq)]
pub struct Secret(Arc<str>);

impl Secret {
    /// Wraps a credential value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(Arc::from(value.into()))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// Explicitly exposes the value at a credential-storage or authenticated
    /// request boundary. Callers must never log the returned string.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        self.expose()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret([REDACTED])")
    }
}

/// Credentials sent to an HTTP proxy without embedding them in its URL.
///
/// Debug output deliberately redacts both fields: proxy usernames are often
/// account identifiers, while the password is held by [`Secret`].
#[derive(Clone, Eq, PartialEq)]
pub struct ProxyAuthentication {
    username: String,
    password: Secret,
}

impl ProxyAuthentication {
    /// Creates credentials for HTTP Basic proxy authentication.
    #[must_use]
    pub fn new(username: impl Into<String>, password: Secret) -> Self {
        Self {
            username: username.into(),
            password,
        }
    }

    pub(crate) fn username(&self) -> &str {
        &self.username
    }

    pub(crate) fn password(&self) -> &str {
        self.password.expose()
    }
}

impl fmt::Debug for ProxyAuthentication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyAuthentication")
            .field("username", &"[REDACTED]")
            .field("password", &"[REDACTED]")
            .finish()
    }
}

/// Authentication material returned immediately before a request is sent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthMaterial {
    /// Provider-specific API key header.
    ApiKey(Secret),
    /// OAuth or API bearer token.
    Bearer(Secret),
    /// Provider-specific primary credential header.
    Header {
        /// Validated HTTP header name.
        name: String,
        /// Non-secret prefix prepended to the credential.
        value_prefix: String,
        /// Credential value.
        secret: Secret,
    },
    /// `ChatGPT` subscription bearer plus required Codex backend identity headers.
    OpenAiSubscription {
        /// OAuth access token.
        access_token: Secret,
        /// `ChatGPT` workspace/account identifier derived from token claims.
        account_id: Secret,
        /// Client origin classification.
        originator: String,
        /// Versioned client identifier.
        user_agent: String,
        /// Stable Rottweiler provider-session identifier.
        session_id: String,
    },
    /// GitHub Copilot subscription bearer and integration identity headers.
    GitHubCopilot {
        /// GitHub OAuth device-flow token used directly by Copilot inference.
        access_token: Secret,
        /// Versioned Rottweiler client identity.
        user_agent: String,
        /// Whether this request follows user input or agent-generated work.
        initiator: String,
        /// Whether the request contains image input.
        vision: bool,
        /// Copilot GPT shims reject both `OpenAI` max-output fields. The wrapper
        /// enforces the discovered limit before setting this wire profile.
        omit_max_output_tokens: bool,
    },
    /// No authentication, useful for trusted local model servers.
    None,
}

impl AuthMaterial {
    pub(crate) const fn omit_max_output_tokens(&self) -> bool {
        matches!(
            self,
            Self::GitHubCopilot {
                omit_max_output_tokens: true,
                ..
            }
        )
    }

    pub(crate) fn openai_subscription_session_id(&self) -> Option<&str> {
        match self {
            Self::OpenAiSubscription { session_id, .. } => Some(session_id),
            Self::ApiKey(_)
            | Self::Bearer(_)
            | Self::Header { .. }
            | Self::GitHubCopilot { .. }
            | Self::None => None,
        }
    }

    pub(crate) fn apply_openai(&self, headers: &mut HeaderMap) -> Result<(), ProviderError> {
        match self {
            Self::ApiKey(secret) | Self::Bearer(secret) => {
                insert_sensitive(
                    headers,
                    AUTHORIZATION,
                    &format!("Bearer {}", secret.expose()),
                )?;
            }
            Self::Header {
                name,
                value_prefix,
                secret,
            } => {
                let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                    ProviderError::new(
                        ProviderErrorKind::InvalidRequest,
                        "configured primary credential header name is invalid",
                    )
                })?;
                insert_sensitive(headers, name, &format!("{value_prefix}{}", secret.expose()))?;
            }
            Self::OpenAiSubscription {
                access_token,
                account_id,
                originator,
                user_agent,
                session_id,
            } => {
                insert_sensitive(
                    headers,
                    AUTHORIZATION,
                    &format!("Bearer {}", access_token.expose()),
                )?;
                insert_sensitive(
                    headers,
                    HeaderName::from_static("chatgpt-account-id"),
                    account_id.expose(),
                )?;
                insert_header(headers, HeaderName::from_static("originator"), originator)?;
                insert_header(headers, HeaderName::from_static("user-agent"), user_agent)?;
                insert_header(headers, HeaderName::from_static("session-id"), session_id)?;
            }
            Self::GitHubCopilot {
                access_token,
                user_agent,
                initiator,
                vision,
                omit_max_output_tokens: _,
            } => apply_github_copilot(headers, access_token, user_agent, initiator, *vision)?,
            Self::None => {}
        }
        Ok(())
    }

    pub(crate) fn apply_anthropic(&self, headers: &mut HeaderMap) -> Result<(), ProviderError> {
        match self {
            Self::ApiKey(secret) => {
                insert_sensitive(
                    headers,
                    HeaderName::from_static("x-api-key"),
                    secret.expose(),
                )?;
            }
            Self::Bearer(secret) => {
                insert_sensitive(
                    headers,
                    AUTHORIZATION,
                    &format!("Bearer {}", secret.expose()),
                )?;
            }
            Self::Header {
                name,
                value_prefix,
                secret,
            } => {
                let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                    ProviderError::new(
                        ProviderErrorKind::InvalidRequest,
                        "configured primary credential header name is invalid",
                    )
                })?;
                insert_sensitive(headers, name, &format!("{value_prefix}{}", secret.expose()))?;
            }
            Self::OpenAiSubscription { .. } => {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "ChatGPT subscription authentication is OpenAI Responses-only",
                ));
            }
            Self::GitHubCopilot {
                access_token,
                user_agent,
                initiator,
                vision,
                omit_max_output_tokens: _,
            } => {
                apply_github_copilot(headers, access_token, user_agent, initiator, *vision)?;
                insert_header(
                    headers,
                    HeaderName::from_static("anthropic-beta"),
                    "interleaved-thinking-2025-05-14",
                )?;
            }
            Self::None => {}
        }
        Ok(())
    }
}

fn apply_github_copilot(
    headers: &mut HeaderMap,
    access_token: &Secret,
    user_agent: &str,
    initiator: &str,
    vision: bool,
) -> Result<(), ProviderError> {
    if !matches!(initiator, "user" | "agent") {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "GitHub Copilot request initiator must be user or agent",
        ));
    }
    insert_sensitive(
        headers,
        AUTHORIZATION,
        &format!("Bearer {}", access_token.expose()),
    )?;
    insert_header(headers, HeaderName::from_static("user-agent"), user_agent)?;
    insert_header(
        headers,
        HeaderName::from_static("x-github-api-version"),
        crate::GITHUB_COPILOT_API_VERSION,
    )?;
    insert_header(
        headers,
        HeaderName::from_static("openai-intent"),
        "conversation-edits",
    )?;
    insert_header(headers, HeaderName::from_static("x-initiator"), initiator)?;
    if vision {
        insert_header(
            headers,
            HeaderName::from_static("copilot-vision-request"),
            "true",
        )?;
    }
    // Deliberately never emit x-api-key: Copilot accepts the GitHub bearer.
    headers.remove(HeaderName::from_static("x-api-key"));
    Ok(())
}

pub(crate) fn insert_header(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: &str,
) -> Result<(), ProviderError> {
    let header = HeaderValue::from_str(value).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "provider header contains bytes that cannot be sent",
        )
    })?;
    headers.insert(name, header);
    Ok(())
}

pub(crate) fn insert_sensitive(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: &str,
) -> Result<(), ProviderError> {
    let mut header = HeaderValue::from_str(value).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Authentication,
            "credential contains bytes that cannot be sent in an HTTP header",
        )
    })?;
    header.set_sensitive(true);
    headers.insert(name, header);
    Ok(())
}

/// Resolves current authentication material, including refreshable OAuth.
#[async_trait]
pub trait AuthProvider: Send + Sync + fmt::Debug {
    /// Returns a credential immediately before an API request.
    async fn material(&self) -> Result<AuthMaterial, ProviderError>;
}

/// A static API key, bearer token, or unauthenticated local endpoint.
#[derive(Clone, Debug)]
pub struct StaticAuth(AuthMaterial);

impl StaticAuth {
    /// Creates static authentication.
    #[must_use]
    pub const fn new(material: AuthMaterial) -> Self {
        Self(material)
    }
}

#[async_trait]
impl AuthProvider for StaticAuth {
    async fn material(&self) -> Result<AuthMaterial, ProviderError> {
        Ok(self.0.clone())
    }
}

/// Default time allowed for a browser to return to the loopback callback.
pub const DEFAULT_OAUTH_CALLBACK_TIMEOUT: Duration = Duration::from_mins(3);
const RESERVED_AUTHORIZATION_PARAMETERS: [&str; 7] = [
    "response_type",
    "client_id",
    "redirect_uri",
    "scope",
    "state",
    "code_challenge",
    "code_challenge_method",
];
const RESERVED_TOKEN_PARAMETERS: [&str; 5] = [
    "grant_type",
    "code",
    "redirect_uri",
    "client_id",
    "code_verifier",
];

/// Injectable cryptographic entropy boundary for OAuth state and PKCE.
pub trait OAuthEntropy: Send + Sync + fmt::Debug {
    /// Fills the destination with cryptographically secure random bytes.
    ///
    /// # Errors
    ///
    /// Returns a sanitized provider error when secure randomness is unavailable.
    fn fill(&self, destination: &mut [u8]) -> Result<(), ProviderError>;
}

/// Operating-system cryptographic entropy.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemOAuthEntropy;

impl OAuthEntropy for SystemOAuthEntropy {
    fn fill(&self, destination: &mut [u8]) -> Result<(), ProviderError> {
        getrandom::fill(destination).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "operating-system randomness is unavailable for OAuth login",
            )
        })
    }
}

/// User-configured Authorization Code + PKCE settings for a public native client.
#[derive(Clone, Debug)]
pub struct OAuthAuthorizationCodeConfig {
    /// Provider-documented browser authorization endpoint.
    pub authorization_endpoint: Url,
    /// Provider-documented authorization-code token endpoint.
    pub token_endpoint: Url,
    /// Public native-application client identifier.
    pub client_id: String,
    /// Requested OAuth scopes. They are encoded as one space-delimited value.
    pub scopes: Vec<String>,
    /// Maximum time to wait for the loopback browser redirect.
    pub callback_timeout: Duration,
}

/// Standards-based OAuth Authorization Code + PKCE flow.
pub struct OAuthAuthorizationCode {
    config: OAuthAuthorizationCodeConfig,
    client: reqwest::Client,
    entropy: Arc<dyn OAuthEntropy>,
    extra_authorization_parameters: Vec<(String, String)>,
    extra_token_parameters: Vec<(String, String)>,
    loopback_redirect: OAuthLoopbackRedirect,
}

#[derive(Clone, Debug)]
enum OAuthLoopbackRedirect {
    EphemeralIpLiteral,
    FixedLocalhost { port: u16, path: &'static str },
}

impl fmt::Debug for OAuthAuthorizationCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthAuthorizationCode")
            .field(
                "authorization_endpoint",
                &self.config.authorization_endpoint,
            )
            .field("token_endpoint", &self.config.token_endpoint)
            .field("client_id", &self.config.client_id)
            .field("scopes", &self.config.scopes)
            .field("callback_timeout", &self.config.callback_timeout)
            .finish_non_exhaustive()
    }
}

impl OAuthAuthorizationCode {
    /// Creates a flow with ambient proxy discovery disabled.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be initialized.
    pub fn new(config: OAuthAuthorizationCodeConfig) -> Result<Self, ProviderError> {
        Self::with_proxy(config, None, None)
    }

    /// Creates a flow whose token exchange uses the already-resolved explicit
    /// proxy and optional HTTP Basic proxy credentials.
    ///
    /// # Errors
    ///
    /// Returns an error when the proxy settings cannot initialize a client.
    pub fn with_proxy(
        config: OAuthAuthorizationCodeConfig,
        proxy: Option<&Url>,
        proxy_authentication: Option<&ProxyAuthentication>,
    ) -> Result<Self, ProviderError> {
        let client = crate::http::build_client_with_proxy_auth(proxy, proxy_authentication)?;
        Ok(Self::with_client_and_entropy(
            config,
            client,
            Arc::new(SystemOAuthEntropy),
        ))
    }

    /// Creates a flow with injected HTTP and entropy boundaries for deterministic tests.
    #[must_use]
    pub fn with_client_and_entropy(
        config: OAuthAuthorizationCodeConfig,
        client: reqwest::Client,
        entropy: Arc<dyn OAuthEntropy>,
    ) -> Self {
        Self {
            config,
            client,
            entropy,
            extra_authorization_parameters: Vec::new(),
            extra_token_parameters: Vec::new(),
            loopback_redirect: OAuthLoopbackRedirect::EphemeralIpLiteral,
        }
    }

    pub(crate) fn with_fixed_localhost_callback(mut self, port: u16, path: &'static str) -> Self {
        self.loopback_redirect = OAuthLoopbackRedirect::FixedLocalhost { port, path };
        self
    }

    /// Adds provider-documented, non-protocol authorization parameters.
    /// Reserved `OAuth`/PKCE fields remain owned by this implementation.
    #[must_use]
    pub fn with_authorization_parameters(
        mut self,
        parameters: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        self.extra_authorization_parameters = parameters.into_iter().collect();
        self
    }

    /// Adds provider-documented parameters to the authorization-code token
    /// exchange. Core protocol fields remain owned by this implementation.
    #[must_use]
    pub fn with_token_parameters(
        mut self,
        parameters: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        self.extra_token_parameters = parameters.into_iter().collect();
        self
    }

    /// Binds an ephemeral IPv4 loopback callback and constructs the browser URL.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe endpoint configuration, unavailable secure
    /// randomness, or a loopback bind failure.
    pub async fn begin(&self) -> Result<OAuthLoginSession, ProviderError> {
        self.validate_begin_configuration()?;
        let (listener, redirect_uri) = self.bind_loopback_callback().await?;
        let (authorization_url, state, verifier) = self.build_authorization_url(&redirect_uri)?;

        Ok(OAuthLoginSession {
            authorization_url,
            redirect_uri,
            listener,
            state: Secret::new(state),
            verifier: Secret::new(verifier),
            token_endpoint: self.config.token_endpoint.clone(),
            client_id: self.config.client_id.clone(),
            client: self.client.clone(),
            callback_timeout: self.config.callback_timeout,
            extra_token_parameters: self.extra_token_parameters.clone(),
        })
    }

    fn validate_begin_configuration(&self) -> Result<(), ProviderError> {
        validate_oauth_endpoint("authorization", &self.config.authorization_endpoint)?;
        validate_oauth_endpoint("token", &self.config.token_endpoint)?;
        if self.config.client_id.trim().is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "OAuth client id must not be empty",
            ));
        }
        if self.config.callback_timeout.is_zero() {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "OAuth callback timeout must be greater than zero",
            ));
        }
        if self
            .config
            .scopes
            .iter()
            .any(|scope| scope.trim().is_empty())
        {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "OAuth scopes must not contain empty values",
            ));
        }
        if self.extra_authorization_parameters.iter().any(|(name, _)| {
            name.trim().is_empty()
                || RESERVED_AUTHORIZATION_PARAMETERS
                    .iter()
                    .any(|reserved| name == reserved)
        }) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "OAuth extra authorization parameters must be non-empty and non-reserved",
            ));
        }
        if self.extra_token_parameters.len() > 32
            || self.extra_token_parameters.iter().any(|(name, value)| {
                name.trim().is_empty()
                    || name.len() > 128
                    || value.is_empty()
                    || value.len() > 4096
                    || name.chars().any(char::is_control)
                    || value.chars().any(char::is_control)
                    || RESERVED_TOKEN_PARAMETERS
                        .iter()
                        .any(|reserved| name == reserved)
            })
        {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "OAuth extra token parameters must be bounded, non-empty, and non-reserved",
            ));
        }
        if self
            .config
            .authorization_endpoint
            .query_pairs()
            .any(|(name, _)| {
                RESERVED_AUTHORIZATION_PARAMETERS
                    .iter()
                    .any(|reserved| name == *reserved)
            })
        {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "OAuth authorization endpoint must not preconfigure protocol parameters",
            ));
        }

        Ok(())
    }

    async fn bind_loopback_callback(&self) -> Result<(TcpListener, Url), ProviderError> {
        let port = match self.loopback_redirect {
            OAuthLoopbackRedirect::EphemeralIpLiteral => 0,
            OAuthLoopbackRedirect::FixedLocalhost { port, .. } => port,
        };
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|error| {
                ProviderError::new(
                    ProviderErrorKind::Network,
                    format!("could not bind OAuth loopback callback: {error}"),
                )
            })?;
        let address = listener.local_addr().map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::Network,
                format!("could not inspect OAuth loopback callback: {error}"),
            )
        })?;
        let redirect_uri = match self.loopback_redirect {
            OAuthLoopbackRedirect::EphemeralIpLiteral => {
                format!("http://127.0.0.1:{}/oauth/callback", address.port())
            }
            OAuthLoopbackRedirect::FixedLocalhost { path, .. } => {
                format!("http://localhost:{}{path}", address.port())
            }
        };
        let redirect_uri = Url::parse(&redirect_uri).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                "could not construct OAuth loopback redirect URI",
            )
        })?;

        Ok((listener, redirect_uri))
    }

    fn build_authorization_url(
        &self,
        redirect_uri: &Url,
    ) -> Result<(Url, String, String), ProviderError> {
        let state = random_base64url(self.entropy.as_ref())?;
        let verifier = random_base64url(self.entropy.as_ref())?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut authorization_url = self.config.authorization_endpoint.clone();
        {
            let mut query = authorization_url.query_pairs_mut();
            query.append_pair("response_type", "code");
            query.append_pair("client_id", &self.config.client_id);
            query.append_pair("redirect_uri", redirect_uri.as_str());
            if !self.config.scopes.is_empty() {
                query.append_pair("scope", &self.config.scopes.join(" "));
            }
            query.append_pair("state", &state);
            query.append_pair("code_challenge", &challenge);
            query.append_pair("code_challenge_method", "S256");
            for (name, value) in &self.extra_authorization_parameters {
                query.append_pair(name, value);
            }
        }

        Ok((authorization_url, state, verifier))
    }
}

/// In-progress login. Its debug representation deliberately omits state and verifier.
pub struct OAuthLoginSession {
    authorization_url: Url,
    redirect_uri: Url,
    listener: TcpListener,
    state: Secret,
    verifier: Secret,
    token_endpoint: Url,
    client_id: String,
    client: reqwest::Client,
    callback_timeout: Duration,
    extra_token_parameters: Vec<(String, String)>,
}

impl fmt::Debug for OAuthLoginSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthLoginSession")
            .field("authorization_url", &"[REDACTED]")
            .field("redirect_uri", &self.redirect_uri)
            .field("token_endpoint", &self.token_endpoint)
            .field("client_id", &self.client_id)
            .field("callback_timeout", &self.callback_timeout)
            .finish_non_exhaustive()
    }
}

impl OAuthLoginSession {
    /// URL to print for the user's external browser.
    #[must_use]
    pub const fn authorization_url(&self) -> &Url {
        &self.authorization_url
    }

    /// Exact ephemeral loopback redirect supplied to the authorization server.
    #[must_use]
    pub const fn redirect_uri(&self) -> &Url {
        &self.redirect_uri
    }

    /// Waits for the callback, validates state, and exchanges the code with PKCE.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error on timeout, callback validation failure, token
    /// transport failure, rejection, or malformed token response.
    pub async fn complete(self) -> Result<OAuthTokenSet, ProviderError> {
        let code = tokio::time::timeout(
            self.callback_timeout,
            receive_callback(&self.listener, &self.redirect_uri, &self.state),
        )
        .await
        .map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Timeout,
                "timed out waiting for the OAuth loopback callback",
            )
        })??;

        let mut form = vec![
            ("grant_type".to_owned(), "authorization_code".to_owned()),
            ("code".to_owned(), code),
            ("redirect_uri".to_owned(), self.redirect_uri.to_string()),
            ("client_id".to_owned(), self.client_id),
            (
                "code_verifier".to_owned(),
                self.verifier.expose().to_owned(),
            ),
        ];
        form.extend(self.extra_token_parameters);
        crate::http::require_process_network()?;
        let response = self
            .client
            .post(self.token_endpoint)
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
                    "OAuth authorization-code exchange failed",
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
                    "OAuth token endpoint rejected the authorization code with HTTP {}",
                    response.status()
                ),
            ));
        }
        let response: AuthorizationCodeTokenResponse = crate::token_response::read_json(
            response,
            "OAuth token endpoint returned invalid JSON",
        )
        .await?;
        if response.access_token.is_empty()
            || response
                .token_type
                .as_deref()
                .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
        {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "OAuth token endpoint did not return a bearer access token",
            ));
        }
        Ok(OAuthTokenSet {
            id_token: response.id_token.map(Secret::new),
            access_token: Secret::new(response.access_token),
            refresh_token: response.refresh_token.map(Secret::new),
            expires_in: response.expires_in,
        })
    }
}

/// Tokens returned by a successful authorization-code exchange.
pub struct OAuthTokenSet {
    id_token: Option<Secret>,
    access_token: Secret,
    refresh_token: Option<Secret>,
    expires_in: Option<u64>,
}

impl fmt::Debug for OAuthTokenSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthTokenSet")
            .field("id_token", &self.id_token.as_ref().map(|_| "[REDACTED]"))
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

impl OAuthTokenSet {
    /// Optional `OpenID` Connect ID token returned by the authorization server.
    #[must_use]
    pub const fn id_token(&self) -> Option<&Secret> {
        self.id_token.as_ref()
    }
    /// Short-lived bearer token, for immediate use and credential persistence.
    #[must_use]
    pub const fn access_token(&self) -> &Secret {
        &self.access_token
    }

    /// Optional long-lived refresh token, for credential persistence.
    #[must_use]
    pub const fn refresh_token(&self) -> Option<&Secret> {
        self.refresh_token.as_ref()
    }

    /// Provider-declared access-token lifetime in seconds.
    #[must_use]
    pub const fn expires_in(&self) -> Option<u64> {
        self.expires_in
    }
}

#[derive(Deserialize)]
struct AuthorizationCodeTokenResponse {
    id_token: Option<String>,
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    token_type: Option<String>,
}

fn random_base64url(entropy: &dyn OAuthEntropy) -> Result<String, ProviderError> {
    let mut bytes = [0_u8; 32];
    entropy.fill(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn validate_oauth_endpoint(kind: &str, endpoint: &Url) -> Result<(), ProviderError> {
    let loopback_host = match endpoint.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(_)) | None => false,
    };
    let loopback_http = endpoint.scheme() == "http" && loopback_host;
    if (endpoint.scheme() != "https" && !loopback_http)
        || endpoint.host().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!(
                "OAuth {kind} endpoint must use HTTPS without credentials or a fragment (loopback HTTP is test-only)"
            ),
        ));
    }
    Ok(())
}

async fn receive_callback(
    listener: &TcpListener,
    redirect_uri: &Url,
    expected_state: &Secret,
) -> Result<String, ProviderError> {
    let (mut stream, peer) = listener.accept().await.map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Network,
            format!("OAuth loopback callback failed: {error}"),
        )
    })?;
    if !peer.ip().is_loopback() {
        return Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "OAuth callback did not originate from loopback",
        ));
    }
    let target = read_callback_target(&mut stream, redirect_uri).await?;
    let result = validate_callback_target(&target, redirect_uri, expected_state);
    let success = result.is_ok();
    send_callback_response(&mut stream, success).await;
    result
}

async fn read_callback_target(
    stream: &mut TcpStream,
    redirect_uri: &Url,
) -> Result<String, ProviderError> {
    const MAX_CALLBACK_REQUEST_BYTES: usize = 16 * 1024;
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n")
        && !request.windows(2).any(|window| window == b"\n\n")
    {
        let read = stream.read(&mut chunk).await.map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::Network,
                format!("could not read OAuth loopback callback: {error}"),
            )
        })?;
        if read == 0 {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "OAuth loopback callback closed before sending a request",
            ));
        }
        if request.len().saturating_add(read) > MAX_CALLBACK_REQUEST_BYTES {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "OAuth loopback callback exceeded the request-size limit",
            ));
        }
        request.extend_from_slice(&chunk[..read]);
    }
    let text = std::str::from_utf8(&request).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            "OAuth loopback callback was not valid HTTP",
        )
    })?;
    let request_line = text.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next();
    let target = parts.next();
    let version = parts.next();
    if method != Some("GET") || version != Some("HTTP/1.1") || parts.next().is_some() {
        return Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            "OAuth loopback callback was not a valid GET request",
        ));
    }
    let target = target.map(str::to_owned).ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            "OAuth loopback callback omitted its request target",
        )
    })?;
    let mut hosts = text.lines().skip(1).filter_map(|line| {
        line.split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.trim())
    });
    let expected_host = format!(
        "{}:{}",
        redirect_uri.host_str().unwrap_or_default(),
        redirect_uri.port().unwrap_or_default()
    );
    if hosts.next() != Some(expected_host.as_str()) || hosts.next().is_some() {
        return Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "OAuth callback Host did not match the registered loopback redirect",
        ));
    }
    Ok(target)
}

fn validate_callback_target(
    target: &str,
    redirect_uri: &Url,
    expected_state: &Secret,
) -> Result<String, ProviderError> {
    if !target.starts_with('/') {
        return Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "OAuth callback target did not match the registered loopback redirect",
        ));
    }
    let callback = redirect_uri.join(target).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            "OAuth callback target was malformed",
        )
    })?;
    if callback.scheme() != redirect_uri.scheme()
        || callback.host() != redirect_uri.host()
        || callback.port_or_known_default() != redirect_uri.port_or_known_default()
        || callback.path() != redirect_uri.path()
        || callback.fragment().is_some()
    {
        return Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "OAuth callback target did not match the registered loopback redirect",
        ));
    }
    let state = unique_query_value(&callback, "state")?;
    if state.as_deref() != Some(expected_state.expose()) {
        return Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "OAuth callback state validation failed",
        ));
    }
    if unique_query_value(&callback, "error")?.is_some() {
        return Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "OAuth authorization server rejected the login",
        ));
    }
    unique_query_value(&callback, "code")?
        .filter(|code| !code.is_empty())
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "OAuth callback did not contain an authorization code",
            )
        })
}

fn unique_query_value(url: &Url, name: &str) -> Result<Option<String>, ProviderError> {
    let mut values = url
        .query_pairs()
        .filter_map(|(key, value)| (key == name).then(|| value.into_owned()));
    let value = values.next();
    if values.next().is_some() {
        return Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            format!("OAuth callback contained duplicate {name} parameters"),
        ));
    }
    Ok(value)
}

async fn send_callback_response(stream: &mut TcpStream, success: bool) {
    let (status, body) = if success {
        (
            "200 OK",
            "Authorization received. You can close this window and return to Rottweiler.\n",
        )
    } else {
        (
            "400 Bad Request",
            "Authorization could not be validated. Return to Rottweiler for details.\n",
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// OAuth refresh-token flow settings for providers that expose a token endpoint.
#[derive(Clone, Debug)]
pub struct OAuthRefreshConfig {
    /// OAuth token endpoint.
    pub token_endpoint: Url,
    /// Public client identifier.
    pub client_id: String,
    /// Optional confidential-client secret.
    pub client_secret: Option<Secret>,
    /// Long-lived refresh token.
    pub refresh_token: Secret,
    /// Optional space-delimited scope.
    pub scope: Option<String>,
}

/// Persistence boundary for provider-issued refresh-token rotations.
///
/// Implementations may complete synchronously inside this async method (for
/// example, an OS credential-store API) or await an async secret store. Diagnostics
/// returned from this boundary must never contain the token value.
#[async_trait]
pub trait RefreshTokenSink: Send + Sync + fmt::Debug {
    /// Durably replaces the stored refresh token.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error when the rotated token cannot be persisted.
    async fn persist(&self, refresh_token: &Secret) -> Result<(), ProviderError>;
}

/// Synchronous boundary for registering credentials with export/fixture redaction.
///
/// Implementations must retain no diagnostic representation of the supplied
/// value. Registration is synchronous so a token cannot be returned or used in
/// a provider request before the redaction boundary knows it.
pub trait KnownSecretRegistrar: Send + Sync + fmt::Debug {
    /// Registers one credential for all subsequent redaction operations.
    fn register(&self, secret: &Secret);
}

#[derive(Clone, Debug)]
struct CachedToken {
    value: Secret,
    expires_at: tokio::time::Instant,
}

#[derive(Clone, Debug)]
struct RefreshState {
    refresh_token: Secret,
    cached: Option<CachedToken>,
}

/// OAuth credential source that refreshes and caches bearer tokens.
#[derive(Debug)]
pub struct RefreshingOAuth {
    config: OAuthRefreshConfig,
    client: reqwest::Client,
    refresh_token_sink: Option<Arc<dyn RefreshTokenSink>>,
    secret_registrar: Option<Arc<dyn KnownSecretRegistrar>>,
    state: Mutex<RefreshState>,
}

impl RefreshingOAuth {
    /// Creates a refresh-token flow using the supplied HTTP client.
    #[must_use]
    pub fn new(config: OAuthRefreshConfig, client: reqwest::Client) -> Self {
        Self::new_inner(config, client, None)
    }

    /// Creates a refresh-token flow that durably handles provider token rotation.
    #[must_use]
    pub fn new_with_sink(
        config: OAuthRefreshConfig,
        client: reqwest::Client,
        refresh_token_sink: Arc<dyn RefreshTokenSink>,
    ) -> Self {
        Self::new_inner(config, client, Some(refresh_token_sink))
    }

    fn new_inner(
        config: OAuthRefreshConfig,
        client: reqwest::Client,
        refresh_token_sink: Option<Arc<dyn RefreshTokenSink>>,
    ) -> Self {
        let refresh_token = config.refresh_token.clone();
        Self {
            config,
            client,
            refresh_token_sink,
            secret_registrar: None,
            state: Mutex::new(RefreshState {
                refresh_token,
                cached: None,
            }),
        }
    }

    /// Registers initial and subsequently issued OAuth credentials with a
    /// shared redaction boundary before they can be used or returned.
    #[must_use]
    pub fn with_secret_registrar(mut self, registrar: Arc<dyn KnownSecretRegistrar>) -> Self {
        registrar.register(&self.config.refresh_token);
        if let Some(client_secret) = &self.config.client_secret {
            registrar.register(client_secret);
        }
        self.secret_registrar = Some(registrar);
        self
    }

    /// Creates a refresh-token source using the same explicit proxy policy as
    /// provider API calls.
    ///
    /// # Errors
    ///
    /// Returns an error when the proxy URL or credentials cannot initialize
    /// the HTTP client.
    pub fn with_proxy(
        config: OAuthRefreshConfig,
        proxy: Option<&Url>,
        proxy_authentication: Option<&ProxyAuthentication>,
    ) -> Result<Self, ProviderError> {
        let client = crate::http::build_client_with_proxy_auth(proxy, proxy_authentication)?;
        Ok(Self::new(config, client))
    }

    /// Creates a proxied refresh-token source with durable rotation storage.
    ///
    /// # Errors
    ///
    /// Returns an error when the proxy URL or credentials cannot initialize
    /// the HTTP client.
    pub fn with_proxy_and_sink(
        config: OAuthRefreshConfig,
        proxy: Option<&Url>,
        proxy_authentication: Option<&ProxyAuthentication>,
        refresh_token_sink: Arc<dyn RefreshTokenSink>,
    ) -> Result<Self, ProviderError> {
        let client = crate::http::build_client_with_proxy_auth(proxy, proxy_authentication)?;
        Ok(Self::new_with_sink(config, client, refresh_token_sink))
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    #[serde(default = "default_expiry")]
    expires_in: u64,
    token_type: Option<String>,
}

const fn default_expiry() -> u64 {
    3600
}

#[async_trait]
impl AuthProvider for RefreshingOAuth {
    async fn material(&self) -> Result<AuthMaterial, ProviderError> {
        // Serializing refreshes also guarantees a rotated token is durably
        // stored before any concurrent caller can observe its access token.
        let mut state = self.state.lock().await;
        let now = tokio::time::Instant::now();
        if let Some(token) = state.cached.as_ref()
            && token.expires_at > now + Duration::from_secs(30)
        {
            return Ok(AuthMaterial::Bearer(token.value.clone()));
        }

        let mut form = vec![
            ("grant_type", "refresh_token".to_owned()),
            ("refresh_token", state.refresh_token.expose().to_owned()),
            ("client_id", self.config.client_id.clone()),
        ];
        if let Some(secret) = &self.config.client_secret {
            form.push(("client_secret", secret.expose().to_owned()));
        }
        if let Some(scope) = &self.config.scope {
            form.push(("scope", scope.clone()));
        }

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
                    "OAuth token refresh failed",
                )
            })?;
        if !response.status().is_success() {
            return Err(ProviderError::new(
                if response.status().as_u16() == 401 {
                    ProviderErrorKind::Authentication
                } else if response.status().is_server_error() {
                    ProviderErrorKind::Server
                } else {
                    ProviderErrorKind::InvalidRequest
                },
                format!("OAuth token endpoint returned HTTP {}", response.status()),
            ));
        }
        let token: TokenResponse = crate::token_response::read_json(
            response,
            "OAuth token endpoint returned invalid JSON",
        )
        .await?;
        if token.access_token.is_empty()
            || token
                .token_type
                .as_deref()
                .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
        {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "OAuth token endpoint did not return a bearer access token",
            ));
        }
        if let Some(rotated) = token.refresh_token {
            let rotated = Secret::new(rotated);
            let sink = self.refresh_token_sink.as_ref().ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "OAuth provider rotated the refresh token but no credential sink is configured",
                )
            })?;
            sink.persist(&rotated).await.map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "could not persist the rotated OAuth refresh token",
                )
            })?;
            if let Some(registrar) = &self.secret_registrar {
                registrar.register(&rotated);
            }
            state.refresh_token = rotated;
        }
        let value = Secret::new(token.access_token);
        if let Some(registrar) = &self.secret_registrar {
            registrar.register(&value);
        }
        state.cached = Some(CachedToken {
            value: value.clone(),
            expires_at: crate::token_response::checked_deadline(now, token.expires_in)?,
        });
        Ok(AuthMaterial::Bearer(value))
    }
}

#[cfg(test)]
mod tests;
