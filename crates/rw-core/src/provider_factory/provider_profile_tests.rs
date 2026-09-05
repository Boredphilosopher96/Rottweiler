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
