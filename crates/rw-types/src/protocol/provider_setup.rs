//! Non-secret inputs for user-scoped compatible provider setup.
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS, Allocation)]
#[serde(rename_all = "snake_case")]
pub enum CompatibleProviderAdapter {
    Chat,
    Responses,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS, Allocation)]
#[serde(rename_all = "snake_case")]
pub enum CompatibleProviderAuth {
    ApiKey,
    None,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct CompatibleProviderSetup {
    #[schemars(length(min = 1, max = 128))]
    pub provider: String,
    pub adapter: CompatibleProviderAdapter,
    #[schemars(length(min = 1, max = 2048))]
    pub endpoint: String,
    pub auth: CompatibleProviderAuth,
    /// Optional static model for local endpoints that do not expose discovery.
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>", extend("minLength" = 1, "maxLength" = 256))]
    pub initial_model: Option<String>,
}

impl CompatibleProviderSetup {
    /// Checks transport bounds; the configuration owner also validates endpoint policy.
    ///
    /// # Errors
    /// Rejects invalid names and oversized, empty, or control-bearing input.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.provider.is_empty()
            || self.provider.len() > 128
            || !self
                .provider
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        {
            return Err(
                "provider name must be 1-128 ASCII letters, digits, dots, dashes, or underscores",
            );
        }
        if self.endpoint.is_empty()
            || self.endpoint.len() > 2048
            || self.endpoint.chars().any(char::is_control)
        {
            return Err("endpoint must be a bounded absolute URL");
        }
        if self.initial_model.as_ref().is_some_and(|model| {
            model.is_empty()
                || model.len() > 256
                || model.trim() != model
                || model.chars().any(char::is_control)
        }) {
            return Err("initial model must be a nonempty model ID of at most 256 bytes");
        }
        Ok(())
    }
}

/// Returns the stable vault identifier for a provider's primary API key.
///
/// # Errors
/// Rejects empty names and names outside the provider identifier alphabet.
pub fn default_provider_api_key_credential_id(provider: &str) -> Result<String, &'static str> {
    if provider.is_empty()
        || !provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err("provider name must contain only ASCII letters, digits, '.', '-', or '_'");
    }
    Ok(format!("providers.{provider}.api_key"))
}
