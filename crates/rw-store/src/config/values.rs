use std::collections::BTreeMap;

use rw_types::config::ProviderAuthScheme;
use url::Url;

use super::{ConfigError, ConfigSource, ConfigWarning, LoadedConfig};

impl LoadedConfig {
    /// Returns the source of a named effective leaf.
    #[must_use]
    pub fn provenance(&self, key: &str) -> Option<&ConfigSource> {
        self.provenance.get(key)
    }

    /// Security warnings raised while loading project configuration.
    #[must_use]
    pub fn warnings(&self) -> &[ConfigWarning] {
        &self.warnings
    }

    /// Whether the exact current project executable inventory was trusted.
    #[must_use]
    pub const fn project_trusted(&self) -> bool {
        self.project_trusted
    }

    /// Renders stable, scriptable effective values with a source per leaf.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn render_with_provenance(&self) -> String {
        let mut lines = vec![
            self.render_leaf(
                "engine.max_concurrent_sessions",
                &self.config.engine.max_concurrent_sessions.to_string(),
            ),
            self.render_leaf(
                "engine.subagent_max_depth",
                &self.config.engine.subagent_max_depth.to_string(),
            ),
            self.render_leaf(
                "engine.subagent_max_concurrency",
                &self.config.engine.subagent_max_concurrency.to_string(),
            ),
            self.render_leaf("models.default", &quoted(&self.config.models.default)),
        ];

        if self.config.models.aliases.is_empty() {
            lines.push(self.render_leaf("models.aliases", "{}"));
        } else {
            for (alias, candidates) in &self.config.models.aliases {
                let key = format!("models.aliases.{alias}");
                let value = candidates
                    .iter()
                    .map(|candidate| quoted(candidate))
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(self.render_leaf(&key, &format!("[{value}]")));
            }
        }

        if self.config.models.thinking.is_empty() {
            lines.push(self.render_leaf("models.thinking", "{}"));
        } else {
            for (alias, level) in &self.config.models.thinking {
                lines.push(self.render_leaf(&format!("models.thinking.{alias}"), level.as_str()));
            }
        }

        lines.push(self.render_leaf("compaction.auto", &self.config.compaction.auto.to_string()));
        lines.push(self.render_leaf(
            "compaction.reserved",
            &optional_u64(self.config.compaction.reserved_tokens),
        ));
        lines.push(
            self.render_leaf(
                "compaction.model_alias",
                &self
                    .config
                    .compaction
                    .model_alias
                    .as_deref()
                    .map_or_else(|| "<unset>".to_owned(), quoted),
            ),
        );
        for (key, value) in [
            (
                "budget.session_cost_cap_micros_usd",
                self.config.budget.session_cost_cap_micros_usd,
            ),
            (
                "budget.daily_cost_cap_micros_usd",
                self.config.budget.daily_cost_cap_micros_usd,
            ),
            (
                "budget.session_ai_credit_cap_micros",
                self.config.budget.session_ai_credit_cap_micros,
            ),
            (
                "budget.daily_ai_credit_cap_micros",
                self.config.budget.daily_ai_credit_cap_micros,
            ),
            (
                "budget.session_token_cap",
                self.config.budget.session_token_cap,
            ),
            ("budget.daily_token_cap", self.config.budget.daily_token_cap),
            (
                "budget.spend_rate_alarm_micros_usd_per_minute",
                self.config.budget.spend_rate_alarm_micros_usd_per_minute,
            ),
            (
                "budget.ai_credit_rate_alarm_micros_per_minute",
                self.config.budget.ai_credit_rate_alarm_micros_per_minute,
            ),
            (
                "budget.token_rate_alarm_per_minute",
                self.config.budget.token_rate_alarm_per_minute,
            ),
        ] {
            lines.push(self.render_leaf(key, &optional_u64(value)));
        }
        lines.push(self.render_leaf(
            "budget.warn_at_percent",
            &self.config.budget.warn_at_percent.to_string(),
        ));

