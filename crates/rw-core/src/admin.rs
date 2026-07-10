use std::{collections::BTreeMap, fmt, path::PathBuf};

use rw_providers::{
    DEFAULT_OAUTH_CALLBACK_TIMEOUT, OAuthAuthorizationCode, OAuthAuthorizationCodeConfig,
    ProxyAuthentication, ProxyEnvironment, ProxySettings, ProxySource, Secret as ProviderSecret,
    default_models_path, refresh_models_dev_with_proxy_auth,
};
use rw_store::{
    config::ConfigLoader,
    credentials::{CredentialManager, CredentialReference, Secret as StoredSecret},
};
use rw_types::config::ProviderConfig;
use thiserror::Error;
use url::Url;

/// Default public model-catalog endpoint used by the administrative facade.
pub const DEFAULT_MODEL_CATALOG_URL: &str = rw_providers::DEFAULT_MODELS_DEV_URL;

/// Sanitized failure returned by a headless administrative workflow.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct AdminError {
    message: String,
}

impl AdminError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn from_display(error: impl fmt::Display) -> Self {
        Self::new(error.to_string())
    }
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
    let proxies = ProxySettings {
        global,
        per_provider: BTreeMap::new(),
        environment: ProxyEnvironment::capture(),
    };
    let output = output
        .map_or_else(default_models_path, Ok)
        .map_err(AdminError::from_display)?;
    let manager = CredentialManager::system(credentials_path);
    let (proxy_authentication, credential_warnings) = resolve_proxy_authentication(
        &manager,
        effective.config.network.proxy_username.as_deref(),
        effective
            .config
            .network
            .proxy_password_credential
            .as_deref(),
    )?;
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
    let authorization_endpoint = required_url(
        provider_name,
        "oauth_authorization_endpoint",
        provider.oauth_authorization_endpoint.as_deref(),
    )?;
    let token_endpoint = required_url(
        provider_name,
        "oauth_token_endpoint",
        provider.oauth_token_endpoint.as_deref(),
    )?;
    let client_id = provider.oauth_client_id.clone().ok_or_else(|| {
        AdminError::new(format!(
            "provider {provider_name:?} does not configure oauth_client_id"
        ))
    })?;
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
    let flow = OAuthAuthorizationCode::with_proxy(
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
    .map_err(AdminError::from_display)?;
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
