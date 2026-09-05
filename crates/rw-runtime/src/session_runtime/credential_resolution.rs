use miette::Result;
use miette::miette;
use rw_core::Config;
use rw_providers::FixtureRedactor;
use rw_store::credentials::CredentialManager;
use rw_store::credentials::CredentialReference;
use rw_tools::UpstreamProxy;
use rw_types::config::WebSearchConfig;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::OnceCell;
use url::Url;

#[derive(Clone)]
pub(super) struct ResolvedToolProxy {
    pub(super) url: Url,
    pub(super) upstream: UpstreamProxy,
}

pub(super) type DeferredCredentialResolver =
    Arc<dyn Fn(&str) -> std::result::Result<String, String> + Send + Sync>;

#[derive(Clone)]
pub(super) struct DeferredToolProxy {
    pub(super) configured: String,
    pub(super) username: Option<String>,
    pub(super) password_credential: Option<String>,
    pub(super) redactor: FixtureRedactor,
    pub(super) resolver: DeferredCredentialResolver,
    pub(super) resolved: Arc<OnceCell<ResolvedToolProxy>>,
}

impl DeferredToolProxy {
    pub(super) fn from_config(
        config: &Config,
        credentials_path: &Path,
        offline: bool,
        redactor: FixtureRedactor,
    ) -> Result<Option<Self>> {
        if offline {
            return Ok(None);
        }
        let Some(configured) = config.network.proxy.clone() else {
            return Ok(None);
        };
        Url::parse(&configured)
            .map_err(|error| miette!("configured global proxy is invalid: {error}"))?;
        match (
            config.network.proxy_username.as_ref(),
            config.network.proxy_password_credential.as_ref(),
        ) {
            (None, None) | (Some(_), Some(_)) => {}
            _ => {
                return Err(miette!(
                    "global proxy authentication requires username and password credential reference"
                ));
            }
        }
        let credentials_path = credentials_path.to_path_buf();
        let resolver: DeferredCredentialResolver = Arc::new(move |reference| {
            let resolved = CredentialManager::system(&credentials_path)
                .resolve_authorized(&CredentialReference::new(reference))
                .map_err(|error| format!("global proxy credential could not resolve: {error}"))?;
            for warning in resolved.warnings() {
                tracing::warn!("{warning}");
            }
            Ok(resolved.secret().expose_secret().clone())
        });
        Ok(Some(Self {
            configured,
            username: config.network.proxy_username.clone(),
            password_credential: config.network.proxy_password_credential.clone(),
            redactor,
            resolver,
            resolved: Arc::new(OnceCell::new()),
        }))
    }

    #[cfg(test)]
    pub(super) fn with_resolver(
        configured: impl Into<String>,
        username: Option<String>,
        password_credential: Option<String>,
        redactor: FixtureRedactor,
        resolver: DeferredCredentialResolver,
    ) -> Self {
        Self {
            configured: configured.into(),
            username,
            password_credential,
            redactor,
            resolver,
            resolved: Arc::new(OnceCell::new()),
        }
    }

    pub(super) async fn resolve(&self) -> std::result::Result<ResolvedToolProxy, String> {
        self.resolved
            .get_or_try_init(|| async {
                let configured = self.configured.clone();
                let username = self.username.clone();
                let password_credential = self.password_credential.clone();
                let redactor = self.redactor.clone();
                let resolver = Arc::clone(&self.resolver);
                tokio::task::spawn_blocking(move || {
                    resolve_tool_proxy_parts(
                        &configured,
                        username.as_deref(),
                        password_credential.as_deref(),
                        &redactor,
                        |reference| resolver(reference).map_err(miette::Report::msg),
                    )
                    .map_err(|error| error.to_string())
                })
                .await
                .map_err(|error| format!("tool proxy credential worker failed: {error}"))?
            })
            .await
            .cloned()
    }
}

#[derive(Clone)]
pub(super) struct DeferredWebSearchHeaders {
    pub(super) config: WebSearchConfig,
    pub(super) redactor: FixtureRedactor,
    pub(super) resolver: DeferredCredentialResolver,
    pub(super) resolved: Arc<OnceCell<BTreeMap<String, String>>>,
}

