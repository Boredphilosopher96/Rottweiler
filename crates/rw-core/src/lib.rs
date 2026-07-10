//! Headless Rottweiler agent engine.

mod admin;
mod provider_factory;

pub use admin::{
    AdminError, DEFAULT_MODEL_CATALOG_URL, ModelCatalogRefresh, OAuthLogin, OAuthLoginResult,
    ProviderApiKey, ResolvedProviderApiKey, begin_oauth_login,
    default_provider_api_key_credential_id, refresh_model_catalog, resolve_provider_api_key,
    store_provider_api_key,
};
pub use provider_factory::{ProviderFactory, ProviderFactoryError, ProviderRuntime, ResolvedModel};

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "core";
