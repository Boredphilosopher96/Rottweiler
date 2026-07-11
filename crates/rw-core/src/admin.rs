use std::{
    collections::BTreeMap,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use rw_providers::{
    DEFAULT_OAUTH_CALLBACK_TIMEOUT, DeviceFlowCancellation, GITHUB_COPILOT_DEVICE_CODE_ENDPOINT,
    GitHubCopilotDeviceFlow, GitHubCopilotDeviceSession, GuardedHttpFetchRequest,
    OAuthAuthorizationCode, OAuthAuthorizationCodeConfig, OPENAI_SUBSCRIPTION_TOKEN_ENDPOINT,
    ProxyAuthentication, ProxyEnvironment, ProxySettings, ProxySource, Secret as ProviderSecret,
    default_models_path, guarded_http_fetch, openai_subscription_oauth_flow,
    refresh_models_dev_with_proxy_auth,
};
use rw_store::{
    config::ConfigLoader,
    credentials::{
        CredentialEnvironment, CredentialKeychain, CredentialManager, CredentialReference,
        Secret as StoredSecret,
    },
};
use rw_types::config::{ProviderConfig, UpdateChannel};
use thiserror::Error;
use url::Url;

use crate::copilot_credentials::{GitHubCopilotCredential, github_copilot_credential_id};
use crate::subscription_credentials::{
    OpenAiSubscriptionCredentialBundle, openai_subscription_credential_id,
};

/// Default public model-catalog endpoint used by the administrative facade.
pub const DEFAULT_MODEL_CATALOG_URL: &str = rw_providers::DEFAULT_MODELS_DEV_URL;

/// Compile-time update metadata origin. Ordinary development builds fail
/// closed when it is absent; runtime/project configuration cannot replace it.
pub const EMBEDDED_UPDATE_BASE_URL: Option<&str> = option_env!("ROTTWEILER_UPDATE_BASE_URL");

/// Sanitized failure returned by a headless administrative workflow.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct AdminError {
    message: String,
}

impl AdminError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn from_display(error: impl fmt::Display) -> Self {
        Self::new(error.to_string())
    }
}

/// API-key material owned by the core boundary.
///
/// This type deliberately has no serialization support, and its debug output
/// never exposes the wrapped value.
pub struct ProviderApiKey(StoredSecret<String>);

/// Validates the versioned shape of a stored built-in subscription credential
/// without returning or logging any credential material.
///
/// # Errors
///
/// Returns a sanitized error when the selected built-in bundle is malformed.
pub fn validate_stored_provider_credential(kind: &str, value: &str) -> Result<(), AdminError> {
    match kind {
        "openai_codex" | "openai_subscription" => {
            OpenAiSubscriptionCredentialBundle::parse(value).map(|_| ())
        }
        "github_copilot" => GitHubCopilotCredential::parse(value).map(|_| ()),
        _ => Ok(()),
    }
}

impl ProviderApiKey {
    /// Builds an API key from hidden terminal input, removing only trailing CR
    /// and LF line terminators. Spaces and every other byte remain unchanged.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error when no key remains.
    pub fn from_terminal_input(mut value: String) -> Result<Self, AdminError> {
        while value.ends_with(['\r', '\n']) {
            value.pop();
        }
        if value.is_empty() {
            return Err(AdminError::new("API key must not be empty"));
        }
        Ok(Self(StoredSecret::new(value)))
    }

    /// Exposes the key only at an authenticated provider boundary.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for ProviderApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderApiKey([REDACTED])")
    }
}

/// Provider-neutral result of resolving an API key.
pub struct ResolvedProviderApiKey {
    api_key: ProviderApiKey,
    warnings: Vec<String>,
}

impl ResolvedProviderApiKey {
    /// Resolved key, with environment taking precedence over stored values.
    #[must_use]
    pub const fn api_key(&self) -> &ProviderApiKey {
        &self.api_key
    }

    /// Security or fallback warnings that the active UI must surface.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// Returns the stable keychain identifier used when a provider does not set
/// `api_key_credential` explicitly.
///
/// # Errors
///
/// Returns a sanitized error when the provider name cannot safely form a
/// credential identifier.
pub fn default_provider_api_key_credential_id(provider_name: &str) -> Result<String, AdminError> {
    if provider_name.is_empty()
        || !provider_name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(AdminError::new(
            "provider name must contain only ASCII letters, digits, '.', '-', or '_'",
        ));
    }
    Ok(format!("providers.{provider_name}.api_key"))
}

