use super::*;

/// Trusted configuration for one public-client MCP OAuth authorization-code login.
#[derive(Clone, Debug)]
pub struct McpOAuthLoginConfig {
    pub server: McpServerId,
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
    server: McpServerId,
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
    pub server: McpServerId,
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
