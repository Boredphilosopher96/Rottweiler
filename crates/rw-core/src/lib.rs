//! Headless Rottweiler agent engine.

mod admin;

pub use admin::{
    AdminError, DEFAULT_MODEL_CATALOG_URL, ModelCatalogRefresh, OAuthLogin, OAuthLoginResult,
    begin_oauth_login, refresh_model_catalog,
};

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "core";