/// Stores a provider API key in the configured credential identifier or the
/// safe provider-derived default. Only warnings leave the core boundary.
///
/// # Errors
///
/// Returns a sanitized error for configuration discovery, an unknown provider,
/// an unsafe default identifier, or credential storage failure.
pub fn store_provider_api_key(
    provider_name: &str,
    api_key: ProviderApiKey,
) -> Result<Vec<String>, AdminError> {
    let loader = ConfigLoader::from_environment().map_err(AdminError::from_display)?;
    let credentials_path = loader.credentials_path();
    let effective = loader.load().map_err(AdminError::from_display)?;
    let provider = configured_provider(&effective, provider_name)?;
    let manager = CredentialManager::system(credentials_path);
    let mut warnings = config_warnings(&effective);
    let ProviderApiKey(secret) = api_key;
    warnings.extend(store_provider_api_key_with_manager(
        &manager,
        provider_name,
        provider,
        &secret,
    )?);
    Ok(warnings)
}

/// Resolves a provider API key using environment, keychain, then the warned
/// mode-0600 fallback.
///
/// # Errors
///
/// Returns a sanitized error for configuration discovery, an unknown provider,
/// an unsafe default identifier, or missing credential material.
pub fn resolve_provider_api_key(provider_name: &str) -> Result<ResolvedProviderApiKey, AdminError> {
    let loader = ConfigLoader::from_environment().map_err(AdminError::from_display)?;
    let credentials_path = loader.credentials_path();
    let effective = loader.load().map_err(AdminError::from_display)?;
    let provider = configured_provider(&effective, provider_name)?;
    let manager = CredentialManager::system(credentials_path);
    let mut resolved = resolve_provider_api_key_with_manager(&manager, provider_name, provider)?;
    let mut warnings = config_warnings(&effective);
    warnings.append(&mut resolved.warnings);
    resolved.warnings = warnings;
    Ok(resolved)
}

/// Provider-neutral result of refreshing the local model catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCatalogRefresh {
    /// Number of installed model entries.
    pub model_count: usize,
    /// Validated source URL recorded in the catalog.
    pub source_url: String,
    /// Installed catalog path.
    pub path: PathBuf,
    /// Security or fallback warnings that the active UI must surface.
    pub warnings: Vec<String>,
}

/// Loads user configuration, resolves any proxy credential, and atomically
/// refreshes the model catalog.
///
/// # Errors
///
/// Returns a sanitized error when configuration, credential lookup, transport,
/// catalog validation, or installation fails.
pub async fn refresh_model_catalog(
    source: &str,
    output: Option<PathBuf>,
) -> Result<ModelCatalogRefresh, AdminError> {
    let loader = ConfigLoader::from_environment().map_err(AdminError::from_display)?;
    let credentials_path = loader.credentials_path();
    let effective = loader.load().map_err(AdminError::from_display)?;
    let mut warnings = config_warnings(&effective);
    let global = effective
        .config
        .network
        .proxy
        .as_deref()
        .map(Url::parse)
        .transpose()
        .map_err(AdminError::from_display)?;
    let global_configured = global.is_some();
    let proxies = ProxySettings {
        global,
        per_provider: BTreeMap::new(),
        environment: ProxyEnvironment::capture(),
    };
    let output = output
        .map_or_else(default_models_path, Ok)
        .map_err(AdminError::from_display)?;
    let manager = CredentialManager::system(credentials_path);
    let (proxy_authentication, credential_warnings) = if global_configured {
        resolve_proxy_authentication(
            &manager,
            effective.config.network.proxy_username.as_deref(),
            effective
                .config
                .network
                .proxy_password_credential
                .as_deref(),
        )?
    } else {
        (None, Vec::new())
    };
    warnings.extend(credential_warnings);
    let report = refresh_models_dev_with_proxy_auth(
        source,
        &output,
        &proxies,
        proxy_authentication.as_ref(),
    )
    .await
    .map_err(AdminError::from_display)?;
    Ok(ModelCatalogRefresh {
        model_count: report.model_count,
        source_url: report.source_url,
        path: report.path,
        warnings,
    })
}

/// Opaque, proxy-aware client for signed update metadata and artifacts.
/// Proxy credentials never cross the core boundary or appear in debug output.
pub struct UpdateNetworkClient {
    channel: UpdateChannel,
    proxies: ProxySettings,
    proxy_authentication: Option<ProxyAuthentication>,
    warnings: Vec<String>,
}

impl UpdateNetworkClient {
    /// Effective user-scoped update channel.
    #[must_use]
    pub const fn channel(&self) -> UpdateChannel {
        self.channel
    }

