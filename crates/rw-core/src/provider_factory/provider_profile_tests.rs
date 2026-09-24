use super::catalog::provider_auth_kind;
use super::*;

#[test]
fn built_in_profiles_own_canonical_setup_metadata() {
    let expected = [
        (
            BuiltinProviderId::Anthropic,
            "anthropic",
            AdapterKind::Anthropic,
            ProviderAuthKind::ApiKey,
        ),
        (
            BuiltinProviderId::OpenAi,
            "openai",
            AdapterKind::OpenAiResponses,
            ProviderAuthKind::ApiKey,
        ),
        (
            BuiltinProviderId::OpenAiCodex,
            "openai_codex",
            AdapterKind::OpenAiSubscription,
            ProviderAuthKind::Oauth,
        ),
        (
            BuiltinProviderId::GitHubCopilot,
            "github_copilot",
            AdapterKind::GitHubCopilot,
            ProviderAuthKind::DeviceFlow,
        ),
    ];

    for (id, canonical_id, adapter_kind, auth_kind) in expected {
        let profile = id.profile();
        assert_eq!(BuiltinProviderId::parse(canonical_id), Some(id));
        assert_eq!(
            BuiltinProviderId::from_config(canonical_id, profile.config_kind()),
            Some(id)
        );
        assert_eq!(profile.id(), id);
        assert_eq!(profile.canonical_id(), canonical_id);
        assert_eq!(profile.config_kind(), canonical_id);
        assert_eq!(profile.adapter_kind(), adapter_kind);
        assert_eq!(profile.onboarding_auth_kind(), auth_kind);
        assert!(profile.setup_exposed());
    }
}

#[test]
fn custom_adapter_kinds_do_not_become_built_in_provider_ids() {
    for (kind, adapter) in [
        ("openai_chat", AdapterKind::OpenAiChat),
        (
            "openai_compatible_responses",
            AdapterKind::OpenAiCompatibleResponses,
        ),
        ("openai_compatible", AdapterKind::OpenAiCompatibleChat),
    ] {
        assert_eq!(AdapterKind::from_config_kind(kind), Some(adapter));
        assert_eq!(BuiltinProviderId::parse(kind), None);
    }
    assert_eq!(BuiltinProviderId::parse("custom"), None);
    assert_eq!(
        BuiltinProviderId::from_config("openai", "openai_chat"),
        None
    );
}

#[test]
fn custom_provider_auth_remains_config_driven() {
    let mut config = Config::default();
    config.providers.insert(
        "company_gateway".to_owned(),
        ProviderConfig {
            kind: "openai_compatible".to_owned(),
            oauth_token_env: Some("COMPANY_GATEWAY_TOKEN".to_owned()),
            ..ProviderConfig::default()
        },
    );

    assert_eq!(
        provider_auth_kind(&config, "company_gateway"),
        ProviderAuthKind::Oauth
    );
    assert_eq!(BuiltinProviderId::parse("company_gateway"), None);
}

#[test]
fn explicit_no_catalog_projects_configured_local_models_with_concrete_routes()
-> Result<(), Box<dyn std::error::Error>> {
    let mut config = Config::default();
    config.providers.insert(
        "local".into(),
        ProviderConfig {
            kind: "openai_compatible".into(),
            base_url: Some("http://127.0.0.1:11434/v1/chat/completions".into()),
            auth_scheme: Some(rw_types::config::ProviderAuthScheme::None),
            ..ProviderConfig::default()
        },
    );
    config
        .models
        .aliases
        .insert("local-model".into(), vec!["local/qwen/test".into()]);
    let catalog = super::catalog::configured_local_catalog(&config, "local")?;
    assert_eq!(catalog.models[0].id, "qwen/test");
    let snapshot = super::catalog::project_model_catalog(
        &config,
        &PricingTable::default(),
        vec![(
            "local".into(),
            "local/catalog-discovery".into(),
            true,
            Ok(catalog),
        )],
    );
    assert_eq!(snapshot.models[0].id, "local/qwen/test");
    assert!(snapshot.models[0].available);
    assert_eq!(
        snapshot
            .providers
            .iter()
            .find(|provider| provider.name == "local")
            .ok_or("missing local descriptor")?
            .auth_kind,
        ProviderAuthKind::None
    );
    config
        .providers
        .get_mut("local")
        .ok_or("missing local provider")?
        .base_url = Some("https://remote.example/v1/chat/completions".into());
    assert!(super::catalog::configured_local_catalog(&config, "local").is_err());
    Ok(())
}
