use std::collections::BTreeMap;

use url::Url;

/// Captured standard proxy variables. Capture is separated from resolution so
/// tests and config rendering never mutate process-global environment state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProxyEnvironment {
    /// `HTTP_PROXY` or lowercase equivalent.
    pub http_proxy: Option<Url>,
    /// `HTTPS_PROXY` or lowercase equivalent.
    pub https_proxy: Option<Url>,
    /// `ALL_PROXY` or lowercase equivalent.
    pub all_proxy: Option<Url>,
    /// Comma-separated host suffixes that bypass an environment proxy.
    pub no_proxy: Vec<String>,
}

impl ProxyEnvironment {
    /// Reads standard variables. Invalid URLs are ignored rather than logged,
    /// avoiding accidental emission of inline credentials.
    #[must_use]
    pub fn capture() -> Self {
        let plain = first_env(&["HTTP_PROXY", "http_proxy"])
            .as_deref()
            .and_then(parse_safe_proxy);
        let secure = first_env(&["HTTPS_PROXY", "https_proxy"])
            .as_deref()
            .and_then(parse_safe_proxy);
        let universal = first_env(&["ALL_PROXY", "all_proxy"])
            .as_deref()
            .and_then(parse_safe_proxy);
        let no_proxy = first_env(&["NO_PROXY", "no_proxy"])
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_ascii_lowercase)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            http_proxy: plain,
            https_proxy: secure,
            all_proxy: universal,
            no_proxy,
        }
    }
}

fn first_env(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| std::env::var(name).ok())
}

fn parse_safe_proxy(value: &str) -> Option<Url> {
    let url = Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url)
}

/// Proxy configuration in descending precedence before environment fallback.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProxySettings {
    /// User-level global outbound proxy.
    pub global: Option<Url>,
    /// Provider-specific overrides by provider name.
    pub per_provider: BTreeMap<String, Url>,
    /// Standard environment fallback.
    pub environment: ProxyEnvironment,
}

/// Where an effective proxy came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxySource {
    /// Provider-specific configuration.
    Provider,
    /// Global Rottweiler network configuration.
    Global,
    /// Standard process environment.
    Environment,
}

/// Effective proxy for a provider request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxyResolution {
    /// Sanitized proxy URL (credentials are prohibited at config parsing).
    pub url: Url,
    /// Winning precedence layer.
    pub source: ProxySource,
}

impl ProxySettings {
    /// Resolves provider > global > environment. `NO_PROXY` participates only
    /// in the lowest-precedence environment layer.
    #[must_use]
    pub fn resolve(&self, provider: &str, endpoint: &Url) -> Option<ProxyResolution> {
        if let Some(url) = self.per_provider.get(provider) {
            return Some(ProxyResolution {
                url: url.clone(),
                source: ProxySource::Provider,
            });
        }
        self.resolve_global(endpoint)
    }

    /// Resolves the global Rottweiler proxy followed by environment fallback,
    /// deliberately ignoring provider-specific overrides. Non-provider network
    /// clients such as model-catalog refresh and self-update use this path.
    #[must_use]
    pub fn resolve_global(&self, endpoint: &Url) -> Option<ProxyResolution> {
        if let Some(url) = &self.global {
            return Some(ProxyResolution {
                url: url.clone(),
                source: ProxySource::Global,
            });
        }
        if host_is_bypassed(endpoint, &self.environment.no_proxy) {
            return None;
        }
        let url = match endpoint.scheme() {
            "https" => self
                .environment
                .https_proxy
                .as_ref()
                .or(self.environment.http_proxy.as_ref())
                .or(self.environment.all_proxy.as_ref()),
            "http" => self
                .environment
                .http_proxy
                .as_ref()
                .or(self.environment.all_proxy.as_ref()),
            _ => self.environment.all_proxy.as_ref(),
        }?;
        Some(ProxyResolution {
            url: url.clone(),
            source: ProxySource::Environment,
        })
    }
}

fn host_is_bypassed(endpoint: &Url, entries: &[String]) -> bool {
    let Some(host) = endpoint.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    entries.iter().any(|entry| {
        if entry == "*" {
            return true;
        }
        let suffix = entry.trim_start_matches('.');
        host == suffix || host.ends_with(&format!(".{suffix}"))
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use url::Url;

    use super::{ProxyEnvironment, ProxySettings, ProxySource};

    fn url(value: &str) -> Url {
        Url::parse(value).unwrap_or_else(|error| panic!("fixture URL must parse: {error}"))
    }

    #[test]
    fn precedence_and_no_proxy_are_explicit() {
        let settings = ProxySettings {
            global: Some(url("http://global.test:8080")),
            per_provider: BTreeMap::from([(
                "anthropic".to_owned(),
                url("http://anthropic.test:8080"),
            )]),
            environment: ProxyEnvironment {
                http_proxy: Some(url("http://env.test:8080")),
                https_proxy: Some(url("http://secure-env.test:8080")),
                all_proxy: Some(url("http://all-env.test:8080")),
                no_proxy: vec!["api.openai.test".to_owned()],
            },
        };
        let endpoint = url("https://api.provider.test/v1");
        let provider = settings
            .resolve("anthropic", &endpoint)
            .unwrap_or_else(|| panic!("provider proxy expected"));
        assert_eq!(provider.source, ProxySource::Provider);
        let global = settings
            .resolve("openai", &endpoint)
            .unwrap_or_else(|| panic!("global proxy expected"));
        assert_eq!(global.source, ProxySource::Global);

        let environment_only = ProxySettings {
            global: None,
            per_provider: BTreeMap::new(),
            environment: settings.environment.clone(),
        };
        assert_eq!(
            environment_only
                .resolve("openai", &endpoint)
                .map(|resolution| resolution.source),
            Some(ProxySource::Environment)
        );
        assert_eq!(
            settings
                .resolve("openai", &url("https://api.openai.test/v1"))
                .map(|resolution| resolution.source),
            Some(ProxySource::Global)
        );
        assert_eq!(
            environment_only.resolve("openai", &url("https://api.openai.test/v1")),
            None
        );
    }

    #[test]
    fn global_resolution_ignores_provider_overrides() {
        let settings = ProxySettings {
            global: Some(url("http://global.test:8080")),
            per_provider: BTreeMap::from([(
                "models.dev".to_owned(),
                url("http://provider-only.test:8080"),
            )]),
            environment: ProxyEnvironment::default(),
        };
        let resolved = settings
            .resolve_global(&url("https://models.dev/api.json"))
            .unwrap_or_else(|| panic!("global proxy expected"));
        assert_eq!(resolved.source, ProxySource::Global);
        assert_eq!(resolved.url, url("http://global.test:8080"));
    }
}