    /// Sanitized configuration/credential warnings.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Fetches one exact, bounded response without following redirects.
    /// Ambient proxies are disabled; global/config/environment proxy precedence
    /// is resolved explicitly for each signed target.
    ///
    /// # Errors
    ///
    /// Rejects invalid/private destinations, redirects, non-200 responses,
    /// DNS overflows, timeouts, transport failures, and oversized bodies with
    /// sanitized errors that never include the URL.
    pub async fn fetch(
        &self,
        url: &Url,
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<Vec<u8>, AdminError> {
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || max_bytes == 0
            || max_bytes > 64 * 1024 * 1024
            || timeout.is_zero()
            || timeout > Duration::from_secs(120)
        {
            return Err(AdminError::new("signed update fetch request is invalid"));
        }
        let resolution = self.proxies.resolve_global(url);
        let proxy = resolution.as_ref().map(|value| value.url.clone());
        let proxy_authentication = resolution
            .as_ref()
            .filter(|value| value.source == ProxySource::Global)
            .and(self.proxy_authentication.clone());
        let dns_pin = if proxy.is_some() {
            None
        } else {
            Some(resolve_public_update_address(url).await?)
        };
        let response = guarded_http_fetch(GuardedHttpFetchRequest {
            url: url.clone(),
            headers: vec![(
                "accept".to_owned(),
                "application/json, application/octet-stream".to_owned(),
            )],
            proxy,
            proxy_authentication,
            dns_pin,
            max_bytes,
            timeout,
        })
        .await
        .map_err(|_| AdminError::new("signed update fetch failed"))?;
        if response.status != 200 || response.final_url != *url || response.location.is_some() {
            return Err(AdminError::new(
                "signed update fetch returned an unexpected response",
            ));
        }
        Ok(response.body)
    }
}

/// Loads the effective user update channel and resolves global proxy
/// authentication exactly once for the update process.
///
/// # Errors
///
/// Returns a sanitized config, proxy, or credential error.
pub fn prepare_update_network() -> Result<UpdateNetworkClient, AdminError> {
    let loader = ConfigLoader::from_environment().map_err(AdminError::from_display)?;
    let credentials_path = loader.credentials_path();
    let effective = loader.load().map_err(AdminError::from_display)?;
    let global = effective
        .config
        .network
        .proxy
        .as_deref()
        .map(Url::parse)
        .transpose()
        .map_err(AdminError::from_display)?;
    let global_configured = global.is_some();
    let proxies = ProxySettings {
        global,
        per_provider: BTreeMap::new(),
        environment: ProxyEnvironment::capture(),
    };
    let manager = CredentialManager::system(credentials_path);
    let (proxy_authentication, credential_warnings) = if global_configured {
        resolve_proxy_authentication(
            &manager,
            effective.config.network.proxy_username.as_deref(),
            effective
                .config
                .network
                .proxy_password_credential
                .as_deref(),
        )?
    } else {
        (None, Vec::new())
    };
    let mut warnings = config_warnings(&effective);
    warnings.extend(credential_warnings);
    Ok(UpdateNetworkClient {
        channel: effective.config.updates.channel,
        proxies,
        proxy_authentication,
        warnings,
    })
}

async fn resolve_public_update_address(url: &Url) -> Result<(String, SocketAddr), AdminError> {
    let host = url
        .host_str()
        .ok_or_else(|| AdminError::new("signed update destination has no host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| AdminError::new("signed update destination has no port"))?;
    let mut addresses = tokio::net::lookup_host((host.trim_matches(['[', ']']), port))
        .await
        .map_err(|_| AdminError::new("signed update DNS lookup failed"))?
        .take(17)
        .collect::<Vec<_>>();
    if addresses.len() > 16 {
        return Err(AdminError::new(
            "signed update DNS lookup exceeded its address limit",
        ));
    }
    addresses.sort_unstable();
    addresses.dedup();
    let address = addresses
        .into_iter()
        .find(|address| public_update_ip(address.ip()))
        .ok_or_else(|| AdminError::new("signed update destination is not public"))?;
    Ok((host.to_owned(), address))
}

fn public_update_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => public_update_v4(value),
        IpAddr::V6(value) => public_update_v6(value),
    }
}

fn public_update_v4(address: Ipv4Addr) -> bool {
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

fn public_update_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    if let Some(mapped) = address.to_ipv4_mapped() {
        return public_update_v4(mapped);
    }
    if segments[..6] == [0, 0, 0, 0, 0, 0] {
        return public_update_v4(update_embedded_v4(segments[6], segments[7]));
    }
    if segments[0] == 0x0064 && segments[1] == 0xff9b {
        return segments[2..6] == [0, 0, 0, 0]
            && public_update_v4(update_embedded_v4(segments[6], segments[7]));
    }
    if segments[0] == 0x2002 {
        return public_update_v4(update_embedded_v4(segments[1], segments[2]));
    }
    if matches!(segments[4], 0 | 0x0200) && segments[5] == 0x5efe {
        return public_update_v4(update_embedded_v4(segments[6], segments[7]));
    }
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || address.is_unique_local()
        || address.is_unicast_link_local()
        || (segments[0] == 0x2001 && matches!(segments[1], 0 | 0x0db8)))
}

fn update_embedded_v4(high: u16, low: u16) -> Ipv4Addr {
    let [a, b] = high.to_be_bytes();
    let [c, d] = low.to_be_bytes();
    Ipv4Addr::new(a, b, c, d)
}

