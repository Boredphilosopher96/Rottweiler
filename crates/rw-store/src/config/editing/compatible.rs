//! Atomic, non-destructive onboarding for compatible inference endpoints.
use super::{
    ConfigError, ConfigLoader, LoadedConfig, acquire_tui_settings_lock, persist_tui_config_atomic,
    persist_tui_provenance, prepare_tui_config_parent, read_tui_config_document,
    validate_tui_config_file,
};
use rw_types::config::{ProviderAuthScheme, ProviderConfig};
use rw_types::{CompatibleProviderAdapter, CompatibleProviderAuth, CompatibleProviderSetup};

impl ConfigLoader {
    /// Adds a user-owned endpoint and optional static model route without changing defaults.
    ///
    /// # Errors
    /// Rejects unsafe endpoints, conflicting names, invalid configuration, and unsafe writes.
    pub fn configure_compatible_provider(
        &self,
        setup: &CompatibleProviderSetup,
    ) -> Result<LoadedConfig, ConfigError> {
        let invalid = |reason: &str| ConfigError::InvalidUserSetting {
            key: "providers".into(),
            reason: reason.into(),
        };
        setup.validate().map_err(invalid)?;
        let endpoint = url::Url::parse(&setup.endpoint)
            .map_err(|_| invalid("endpoint must be an absolute HTTP(S) URL"))?;
        if setup.auth == CompatibleProviderAuth::None
            && !super::super::validation::is_loopback_endpoint(&endpoint)
        {
            return Err(invalid(
                "authentication may be omitted only for an explicit loopback endpoint",
            ));
        }
        let profile = ProviderConfig {
            kind: match setup.adapter {
                CompatibleProviderAdapter::Chat => "openai_compatible",
                CompatibleProviderAdapter::Responses => "openai_compatible_responses",
            }
            .into(),
            base_url: Some(setup.endpoint.clone()),
            auth_scheme: (setup.auth == CompatibleProviderAuth::None)
                .then_some(ProviderAuthScheme::None),
            api_key_credential: if setup.auth == CompatibleProviderAuth::ApiKey {
                Some(
                    rw_types::default_provider_api_key_credential_id(&setup.provider)
                        .map_err(invalid)?,
                )
            } else {
                None
            },
            ..ProviderConfig::default()
        };
        let alias = format!("{}-model", setup.provider);
        let route = setup
            .initial_model
            .as_ref()
            .map(|model| vec![format!("{}/{model}", setup.provider)]);
        let parent = self
            .user_path
            .parent()
            .ok_or_else(|| invalid("user configuration has no parent directory"))?;
        prepare_tui_config_parent(parent, &self.user_path)?;
        let _lock = acquire_tui_settings_lock(parent, "providers")?;
        validate_tui_config_file(&self.user_path, "providers")?;
        let effective = self.load()?;
        if let Some(existing) = effective.config.providers.get(&setup.provider) {
            if existing != &profile {
                return Err(invalid("provider name already exists; choose a new name"));
            }
            if route
                .as_ref()
                .is_none_or(|route| effective.config.models.aliases.get(&alias) == Some(route))
            {
                return Ok(effective);
            }
            // An identical endpoint may add its missing initial route on retry.
            // Existing aliases are rejected below, even when this profile owns them.
        }
        if route.is_some() && effective.config.models.aliases.contains_key(&alias) {
            return Err(invalid(
                "the generated model alias already exists; choose a new provider name",
            ));
        }
        let mut candidate = effective.config.clone();
        candidate
            .providers
            .insert(setup.provider.clone(), profile.clone());
        if let Some(route) = &route {
            candidate
                .models
                .aliases
                .insert(alias.clone(), route.clone());
        }
        super::super::validation::validate(&candidate)?;
        let mut document = read_tui_config_document(&self.user_path)?;
        let profile_value = toml::Value::try_from(&profile)
            .map_err(|_| invalid("provider configuration could not serialize"))?;
        table(&mut document, &["providers"])?.insert(setup.provider.clone(), profile_value);
        if let Some(route) = route {
            table(&mut document, &["models", "aliases"])?.insert(
                alias,
                toml::Value::Array(route.into_iter().map(toml::Value::String).collect()),
            );
        }
        let encoded = toml::to_string_pretty(&document)
            .map_err(|_| invalid("configuration could not serialize"))?;
        persist_tui_provenance(
            parent,
            &self.user_path,
            &format!("providers.{}.kind", setup.provider),
            &profile.kind,
        )?;
        persist_tui_config_atomic(parent, &self.user_path, encoded.as_bytes(), "providers")?;
        self.load()
    }
}

fn table<'a>(
    document: &'a mut toml::Value,
    path: &[&str],
) -> Result<&'a mut toml::Table, ConfigError> {
    let mut value = document;
    for key in path {
        value = value
            .as_table_mut()
            .ok_or_else(invalid_table)?
            .entry((*key).to_owned())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    }
    value.as_table_mut().ok_or_else(invalid_table)
}
fn invalid_table() -> ConfigError {
    ConfigError::InvalidUserSetting {
        key: "providers".into(),
        reason: "configuration contains an incompatible table".into(),
    }
}
