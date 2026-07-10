//! Headless Rottweiler agent engine.

mod admin;
mod copilot_credentials;
mod provider_factory;
mod subscription_credentials;

pub use admin::{
    AdminError, DEFAULT_MODEL_CATALOG_URL, GitHubCopilotLogin, GitHubCopilotLoginResult,
    ModelCatalogRefresh, OAuthLogin, OAuthLoginResult, ProviderApiKey, ProviderLogin,
    ProviderLoginCancellation, ResolvedProviderApiKey, begin_oauth_login, begin_provider_login,
    default_provider_api_key_credential_id, refresh_model_catalog, resolve_provider_api_key,
    store_provider_api_key,
};
pub use provider_factory::{ProviderFactory, ProviderFactoryError, ProviderRuntime, ResolvedModel};
pub use rw_providers::{ProviderModelMetadata, UsageAccounting as ModelAccounting};

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "core";