/// Opaque, in-progress OAuth browser login owned by the core facade.
///
/// Provider wire types and credential material remain private. A UI only sees
/// the authorization URL, loopback callback address, warnings, and final
/// provider-neutral result.
pub struct OAuthLogin {
    provider_name: String,
    provider: ProviderConfig,
    credential_manager: CredentialManager,
    session: rw_providers::OAuthLoginSession,
    authorization_url: String,
    redirect_uri: String,
    warnings: Vec<String>,
    openai_subscription: bool,
}

impl OAuthLogin {
    /// Browser URL the UI should present or open.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    /// Loopback callback URI on which the core is waiting.
    #[must_use]
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Configuration and credential-fallback warnings discovered before login.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Waits for the callback, validates state, exchanges the code, and stores
    /// the resulting credentials.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error when callback validation, token exchange, or
    /// credential storage fails.
    pub async fn complete(self) -> Result<OAuthLoginResult, AdminError> {
        let tokens = self
            .session
            .complete()
            .await
            .map_err(AdminError::from_display)?;
        let mut warnings = Vec::new();
        if self.openai_subscription {
            let bundle = OpenAiSubscriptionCredentialBundle::from_login(&tokens)?;
            let encoded = bundle.encode()?;
            let stored = self
                .credential_manager
                .store(
                    &CredentialReference::new(openai_subscription_credential_id(
                        &self.provider_name,
                    )),
                    &StoredSecret::new(encoded),
                )
                .map_err(AdminError::from_display)?;
            warnings.extend(stored.warnings().iter().map(ToString::to_string));
            return Ok(OAuthLoginResult {
                provider: self.provider_name,
                refresh_token_stored: true,
                warnings,
            });
        }
        let access_identifier = self
            .provider
            .oauth_access_token_credential
            .unwrap_or_else(|| format!("providers.{}.oauth.access_token", self.provider_name));
        let access = self
            .credential_manager
            .store(
                &CredentialReference::new(access_identifier),
                &StoredSecret::new(tokens.access_token().expose_secret().to_owned()),
            )
            .map_err(AdminError::from_display)?;
        warnings.extend(access.warnings().iter().map(ToString::to_string));

        let refresh_token_stored = if let Some(refresh_token) = tokens.refresh_token() {
            let refresh_identifier = self
                .provider
                .oauth_refresh_token_credential
                .unwrap_or_else(|| format!("providers.{}.oauth.refresh_token", self.provider_name));
            let refresh = self
                .credential_manager
                .store(
                    &CredentialReference::new(refresh_identifier),
                    &StoredSecret::new(refresh_token.expose_secret().to_owned()),
                )
                .map_err(AdminError::from_display)?;
            warnings.extend(refresh.warnings().iter().map(ToString::to_string));
            true
        } else {
            false
        };
        Ok(OAuthLoginResult {
            provider: self.provider_name,
            refresh_token_stored,
            warnings,
        })
    }
}

/// Provider-neutral completion details for an OAuth login.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthLoginResult {
    /// Locally configured provider name.
    pub provider: String,
    /// Whether the provider issued and the core stored a refresh token.
    pub refresh_token_stored: bool,
    /// Credential-storage warnings that the active UI must surface.
    pub warnings: Vec<String>,
}

/// Authentication interaction selected from the configured provider kind.
pub enum ProviderLogin {
    /// Browser-based authorization-code login.
    OAuth(Box<OAuthLogin>),
    /// GitHub device authorization for the isolated Copilot profile.
    GitHubCopilot(Box<GitHubCopilotLogin>),
}

/// Opaque in-progress GitHub Copilot device authorization.
pub struct GitHubCopilotLogin {
    provider_name: String,
    credential_manager: CredentialManager,
    session: GitHubCopilotDeviceSession,
    verification_uri: String,
    user_code: String,
    oauth_client_id: String,
    warnings: Vec<String>,
}

impl GitHubCopilotLogin {
    /// GitHub page where the user enters [`Self::user_code`].
    #[must_use]
    pub fn verification_uri(&self) -> &str {
        &self.verification_uri
    }

    /// Short device code that the UI must show directly to the user.
    #[must_use]
    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    /// Configuration and credential fallback warnings discovered before login.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Polls until GitHub authorizes, denies, expires, or cancellation wins.
    ///
    /// # Errors
    ///
    /// Returns a sanitized device-flow or credential-storage failure.
    pub async fn complete(
        self,
        cancellation: &ProviderLoginCancellation,
    ) -> Result<GitHubCopilotLoginResult, AdminError> {
        let token = self
            .session
            .complete(&cancellation.0)
            .await
            .map_err(AdminError::from_display)?
            .into_secret();
        let warnings = store_github_copilot_token_with_manager(
            &self.credential_manager,
            &self.provider_name,
            &token,
            &self.oauth_client_id,
        )?;
        Ok(GitHubCopilotLoginResult {
            provider: self.provider_name,
            warnings,
        })
    }
}

