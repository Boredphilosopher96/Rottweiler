use std::sync::Arc;

use rw_providers::{
    AnthropicConfig, AnthropicProvider, AuthMaterial, CacheBreakpointSupport, NetworkPolicy,
    OpenAiCompatibleConfig, OpenAiCompatibleProvider, OpenAiWireMode, Provider, Secret, StaticAuth,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

async fn fixture_server(bodies: Vec<String>) -> (Url, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("fixture listener must bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("fixture address must resolve: {error}"));
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut stream, _) = listener
                .accept()
                .await
                .unwrap_or_else(|error| panic!("fixture request must connect: {error}"));
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream
                    .read(&mut chunk)
                    .await
                    .unwrap_or_else(|error| panic!("fixture request must read: {error}"));
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
            }
            requests.push(String::from_utf8_lossy(&request).to_ascii_lowercase());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .unwrap_or_else(|error| panic!("fixture response must write: {error}"));
        }
        requests
    });
    (
        Url::parse(&format!("http://{address}/"))
            .unwrap_or_else(|error| panic!("fixture URL must parse: {error}")),
        task,
    )
}

fn openai_provider(endpoint: Url, auth: AuthMaterial) -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
        name: "openai-fixture".to_owned(),
        endpoint,
        auth: Arc::new(StaticAuth::new(auth)),
        proxy: None,
        proxy_authentication: None,
        network_policy: NetworkPolicy::Allow,
        wire_mode: OpenAiWireMode::Responses,
        tool_calling: true,
        cache_breakpoints: CacheBreakpointSupport::Automatic,
        supported_reasoning_efforts: Vec::new(),
        supports_vision: true,
        max_context_tokens: None,
        max_output_tokens: None,
    })
    .unwrap_or_else(|error| panic!("OpenAI fixture provider must build: {error}"))
}

#[tokio::test]
async fn openai_discovers_sorted_models_with_auth() {
    let (origin, server) = fixture_server(vec![
        r#"{"object":"list","data":[{"id":"z-model"},{"id":"a-model"},{"broken":true},{"id":"a-model"}]}"#.to_owned(),
    ])
    .await;
    let provider = openai_provider(
        origin
            .join("v1/responses")
            .unwrap_or_else(|error| panic!("fixture endpoint must join: {error}")),
        AuthMaterial::Bearer(Secret::new("PRIVATE-OPENAI-TOKEN")),
    );
    let catalog = provider
        .discover_models()
        .await
        .unwrap_or_else(|error| panic!("OpenAI discovery must work: {error}"))
        .unwrap_or_else(|| panic!("OpenAI must expose discovery"));
    assert_eq!(
        catalog
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["a-model", "z-model"]
    );
    let requests = server
        .await
        .unwrap_or_else(|error| panic!("fixture server must finish: {error}"));
    assert!(requests[0].starts_with("get /v1/models http/1.1"));
    assert!(requests[0].contains("authorization: bearer private-openai-token"));
}

#[tokio::test]
async fn chatgpt_discovers_visible_models_from_tolerant_envelope() {
    let (origin, server) = fixture_server(vec![
        r#"{"models":[{"slug":"gpt-visible","display_name":"GPT Visible","description":"Coding model","visibility":"list","context_window":272000,"max_output_tokens":32000,"input_modalities":["text","image"],"supported_reasoning_levels":[{"effort":"low"}]},{"slug":"gpt-hidden","visibility":"hide"},{"future":true}]}"#.to_owned(),
    ])
    .await;
    let provider = openai_provider(
        origin
            .join("backend-api/codex/responses")
            .unwrap_or_else(|error| panic!("fixture endpoint must join: {error}")),
        AuthMaterial::OpenAiSubscription {
            access_token: Secret::new("PRIVATE-SUBSCRIPTION-TOKEN"),
            account_id: Secret::new("PRIVATE-ACCOUNT"),
            originator: "rottweiler".to_owned(),
            user_agent: "rottweiler/test".to_owned(),
            session_id: "fixture-session".to_owned(),
        },
    );
    let catalog = provider
        .discover_models()
        .await
        .unwrap_or_else(|error| panic!("ChatGPT discovery must work: {error}"))
        .unwrap_or_else(|| panic!("ChatGPT must expose discovery"));
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(catalog.models[0].id, "gpt-visible");
    let capabilities = catalog.models[0]
        .capabilities
        .as_ref()
        .unwrap_or_else(|| panic!("ChatGPT metadata must expose capabilities"));
    assert_eq!(capabilities.max_context_tokens, Some(272_000));
    assert_eq!(capabilities.max_output_tokens, Some(32_000));
    assert!(capabilities.vision);
    assert!(capabilities.thinking);
    let requests = server
        .await
        .unwrap_or_else(|error| panic!("fixture server must finish: {error}"));
    assert!(requests[0].starts_with(&format!(
        "get /backend-api/codex/models?client_version={} http/1.1",
        env!("CARGO_PKG_VERSION")
    )));
    assert!(requests[0].contains("chatgpt-account-id: private-account"));
}

#[tokio::test]
async fn anthropic_follows_bounded_cursor_pagination() {
    let (origin, server) = fixture_server(vec![
        r#"{"data":[{"type":"model","id":"claude-z","display_name":"Claude Z"}],"has_more":true,"last_id":"claude-z"}"#.to_owned(),
        r#"{"data":[{"type":"model","id":"claude-a","display_name":"Claude A"},{"type":"future","id":"ignored"}],"has_more":false,"last_id":"claude-a"}"#.to_owned(),
    ])
    .await;
    let provider = AnthropicProvider::new(AnthropicConfig {
        name: "anthropic-fixture".to_owned(),
        endpoint: origin
            .join("v1/messages")
            .unwrap_or_else(|error| panic!("fixture endpoint must join: {error}")),
        auth: Arc::new(StaticAuth::new(AuthMaterial::ApiKey(Secret::new(
            "PRIVATE-ANTHROPIC-KEY",
        )))),
        proxy: None,
        proxy_authentication: None,
        network_policy: NetworkPolicy::Allow,
        thinking_strategy: None,
        max_context_tokens: None,
        max_output_tokens: None,
    })
    .unwrap_or_else(|error| panic!("Anthropic fixture provider must build: {error}"));
    let catalog = provider
        .discover_models()
        .await
        .unwrap_or_else(|error| panic!("Anthropic discovery must work: {error}"))
        .unwrap_or_else(|| panic!("Anthropic must expose discovery"));
    assert_eq!(
        catalog
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["claude-a", "claude-z"]
    );
    let requests = server
        .await
        .unwrap_or_else(|error| panic!("fixture server must finish: {error}"));
    assert!(requests[0].starts_with("get /v1/models?limit=100 http/1.1"));
    assert!(requests[1].starts_with("get /v1/models?limit=100&after_id=claude-z http/1.1"));
    for request in requests {
        assert!(request.contains("x-api-key: private-anthropic-key"));
        assert!(request.contains("anthropic-version: 2023-06-01"));
    }
}