impl DeferredWebSearchHeaders {
    pub(super) fn from_config(
        config: &WebSearchConfig,
        credentials_path: &Path,
        offline: bool,
        redactor: FixtureRedactor,
    ) -> Option<Self> {
        if offline || config.endpoint.is_none() || config.header_credentials.is_empty() {
            return None;
        }
        let credentials_path = credentials_path.to_path_buf();
        let resolver: DeferredCredentialResolver = Arc::new(move |reference| {
            let resolved = CredentialManager::system(&credentials_path)
                .resolve_authorized(&CredentialReference::new(reference))
                .map_err(|error| {
                    format!("web-search credential {reference:?} could not resolve: {error}")
                })?;
            for warning in resolved.warnings() {
                tracing::warn!("{warning}");
            }
            Ok(resolved.secret().expose_secret().clone())
        });
        Some(Self {
            config: config.clone(),
            redactor,
            resolver,
            resolved: Arc::new(OnceCell::new()),
        })
    }

    #[cfg(test)]
    pub(super) fn with_resolver(
        config: WebSearchConfig,
        redactor: FixtureRedactor,
        resolver: DeferredCredentialResolver,
    ) -> Self {
        Self {
            config,
            redactor,
            resolver,
            resolved: Arc::new(OnceCell::new()),
        }
    }

    pub(super) async fn resolve(&self) -> std::result::Result<BTreeMap<String, String>, String> {
        self.resolved
            .get_or_try_init(|| async {
                let config = self.config.clone();
                let redactor = self.redactor.clone();
                let resolver = Arc::clone(&self.resolver);
                tokio::task::spawn_blocking(move || {
                    resolve_websearch_headers_with(&config, false, &redactor, |reference| {
                        resolver(reference).map_err(miette::Report::msg)
                    })
                    .map_err(|error| error.to_string())
                })
                .await
                .map_err(|error| format!("web-search credential worker failed: {error}"))?
            })
            .await
            .cloned()
    }
}

pub(super) fn resolve_tool_proxy_parts(
    configured: &str,
    username: Option<&str>,
    password_credential: Option<&str>,
    redactor: &FixtureRedactor,
    mut resolve: impl FnMut(&str) -> Result<String>,
) -> Result<ResolvedToolProxy> {
    let url = Url::parse(configured)
        .map_err(|error| miette!("configured global proxy is invalid: {error}"))?;
    let mut upstream = UpstreamProxy::new(url.clone())
        .map_err(|error| miette!("configured global proxy is invalid: {error}"))?;
    match (username, password_credential) {
        (None, None) => {}
        (Some(username), Some(reference)) => {
            let password = resolve(reference)?;
            redactor.register_known_value(&password);
            upstream = upstream.with_basic_auth(username, &password);
        }
        _ => {
            return Err(miette!(
                "global proxy authentication requires username and password credential reference"
            ));
        }
    }
    Ok(ResolvedToolProxy { url, upstream })
}

#[cfg(test)]
pub(super) fn resolve_tool_proxy(
    config: &Config,
    credentials_path: &Path,
    offline: bool,
    redactor: &FixtureRedactor,
) -> Result<Option<ResolvedToolProxy>> {
    if offline {
        return Ok(None);
    }
    let Some(configured) = config.network.proxy.as_deref() else {
        return Ok(None);
    };
    resolve_tool_proxy_parts(
        configured,
        config.network.proxy_username.as_deref(),
        config.network.proxy_password_credential.as_deref(),
        redactor,
        |reference| {
            let resolved = CredentialManager::system(credentials_path)
                .resolve(&CredentialReference::new(reference))
                .map_err(|error| miette!("global proxy credential could not resolve: {error}"))?;
            for warning in resolved.warnings() {
                tracing::warn!("{warning}");
            }
            Ok(resolved.secret().expose_secret().clone())
        },
    )
    .map(Some)
}

pub(super) fn resolve_websearch_headers_with(
    config: &WebSearchConfig,
    offline: bool,
    redactor: &FixtureRedactor,
    mut resolve: impl FnMut(&str) -> Result<String>,
) -> Result<BTreeMap<String, String>> {
    if offline || config.endpoint.is_none() {
        return Ok(BTreeMap::new());
    }
    let mut headers = BTreeMap::new();
    for (header, reference) in &config.header_credentials {
        let value = resolve(reference)?;
        redactor.register_known_value(&value);
        headers.insert(header.clone(), value);
    }
    Ok(headers)
}