/// Cooperative cancellation for an in-progress provider login.
#[derive(Clone, Debug, Default)]
pub struct ProviderLoginCancellation(DeviceFlowCancellation);

impl ProviderLoginCancellation {
    /// Cancels any current or future device-flow polling wait.
    pub fn cancel(&self) {
        self.0.cancel();
    }
}

/// Completion details for a stored GitHub Copilot device credential.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubCopilotLoginResult {
    /// Locally configured provider name.
    pub provider: String,
    /// Credential-storage warnings that the active UI must surface.
    pub warnings: Vec<String>,
}

/// Selects the login protocol from the configured provider kind.
///
/// # Errors
///
/// Returns a sanitized configuration, proxy, device-flow, or OAuth error.
pub async fn begin_provider_login(provider_name: &str) -> Result<ProviderLogin, AdminError> {
    let effective = ConfigLoader::from_environment()
        .map_err(AdminError::from_display)?
        .load()
        .map_err(AdminError::from_display)?;
    let provider = configured_provider(&effective, provider_name)?;
    if provider.kind == "github_copilot" {
        begin_github_copilot_login(provider_name)
            .await
            .map(Box::new)
            .map(ProviderLogin::GitHubCopilot)
    } else {
        begin_oauth_login(provider_name)
            .await
            .map(Box::new)
            .map(ProviderLogin::OAuth)
    }
}

/// Starts a configured provider's OAuth Authorization Code + PKCE flow.
///
/// # Errors
///
/// Returns a sanitized error for missing provider settings, unsafe endpoints,
/// proxy credential lookup, client construction, or callback-listener failure.
#[allow(clippy::too_many_lines)]
pub async fn begin_oauth_login(provider_name: &str) -> Result<OAuthLogin, AdminError> {
    let loader = ConfigLoader::from_environment().map_err(AdminError::from_display)?;
    let credentials_path = loader.credentials_path();
    let effective = loader.load().map_err(AdminError::from_display)?;
    let mut warnings = config_warnings(&effective);
    let provider = effective
        .config
        .providers
        .get(provider_name)
        .cloned()
        .ok_or_else(|| {
            AdminError::new(format!(
                "provider {provider_name:?} is not configured at user scope; add [providers.{provider_name}] to the user config"
            ))
        })?;
    let openai_subscription = matches!(
        provider.kind.as_str(),
        "openai_codex" | "openai_subscription"
    );
    let token_endpoint = if openai_subscription {
        Url::parse(OPENAI_SUBSCRIPTION_TOKEN_ENDPOINT).map_err(AdminError::from_display)?
    } else {
        required_url(
            provider_name,
            "oauth_token_endpoint",
            provider.oauth_token_endpoint.as_deref(),
        )?
    };
    let global_proxy = effective
        .config
        .network
        .proxy
        .as_deref()
        .map(Url::parse)
        .transpose()
        .map_err(AdminError::from_display)?;
    let per_provider = provider
        .proxy
        .as_deref()
        .map(Url::parse)
        .transpose()
        .map_err(AdminError::from_display)?
        .map(|proxy| BTreeMap::from([(provider_name.to_owned(), proxy)]))
        .unwrap_or_default();
    let proxies = ProxySettings {
        global: global_proxy,
        per_provider,
        environment: ProxyEnvironment::capture(),
    };
    let resolution = proxies.resolve(provider_name, &token_endpoint);
    let credential_manager = CredentialManager::system(credentials_path);
    let (proxy_authentication, credential_warnings) =
        match resolution.as_ref().map(|value| value.source) {
            Some(ProxySource::Provider) => resolve_proxy_authentication(
                &credential_manager,
                provider.proxy_username.as_deref(),
                provider.proxy_password_credential.as_deref(),
            )?,
            Some(ProxySource::Global) => resolve_proxy_authentication(
                &credential_manager,
                effective.config.network.proxy_username.as_deref(),
                effective
                    .config
                    .network
                    .proxy_password_credential
                    .as_deref(),
            )?,
            Some(ProxySource::Environment) | None => (None, Vec::new()),
        };
    warnings.extend(credential_warnings);
    let flow = if openai_subscription {
        openai_subscription_oauth_flow(
            resolution.as_ref().map(|value| &value.url),
            proxy_authentication.as_ref(),
        )
        .map_err(AdminError::from_display)?
    } else {
        let authorization_endpoint = required_url(
            provider_name,
            "oauth_authorization_endpoint",
            provider.oauth_authorization_endpoint.as_deref(),
        )?;
        let client_id = provider.oauth_client_id.clone().ok_or_else(|| {
            AdminError::new(format!(
                "provider {provider_name:?} does not configure oauth_client_id"
            ))
        })?;
        OAuthAuthorizationCode::with_proxy(
            OAuthAuthorizationCodeConfig {
                authorization_endpoint,
                token_endpoint,
                client_id,
                scopes: provider.oauth_scopes.clone(),
                callback_timeout: DEFAULT_OAUTH_CALLBACK_TIMEOUT,
            },
            resolution.as_ref().map(|value| &value.url),
            proxy_authentication.as_ref(),
        )
        .map_err(AdminError::from_display)?
    };
    let session = flow.begin().await.map_err(AdminError::from_display)?;
    let authorization_url = session.authorization_url().to_string();
    let redirect_uri = session.redirect_uri().to_string();
    Ok(OAuthLogin {
        provider_name: provider_name.to_owned(),
        provider,
        credential_manager,
        session,
        authorization_url,
        redirect_uri,
        warnings,
        openai_subscription,
    })
}