        if self.config.providers.is_empty() {
            lines.push(self.render_leaf("providers", "{}"));
        } else {
            for (name, provider) in &self.config.providers {
                lines.push(
                    self.render_leaf(&format!("providers.{name}.kind"), &quoted(&provider.kind)),
                );
                if let Some(base_url) = &provider.base_url {
                    lines.push(
                        self.render_leaf(&format!("providers.{name}.base_url"), &quoted(base_url)),
                    );
                }
                if let Some(path_template) = &provider.path_template {
                    lines.push(self.render_leaf(
                        &format!("providers.{name}.path_template"),
                        &quoted(path_template),
                    ));
                }
                for (field, value) in [
                    ("headers", format!("{:?}", provider.headers)),
                    (
                        "header_credentials",
                        format!("{:?}", provider.header_credentials),
                    ),
                    ("extra_query", format!("{:?}", provider.extra_query)),
                    ("extra_body", format!("{:?}", provider.extra_body)),
                    ("model_ids", format!("{:?}", provider.model_ids)),
                ] {
                    if value != "{}" {
                        lines.push(self.render_leaf(&format!("providers.{name}.{field}"), &value));
                    }
                }
                if let Some(auth_scheme) = &provider.auth_scheme {
                    let value = match auth_scheme {
                        ProviderAuthScheme::Bearer => "bearer".to_owned(),
                        ProviderAuthScheme::None => "none".to_owned(),
                        ProviderAuthScheme::Header { name, value_prefix } => format!(
                            "header(name={}, value_prefix={})",
                            quoted(name),
                            quoted(value_prefix)
                        ),
                    };
                    lines.push(self.render_leaf(&format!("providers.{name}.auth_scheme"), &value));
                }
                for (model, pricing) in &provider.pricing {
                    let mut fields = Vec::new();
                    if let Some(currency) = &pricing.currency {
                        fields.push(format!("currency = {}", quoted(currency)));
                    }
                    for (field, value) in [
                        ("input_per_million", pricing.input_per_million.as_ref()),
                        ("output_per_million", pricing.output_per_million.as_ref()),
                        (
                            "cache_read_per_million",
                            pricing.cache_read_per_million.as_ref(),
                        ),
                        (
                            "cache_write_per_million",
                            pricing.cache_write_per_million.as_ref(),
                        ),
                    ] {
                        if let Some(value) = value {
                            fields.push(format!("{field} = {value}"));
                        }
                    }
                    fields.push("source = user_config".to_owned());
                    lines.push(self.render_leaf(
                        &format!("providers.{name}.pricing.{}", quoted(model)),
                        &format!("{{ {} }}", fields.join(", ")),
                    ));
                }
                if let Some(proxy) = &provider.proxy {
                    lines.push(
                        self.render_leaf(
                            &format!("providers.{name}.proxy"),
                            &redacted_proxy(proxy),
                        ),
                    );
                }
                if let Some(username) = &provider.proxy_username {
                    lines.push(self.render_leaf(
                        &format!("providers.{name}.proxy_username"),
                        &quoted(username),
                    ));
                }
                if let Some(credential) = &provider.proxy_password_credential {
                    lines.push(self.render_leaf(
                        &format!("providers.{name}.proxy_password_credential"),
                        &quoted(credential),
                    ));
                }
                if let Some(variable) = &provider.api_key_env {
                    lines.push(
                        self.render_leaf(
                            &format!("providers.{name}.api_key_env"),
                            &quoted(variable),
                        ),
                    );
                }
                if let Some(credential) = &provider.api_key_credential {
                    lines.push(self.render_leaf(
                        &format!("providers.{name}.api_key_credential"),
                        &quoted(credential),
                    ));
                }
                if let Some(variable) = &provider.oauth_token_env {
                    lines.push(self.render_leaf(
                        &format!("providers.{name}.oauth_token_env"),
                        &quoted(variable),
                    ));
                }
                for (field, value) in [
                    (
                        "oauth_authorization_endpoint",
                        provider.oauth_authorization_endpoint.as_deref(),
                    ),
                    (
                        "oauth_token_endpoint",
                        provider.oauth_token_endpoint.as_deref(),
                    ),
                    ("oauth_client_id", provider.oauth_client_id.as_deref()),
                    (
                        "oauth_access_token_credential",
                        provider.oauth_access_token_credential.as_deref(),
                    ),
                    (
                        "oauth_refresh_token_credential",
                        provider.oauth_refresh_token_credential.as_deref(),
                    ),
                ] {
                    if let Some(value) = value {
                        lines.push(
                            self.render_leaf(&format!("providers.{name}.{field}"), &quoted(value)),
                        );
                    }
                }
                if !provider.oauth_scopes.is_empty() {
                    let scopes = provider
                        .oauth_scopes
                        .iter()
                        .map(|scope| quoted(scope))
                        .collect::<Vec<_>>()
                        .join(", ");
                    lines.push(self.render_leaf(
                        &format!("providers.{name}.oauth_scopes"),
                        &format!("[{scopes}]"),
                    ));
                }
            }
        }

