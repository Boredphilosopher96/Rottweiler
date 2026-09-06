use std::{collections::BTreeMap, sync::Arc, time::Duration};

use rw_providers::{
    AnthropicConfig, AnthropicProvider, AuthMaterial, CacheBreakpointSupport, NetworkPolicy,
    OpenAiChatRequestProfile, OpenAiCompatibleConfig, OpenAiCompatibleProvider, OpenAiWireMode,
    Provider, ProviderErrorKind, ProviderRequest, ProxyEnvironment, ProxySettings, StaticAuth,
    ToolChoice, deny_outbound_network_for_process, refresh_models_dev,
};
use rw_types::config::ThinkingLevel;
use tokio::{net::TcpListener, time::timeout};
use url::Url;

fn request() -> ProviderRequest {
    ProviderRequest {
        model: "network-denied-canary".to_owned(),
        turns: Vec::new(),
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 1,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    }
}

#[tokio::test]
async fn reasoning_only_openai_model_rejects_off_before_opening_a_socket() {
    // Adapter composition is local and must remain deterministic while a
    // concurrent replay/offline guard is active. Request validation still
    // runs before the guarded transport boundary.
    let _process_guard = deny_outbound_network_for_process();
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
        name: "reasoning-only".to_owned(),
        endpoint: Url::parse("http://127.0.0.1:9/v1/responses")
            .unwrap_or_else(|error| panic!("fixture endpoint must parse: {error}")),
        auth: Arc::new(StaticAuth::new(AuthMaterial::None)),
        proxy: None,
        proxy_authentication: None,
        network_policy: NetworkPolicy::Allow,
        wire_mode: OpenAiWireMode::Responses,
        chat_request_profile: OpenAiChatRequestProfile::OpenAi,
        tool_calling: false,
        cache_breakpoints: CacheBreakpointSupport::None,
        supported_reasoning_efforts: vec![ThinkingLevel::Low, ThinkingLevel::High],
        supports_vision: false,
        max_context_tokens: None,
        max_output_tokens: None,
        headers: BTreeMap::new(),
        header_credentials: BTreeMap::new(),
        extra_body: BTreeMap::new(),
        model_ids: BTreeMap::new(),
        path_template: None,
    })
    .unwrap_or_else(|error| panic!("reasoning-only adapter must construct: {error}"));

    let Err(error) = provider.stream(request()).await else {
        panic!("unsupported off/none effort must fail before transport");
    };
    assert_eq!(error.kind, ProviderErrorKind::Unsupported);
}

#[tokio::test]
async fn anthropic_thinking_without_strategy_fails_before_opening_a_socket() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("thinking canary listener must bind: {error}"));
    let endpoint = Url::parse(&format!(
        "http://{}/v1/messages",
        listener
            .local_addr()
            .unwrap_or_else(|error| panic!("thinking canary address must resolve: {error}"))
    ))
    .unwrap_or_else(|error| panic!("thinking canary endpoint must parse: {error}"));
    let provider = AnthropicProvider::new(AnthropicConfig {
        name: "anthropic-without-thinking".to_owned(),
        endpoint: endpoint.clone(),
        auth: Arc::new(StaticAuth::new(AuthMaterial::None)),
        proxy: None,
        proxy_authentication: None,
        network_policy: NetworkPolicy::Allow,
        thinking_strategy: None,
        max_context_tokens: None,
        max_output_tokens: None,
    })
    .unwrap_or_else(|error| panic!("Anthropic canary provider must construct: {error}"));
    let mut thinking_request = request();
    thinking_request.thinking = ThinkingLevel::Medium;

    let Err(error) = provider.stream(thinking_request).await else {
        panic!("missing Anthropic thinking strategy must fail before transport");
    };
    assert_eq!(error.kind, ProviderErrorKind::Unsupported);
    assert!(
        timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "unsupported Anthropic thinking unexpectedly opened a live socket"
    );
}

#[tokio::test]
async fn network_denied_prevents_both_live_adapters_from_opening_a_socket() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("canary listener must bind: {error}"));
    let endpoint = Url::parse(&format!(
        "http://{}/v1/provider",
        listener
            .local_addr()
            .unwrap_or_else(|error| panic!("canary listener must have an address: {error}"))
    ))
    .unwrap_or_else(|error| panic!("canary endpoint must parse: {error}"));

    let auth = Arc::new(StaticAuth::new(AuthMaterial::None));
    let anthropic = AnthropicProvider::new(AnthropicConfig {
        name: "anthropic-denied".to_owned(),
        endpoint: endpoint.clone(),
        auth: auth.clone(),
        proxy: None,
        proxy_authentication: None,
        network_policy: NetworkPolicy::Allow,
        thinking_strategy: None,
        max_context_tokens: None,
        max_output_tokens: None,
    })
    .unwrap_or_else(|error| panic!("Anthropic adapter must construct: {error}"));
    let openai = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
        name: "openai-denied".to_owned(),
        endpoint: endpoint.clone(),
        auth,
        proxy: None,
        proxy_authentication: None,
        network_policy: NetworkPolicy::Allow,
        wire_mode: OpenAiWireMode::Responses,
        chat_request_profile: OpenAiChatRequestProfile::OpenAi,
        tool_calling: false,
        cache_breakpoints: CacheBreakpointSupport::None,
        supported_reasoning_efforts: Vec::new(),
        supports_vision: false,
        max_context_tokens: None,
        max_output_tokens: None,
        headers: BTreeMap::new(),
        header_credentials: BTreeMap::new(),
        extra_body: BTreeMap::new(),
        model_ids: BTreeMap::new(),
        path_template: None,
    })
    .unwrap_or_else(|error| panic!("OpenAI adapter must construct: {error}"));

    let _process_guard = deny_outbound_network_for_process();
    for provider in [&anthropic as &dyn Provider, &openai as &dyn Provider] {
        let Err(error) = provider.discover_models().await else {
            panic!("network-denied model discovery must reject before transport");
        };
        assert_eq!(error.kind, ProviderErrorKind::NetworkDisabled);
        let Err(error) = provider.stream(request()).await else {
            panic!("network-denied adapter must reject before transport");
        };
        assert_eq!(error.kind, ProviderErrorKind::NetworkDisabled);
    }

    let output = std::env::temp_dir().join("rottweiler-network-denied-models.toml");
    let proxies = ProxySettings {
        global: None,
        per_provider: BTreeMap::new(),
        environment: ProxyEnvironment::default(),
    };
    let Err(error) = refresh_models_dev(endpoint.as_str(), &output, &proxies).await else {
        panic!("process guard must reject model-catalog networking");
    };
    assert_eq!(error.kind, ProviderErrorKind::NetworkDisabled);

    assert!(
        timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "network-denied adapters unexpectedly opened a live socket"
    );
}