async fn begin_github_copilot_login(provider_name: &str) -> Result<GitHubCopilotLogin, AdminError> {
    let loader = ConfigLoader::from_environment().map_err(AdminError::from_display)?;
    let credentials_path = loader.credentials_path();
    let effective = loader.load().map_err(AdminError::from_display)?;
    let provider = configured_provider(&effective, provider_name)?.clone();
    validate_github_copilot_profile(provider_name, &provider)?;
    let device_endpoint =
        Url::parse(GITHUB_COPILOT_DEVICE_CODE_ENDPOINT).map_err(AdminError::from_display)?;
    let global_proxy = effective
        .config
        .network
        .proxy
        .as_deref()
        .map(Url::parse)
        .transpose()
        .map_err(AdminError::from_display)?;
    let per_provider = provider
        .proxy
        .as_deref()
        .map(Url::parse)
        .transpose()
        .map_err(AdminError::from_display)?
        .map(|proxy| BTreeMap::from([(provider_name.to_owned(), proxy)]))
        .unwrap_or_default();
    let proxies = ProxySettings {
        global: global_proxy,
        per_provider,
        environment: ProxyEnvironment::capture(),
    };
    let resolution = proxies.resolve(provider_name, &device_endpoint);
    let credential_manager = CredentialManager::system(credentials_path);
    let (proxy_authentication, credential_warnings) =
        match resolution.as_ref().map(|value| value.source) {
            Some(ProxySource::Provider) => resolve_proxy_authentication(
                &credential_manager,
                provider.proxy_username.as_deref(),
                provider.proxy_password_credential.as_deref(),
            )?,
            Some(ProxySource::Global) => resolve_proxy_authentication(
                &credential_manager,
                effective.config.network.proxy_username.as_deref(),
                effective
                    .config
                    .network
                    .proxy_password_credential
                    .as_deref(),
            )?,
            Some(ProxySource::Environment) | None => (None, Vec::new()),
        };
    let flow = GitHubCopilotDeviceFlow::from_compiled(
        resolution.as_ref().map(|value| &value.url),
        proxy_authentication.as_ref(),
    )
    .map_err(AdminError::from_display)?;
    let session = flow.begin().await.map_err(AdminError::from_display)?;
    let verification_uri = session.verification_uri().to_string();
    let user_code = session.user_code().to_owned();
    let oauth_client_id = session.client_id().to_owned();
    let mut warnings = config_warnings(&effective);
    warnings.extend(credential_warnings);
    Ok(GitHubCopilotLogin {
        provider_name: provider_name.to_owned(),
        credential_manager,
        session,
        verification_uri,
        user_code,
        oauth_client_id,
        warnings,
    })
}

fn validate_github_copilot_profile(
    provider_name: &str,
    provider: &ProviderConfig,
) -> Result<(), AdminError> {
    if provider.base_url.is_some()
        || provider.api_key_env.is_some()
        || provider.api_key_credential.is_some()
        || provider.oauth_token_env.is_some()
        || provider.oauth_authorization_endpoint.is_some()
        || provider.oauth_token_endpoint.is_some()
        || provider.oauth_client_id.is_some()
        || !provider.oauth_scopes.is_empty()
        || provider.oauth_access_token_credential.is_some()
        || provider.oauth_refresh_token_credential.is_some()
    {
        return Err(AdminError::new(format!(
            "provider {provider_name:?} uses the fixed github_copilot profile and cannot configure API-key, generic OAuth, or endpoint fields"
        )));
    }
    Ok(())
}

fn configured_provider<'a>(
    effective: &'a rw_store::config::LoadedConfig,
    provider_name: &str,
) -> Result<&'a ProviderConfig, AdminError> {
    effective
        .config
        .providers
        .get(provider_name)
        .ok_or_else(|| {
            AdminError::new(format!(
                "provider {provider_name:?} is not configured at user scope; add [providers.{provider_name}] to the user config"
            ))
        })
}