        lines.push(
            self.render_leaf(
                "network.proxy",
                &self
                    .config
                    .network
                    .proxy
                    .as_deref()
                    .map_or_else(|| "<unset>".to_owned(), redacted_proxy),
            ),
        );
        lines.push(
            self.render_leaf(
                "network.proxy_username",
                &self
                    .config
                    .network
                    .proxy_username
                    .as_deref()
                    .map_or_else(|| "<unset>".to_owned(), quoted),
            ),
        );
        lines.push(
            self.render_leaf(
                "network.proxy_password_credential",
                &self
                    .config
                    .network
                    .proxy_password_credential
                    .as_deref()
                    .map_or_else(|| "<unset>".to_owned(), quoted),
            ),
        );
        lines.push(
            self.render_leaf(
                "websearch.endpoint",
                &self
                    .config
                    .websearch
                    .endpoint
                    .as_deref()
                    .map_or_else(|| "<unset>".to_owned(), quoted),
            ),
        );
        lines.push(self.render_leaf(
            "websearch.query_parameter",
            &quoted(&self.config.websearch.query_parameter),
        ));
        lines.push(self.render_leaf(
            "websearch.header_credentials",
            &format!(
                "{:?}",
                self.config
                    .websearch
                    .header_credentials
                    .keys()
                    .collect::<Vec<_>>()
            ),
        ));
        lines.push(self.render_leaf(
            "permissions.default",
            self.config.permissions.default.as_str(),
        ));
        lines.push(self.render_leaf(
            "sandbox.safe_list",
            &format!("{:?}", self.config.sandbox.safe_list),
        ));
        lines.push(self.render_leaf(
            "toolchain.runtime_read_roots",
            &format!("{:?}", self.config.toolchain.runtime_read_roots),
        ));
        lines.push(
            self.render_leaf(
                "toolchain.formatter",
                &self
                    .config
                    .toolchain
                    .formatter
                    .as_deref()
                    .map_or_else(|| "<unset>".to_owned(), quoted),
            ),
        );
        lines.push(self.render_leaf(
            "toolchain.linters",
            &format!("{:?}", self.config.toolchain.linters),
        ));
        lines.push(
            self.render_leaf(
                "toolchain.test",
                &self
                    .config
                    .toolchain
                    .test
                    .as_deref()
                    .map_or_else(|| "<unset>".to_owned(), quoted),
            ),
        );
        lines.push(self.render_leaf(
            "toolchain.rules",
            &format!("{:?}", self.config.toolchain.rules),
        ));
        lines.push(self.render_leaf(
            "telemetry.enabled",
            &self.config.telemetry.enabled.to_string(),
        ));
        lines.push(self.render_leaf("updates.channel", self.config.updates.channel.as_str()));
        lines.push(self.render_leaf("ui.theme", &quoted(&self.config.ui.theme)));
        lines.join("\n") + "\n"
    }

    fn render_leaf(&self, key: &str, value: &str) -> String {
        let source = self
            .provenance
            .get(key)
            .or_else(|| parent_alias_source(&self.provenance, key))
            .map_or_else(|| "built-in".to_owned(), ToString::to_string);
        format!("{key} = {value} [{source}]")
    }
}

pub(super) fn quoted(value: &str) -> String {
    format!("{value:?}")
}

pub(super) fn optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "<unset>".to_owned(), |value| value.to_string())
}

pub(super) fn redacted_proxy(value: &str) -> String {
    Url::parse(value).map_or_else(
        |_| "<invalid proxy>".to_owned(),
        |proxy| quoted(&proxy.origin().ascii_serialization()),
    )
}

pub(super) fn parent_alias_source<'a>(
    provenance: &'a BTreeMap<String, ConfigSource>,
    key: &str,
) -> Option<&'a ConfigSource> {
    ["models.aliases", "models.thinking", "providers"]
        .into_iter()
        .find_map(|parent| {
            key.starts_with(&format!("{parent}."))
                .then(|| provenance.get(parent))
                .flatten()
        })
}

pub(super) fn override_reason(error: ConfigError) -> String {
    match error {
        ConfigError::CliOverride { reason, .. } => reason,
        other => other.to_string(),
    }
}