pub(crate) fn provider_api_key_credential_reference(
    provider_name: &str,
    provider: &ProviderConfig,
) -> Result<CredentialReference, AdminError> {
    let identifier = provider
        .api_key_credential
        .clone()
        .map_or_else(|| default_provider_api_key_credential_id(provider_name), Ok)?;
    let mut reference = CredentialReference::new(identifier);
    if let Some(variable) = &provider.api_key_env {
        reference = reference.with_environment(variable);
    }
    Ok(reference)
}

fn store_provider_api_key_with_manager<E, K>(
    manager: &CredentialManager<E, K>,
    provider_name: &str,
    provider: &ProviderConfig,
    api_key: &StoredSecret<String>,
) -> Result<Vec<String>, AdminError>
where
    E: CredentialEnvironment,
    K: CredentialKeychain,
{
    let reference = provider_api_key_credential_reference(provider_name, provider)?;
    let stored = manager
        .store(&reference, api_key)
        .map_err(AdminError::from_display)?;
    Ok(stored.warnings().iter().map(ToString::to_string).collect())
}

fn store_github_copilot_token_with_manager<E, K>(
    manager: &CredentialManager<E, K>,
    provider_name: &str,
    access_token: &ProviderSecret,
    oauth_client_id: &str,
) -> Result<Vec<String>, AdminError>
where
    E: CredentialEnvironment,
    K: CredentialKeychain,
{
    let credential = GitHubCopilotCredential::from_secret(access_token, oauth_client_id)?;
    let encoded = credential.encode()?;
    let stored = manager
        .store(
            &CredentialReference::new(github_copilot_credential_id(provider_name)),
            &StoredSecret::new(encoded),
        )
        .map_err(AdminError::from_display)?;
    Ok(stored.warnings().iter().map(ToString::to_string).collect())
}

fn resolve_provider_api_key_with_manager<E, K>(
    manager: &CredentialManager<E, K>,
    provider_name: &str,
    provider: &ProviderConfig,
) -> Result<ResolvedProviderApiKey, AdminError>
where
    E: CredentialEnvironment,
    K: CredentialKeychain,
{
    let reference = provider_api_key_credential_reference(provider_name, provider)?;
    let resolved = manager
        .resolve(&reference)
        .map_err(AdminError::from_display)?;
    Ok(ResolvedProviderApiKey {
        api_key: ProviderApiKey(StoredSecret::new(resolved.secret().expose_secret().clone())),
        warnings: resolved
            .warnings()
            .iter()
            .map(ToString::to_string)
            .collect(),
    })
}

fn config_warnings(effective: &rw_store::config::LoadedConfig) -> Vec<String> {
    effective
        .warnings()
        .iter()
        .map(|warning| warning.message().to_owned())
        .collect()
}

fn required_url(provider_name: &str, field: &str, value: Option<&str>) -> Result<Url, AdminError> {
    let value = value.ok_or_else(|| {
        AdminError::new(format!(
            "provider {provider_name:?} does not configure {field}"
        ))
    })?;
    Url::parse(value).map_err(AdminError::from_display)
}

fn resolve_proxy_authentication(
    manager: &CredentialManager,
    username: Option<&str>,
    password_reference: Option<&str>,
) -> Result<(Option<ProxyAuthentication>, Vec<String>), AdminError> {
    let (Some(username), Some(password_reference)) = (username, password_reference) else {
        return Ok((None, Vec::new()));
    };
    let resolved = manager
        .resolve(&CredentialReference::new(password_reference))
        .map_err(AdminError::from_display)?;
    let warnings = resolved
        .warnings()
        .iter()
        .map(ToString::to_string)
        .collect();
    Ok((
        Some(ProxyAuthentication::new(
            username,
            ProviderSecret::new(resolved.secret().expose_secret().clone()),
        )),
        warnings,
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use rw_providers::Secret as ProviderSecret;
    use rw_store::credentials::{
        CredentialEnvironment, CredentialError, CredentialKeychain, CredentialManager,
        CredentialReference, KEYCHAIN_VAULT_ID, KeychainUnavailable, Secret,
    };
    use rw_types::config::ProviderConfig;

    use super::{
        ProviderApiKey, default_provider_api_key_credential_id,
        provider_api_key_credential_reference, resolve_provider_api_key_with_manager,
        store_github_copilot_token_with_manager, store_provider_api_key_with_manager,
    };
    use crate::copilot_credentials::{GitHubCopilotCredential, github_copilot_credential_id};

    #[derive(Clone, Default)]
    struct TestEnvironment(BTreeMap<String, String>);

    impl CredentialEnvironment for TestEnvironment {
        fn get(&self, name: &str) -> Result<Option<String>, CredentialError> {
            Ok(self.0.get(name).cloned())
        }
    }

    #[derive(Clone, Default)]
    struct TestKeychain(Arc<Mutex<BTreeMap<String, String>>>);

    impl CredentialKeychain for TestKeychain {
        fn get(&self, identifier: &str) -> Result<Option<Secret<String>>, KeychainUnavailable> {
            self.0
                .lock()
                .map_err(|_| KeychainUnavailable)
                .map(|values| values.get(identifier).cloned().map(Secret::new))
        }

        fn set(
            &self,
            identifier: &str,
            secret: &Secret<String>,
        ) -> Result<(), KeychainUnavailable> {
            self.0
                .lock()
                .map_err(|_| KeychainUnavailable)?
                .insert(identifier.to_owned(), secret.expose_secret().clone());
            Ok(())
        }
    }

    #[test]
    fn terminal_input_trims_only_line_terminators_and_redacts_debug() {
        let key = ProviderApiKey::from_terminal_input("  key with spaces  \r\n".to_owned())
            .unwrap_or_else(|error| panic!("non-empty key must parse: {error}"));
        assert_eq!(key.expose_secret(), "  key with spaces  ");
        assert!(!format!("{key:?}").contains("key with spaces"));
        assert!(ProviderApiKey::from_terminal_input("\r\n".to_owned()).is_err());
    }

    #[test]
    fn provider_reference_uses_safe_default_and_optional_environment() {
        assert_eq!(
            default_provider_api_key_credential_id("openai-compatible")
                .unwrap_or_else(|error| panic!("safe provider name must work: {error}")),
            "providers.openai-compatible.api_key"
        );
        assert!(default_provider_api_key_credential_id("unsafe/provider").is_err());

        let provider = ProviderConfig {
            api_key_credential: Some("custom-api-key".to_owned()),
            api_key_env: Some("CUSTOM_API_KEY".to_owned()),
            ..ProviderConfig::default()
        };
        let reference = provider_api_key_credential_reference("fixture", &provider)
            .unwrap_or_else(|error| panic!("configured reference must work: {error}"));
        assert_eq!(reference.identifier(), "custom-api-key");
        assert_eq!(reference.environment_variable(), Some("CUSTOM_API_KEY"));
    }

    #[test]
    fn injected_backends_store_configured_id_and_resolve_environment_first() {
        let keychain = TestKeychain::default();
        let manager = CredentialManager::with_backends(
            TestEnvironment(BTreeMap::from([(
                "FIXTURE_API_KEY".to_owned(),
                "environment-key".to_owned(),
            )])),
            keychain.clone(),
            PathBuf::from("unused-test-credentials.toml"),
        );
        let provider = ProviderConfig {
            api_key_credential: Some("fixture-stored-key".to_owned()),
            api_key_env: Some("FIXTURE_API_KEY".to_owned()),
            ..ProviderConfig::default()
        };

        let key = ProviderApiKey::from_terminal_input("stored-key".to_owned())
            .unwrap_or_else(|error| panic!("stored key must parse: {error}"));
        let warnings = store_provider_api_key_with_manager(&manager, "fixture", &provider, &key.0)
            .unwrap_or_else(|error| panic!("injected keychain store must work: {error}"));
        assert!(warnings.is_empty());
        let stored = manager
            .resolve(&CredentialReference::new("fixture-stored-key"))
            .unwrap_or_else(|error| panic!("stored key must resolve from the vault: {error}"));
        assert_eq!(stored.secret().expose_secret(), "stored-key");

        let resolved = resolve_provider_api_key_with_manager(&manager, "fixture", &provider)
            .unwrap_or_else(|error| panic!("injected resolution must work: {error}"));
        assert_eq!(resolved.api_key().expose_secret(), "environment-key");
        assert!(resolved.warnings().is_empty());
    }

    #[test]
    fn copilot_token_is_one_rottweiler_owned_logical_vault_entry() {
        let keychain = TestKeychain::default();
        let manager = CredentialManager::with_backends(
            TestEnvironment::default(),
            keychain.clone(),
            PathBuf::from("unused-test-credentials.toml"),
        );
        let token = ProviderSecret::new("copilot-token-canary".to_owned());
        let warnings = store_github_copilot_token_with_manager(
            &manager,
            "github-copilot",
            &token,
            "rottweiler-test-client",
        )
        .unwrap_or_else(|error| panic!("injected token store must work: {error}"));
        assert!(warnings.is_empty());
        let identifier = github_copilot_credential_id("github-copilot");
        let stored = manager
            .resolve(&CredentialReference::new(identifier.clone()))
            .unwrap_or_else(|error| panic!("stored Copilot token must resolve: {error}"));
        let credential = GitHubCopilotCredential::parse(stored.secret().expose_secret())
            .unwrap_or_else(|error| panic!("stored Copilot credential must parse: {error}"));
        assert_eq!(credential.access_token(), "copilot-token-canary");
        assert_eq!(credential.oauth_client_id(), "rottweiler-test-client");
        let entries = keychain
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(entries.len(), 1);
        assert!(entries.contains_key(KEYCHAIN_VAULT_ID));
    }
}
