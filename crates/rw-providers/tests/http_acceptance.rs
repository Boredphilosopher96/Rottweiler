use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use rw_providers::{
    AnthropicConfig, AnthropicProvider, AuthMaterial, CacheBreakpointSupport, FinishReason,
    FixtureRedactor, NetworkPolicy, OpenAiChatRequestProfile, OpenAiCompatibleConfig,
    OpenAiCompatibleProvider, OpenAiWireMode, PricingTable, Provider, ProviderError,
    ProviderErrorKind, ProviderEvent, ProviderRequest, ProviderRouter, ProxyAuthentication,
    ProxyEnvironment, ProxySettings, Recorder, ReplayProvider, RetryPolicy, Secret, StaticAuth,
    ThinkingLevel, TokenUsage, ToolChoice, ToolDefinition,
};
use rw_types::{Block, Role, Turn, TurnMeta};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use url::Url;

const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-test\",\"usage\":{\"input_tokens\":11,\"cache_read_input_tokens\":3,\"cache_creation_input_tokens\":2}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-anthropic\",\"name\":\"read_file\",\"input\":{}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"src/lib.rs\\\"}\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

const OPENAI_SSE: &str = concat!(
    "data: {\"id\":\"chat-1\",\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-openai\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chat-1\",\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"src/lib.rs\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"id\":\"chat-1\",\"model\":\"gpt-test\",\"choices\":[],\"usage\":{\"prompt_tokens\":13,\"completion_tokens\":7,\"prompt_tokens_details\":{\"cached_tokens\":2},\"completion_tokens_details\":{\"reasoning_tokens\":1}}}\n\n",
    "data: [DONE]\n\n",
);

const OPENAI_RESPONSES_SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-1\",\"model\":\"gpt-responses-test\"}}\n\n",
    "event: response.output_item.added\n",
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc-1\",\"call_id\":\"call-responses\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
    "event: response.function_call_arguments.delta\n",
    "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"path\\\":\"}\n\n",
    "event: response.function_call_arguments.delta\n",
    "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"\\\"src/main.rs\\\"}\"}\n\n",
    "event: response.function_call_arguments.done\n",
    "data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":0,\"arguments\":\"{\\\"path\\\":\\\"src/main.rs\\\"}\"}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"usage\":{\"input_tokens\":17,\"output_tokens\":6,\"input_tokens_details\":{\"cached_tokens\":4},\"output_tokens_details\":{\"reasoning_tokens\":2}}}}\n\n",
);

const OPENAI_PARTIAL_THEN_ERROR_SSE: &str = concat!(
    "data: {\"id\":\"chat-partial\",\"model\":\"gpt-primary\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
    "data: {\"error\":{\"type\":\"server_error\",\"message\":\"fixture failure\"}}\n\n",
);

#[derive(Debug)]
struct CapturedRequest {
    request_line: String,
    body: Vec<u8>,
    raw: Vec<u8>,
}

struct TestServer {
    endpoint: Url,
    task: JoinHandle<Vec<CapturedRequest>>,
}

struct TestProxy {
    url: Url,
    task: JoinHandle<Vec<CapturedRequest>>,
}

struct ObservedServer {
    endpoint: Url,
    requests: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

fn request() -> ProviderRequest {
    ProviderRequest {
        model: "ignored-by-router".to_owned(),
        turns: vec![Turn {
            role: Role::User,
            blocks: vec![Block::Text {
                text: "Inspect the source".to_owned(),
            }],
            meta: TurnMeta::default(),
        }],
        tools: vec![ToolDefinition {
            name: "read_file".to_owned(),
            description: "Read a UTF-8 file".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }),
        }],
        tool_choice: ToolChoice::Auto,
        max_output_tokens: 128,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    }
}

fn anthropic_provider(name: &str, endpoint: Url, proxy: Option<Url>) -> Arc<dyn Provider> {
    Arc::new(
        AnthropicProvider::new(AnthropicConfig {
            name: name.to_owned(),
            endpoint,
            auth: Arc::new(StaticAuth::new(AuthMaterial::None)),
            proxy,
            proxy_authentication: None,
            network_policy: NetworkPolicy::Allow,
            thinking_strategy: None,
            max_context_tokens: None,
            max_output_tokens: None,
        })
        .unwrap_or_else(|error| panic!("Anthropic fixture provider must build: {error}")),
    )
}

fn openai_provider(name: &str, endpoint: Url, proxy: Option<Url>) -> Arc<dyn Provider> {
    openai_provider_with_options(name, endpoint, proxy, None, OpenAiWireMode::ChatCompletions)
}

fn openai_provider_with_options(
    name: &str,
    endpoint: Url,
    proxy: Option<Url>,
    proxy_authentication: Option<ProxyAuthentication>,
    wire_mode: OpenAiWireMode,
) -> Arc<dyn Provider> {
    Arc::new(
        OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
            name: name.to_owned(),
            endpoint,
            auth: Arc::new(StaticAuth::new(AuthMaterial::None)),
            proxy,
            proxy_authentication,
            network_policy: NetworkPolicy::Allow,
            wire_mode,
            chat_request_profile: OpenAiChatRequestProfile::OpenAi,
            tool_calling: true,
            cache_breakpoints: CacheBreakpointSupport::Automatic,
            supported_reasoning_efforts: vec![
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
            supports_vision: true,
            max_context_tokens: None,
            max_output_tokens: None,
            headers: BTreeMap::new(),
            header_credentials: BTreeMap::new(),
            extra_body: BTreeMap::new(),
            model_ids: BTreeMap::new(),
            path_template: None,
        })
        .unwrap_or_else(|error| panic!("OpenAI fixture provider must build: {error}")),
    )
}

async fn collect(
    provider: &dyn Provider,
    provider_request: ProviderRequest,
) -> Vec<Result<ProviderEvent, ProviderError>> {
    provider
        .stream(provider_request)
        .await
        .unwrap_or_else(|error| panic!("fixture stream must start: {error}"))
        .collect()
        .await
}

fn anthropic_expected() -> Vec<Result<ProviderEvent, ProviderError>> {
    vec![
        Ok(ProviderEvent::MessageStart {
            model: "claude-test".to_owned(),
        }),
        Ok(ProviderEvent::Usage {
            usage: TokenUsage {
                input_tokens: 11,
                output_tokens: 0,
                cache_read_tokens: 3,
                cache_write_tokens: 2,
                reasoning_tokens: 0,
            },
        }),
        Ok(ProviderEvent::ToolCallStart {
            id: "call-anthropic".to_owned(),
            name: "read_file".to_owned(),
        }),
        Ok(ProviderEvent::ToolCallArgumentsDelta {
            id: "call-anthropic".to_owned(),
            json_fragment: "{\"path\":".to_owned(),
        }),
        Ok(ProviderEvent::ToolCallArgumentsDelta {
            id: "call-anthropic".to_owned(),
            json_fragment: "\"src/lib.rs\"}".to_owned(),
        }),
        Ok(ProviderEvent::ToolCallEnd {
            id: "call-anthropic".to_owned(),
            arguments: json!({"path": "src/lib.rs"}),
        }),
        Ok(ProviderEvent::Usage {
            usage: TokenUsage {
                input_tokens: 11,
                output_tokens: 5,
                cache_read_tokens: 3,
                cache_write_tokens: 2,
                reasoning_tokens: 0,
            },
        }),
        Ok(ProviderEvent::Finished {
            reason: FinishReason::ToolCalls,
        }),
    ]
}

fn openai_expected() -> Vec<Result<ProviderEvent, ProviderError>> {
    vec![
        Ok(ProviderEvent::MessageStart {
            model: "gpt-test".to_owned(),
        }),
        Ok(ProviderEvent::ToolCallStart {
            id: "call-openai".to_owned(),
            name: "read_file".to_owned(),
        }),
        Ok(ProviderEvent::ToolCallArgumentsDelta {
            id: "call-openai".to_owned(),
            json_fragment: "{\"path\":".to_owned(),
        }),
        Ok(ProviderEvent::ToolCallArgumentsDelta {
            id: "call-openai".to_owned(),
            json_fragment: "\"src/lib.rs\"}".to_owned(),
        }),
        Ok(ProviderEvent::Usage {
            usage: TokenUsage {
                input_tokens: 11,
                output_tokens: 6,
                cache_read_tokens: 2,
                cache_write_tokens: 0,
                reasoning_tokens: 1,
            },
        }),
        Ok(ProviderEvent::ToolCallEnd {
            id: "call-openai".to_owned(),
            arguments: json!({"path": "src/lib.rs"}),
        }),
        Ok(ProviderEvent::Finished {
            reason: FinishReason::ToolCalls,
        }),
    ]
}

fn openai_responses_expected() -> Vec<Result<ProviderEvent, ProviderError>> {
    vec![
        Ok(ProviderEvent::MessageStart {
            model: "gpt-responses-test".to_owned(),
        }),
        Ok(ProviderEvent::ToolCallStart {
            id: "call-responses".to_owned(),
            name: "read_file".to_owned(),
        }),
        Ok(ProviderEvent::ToolCallArgumentsDelta {
            id: "call-responses".to_owned(),
            json_fragment: "{\"path\":".to_owned(),
        }),
        Ok(ProviderEvent::ToolCallArgumentsDelta {
            id: "call-responses".to_owned(),
            json_fragment: "\"src/main.rs\"}".to_owned(),
        }),
        Ok(ProviderEvent::ToolCallEnd {
            id: "call-responses".to_owned(),
            arguments: json!({"path": "src/main.rs"}),
        }),
        Ok(ProviderEvent::Usage {
            usage: TokenUsage {
                input_tokens: 13,
                output_tokens: 4,
                cache_read_tokens: 4,
                cache_write_tokens: 0,
                reasoning_tokens: 2,
            },
        }),
        Ok(ProviderEvent::Finished {
            reason: FinishReason::ToolCalls,
        }),
    ]
}

#[tokio::test]
async fn anthropic_http_tool_stream_records_and_replays_byte_identically() {
    let server = spawn_sse_origin("/v1/messages", ANTHROPIC_SSE, 1).await;
    let provider = anthropic_provider("anthropic", server.endpoint.clone(), None);
    let directory = unique_temp_directory("anthropic");
    let recorder = Recorder::new(provider, &directory, FixtureRedactor::default());

    let live = collect(&recorder, request()).await;
    assert_eq!(live, anthropic_expected());
    let captured = join_requests(server.task).await;
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].request_line, "POST /v1/messages HTTP/1.1");

    // The only listener is closed before replay. A passing replay therefore
    // proves this path reads the fixture without making a provider request.
    let replay_provider = ReplayProvider::load("anthropic", &directory)
        .await
        .unwrap_or_else(|error| panic!("Anthropic replay provider must load: {error}"));
    let replay = collect(&replay_provider, request()).await;
    assert_serialized_bytes_equal(&live, &replay);
    remove_temp_directory(directory).await;
}

#[tokio::test]
async fn openai_chat_http_tool_stream_records_and_replays_byte_identically() {
    let server = spawn_sse_origin("/v1/chat/completions", OPENAI_SSE, 1).await;
    let provider = openai_provider("openai", server.endpoint.clone(), None);
    let directory = unique_temp_directory("openai");
    let recorder = Recorder::new(provider, &directory, FixtureRedactor::default());

    let live = collect(&recorder, request()).await;
    assert_eq!(live, openai_expected());
    let captured = join_requests(server.task).await;
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].request_line,
        "POST /v1/chat/completions HTTP/1.1"
    );

    let replay_provider = ReplayProvider::load("openai", &directory)
        .await
        .unwrap_or_else(|error| panic!("OpenAI replay provider must load: {error}"));
    let replay = collect(&replay_provider, request()).await;
    assert_serialized_bytes_equal(&live, &replay);
    let usage = replay
        .iter()
        .filter_map(|event| match event {
            Ok(ProviderEvent::Usage { usage }) => Some(*usage),
            _ => None,
        })
        .next_back()
        .unwrap_or_else(|| panic!("recorded stream must contain terminal usage"));
    let pricing = PricingTable::from_toml(
        r#"
source_url = "https://models.test/catalog"
snapshot_date = "2026-07-10"
revision = "recorded-cost-fixture"
[models."openai/gpt-test"]
input_per_million_micros_usd = 3000001
output_per_million_micros_usd = 15000003
cache_read_per_million_micros_usd = 300007
reasoning_per_million_micros_usd = 1500000
"#,
    )
    .unwrap_or_else(|error| panic!("pricing fixture must parse: {error}"));
    let cost = pricing
        .cost("openai/gpt-test", usage)
        .unwrap_or_else(|error| panic!("recorded usage cost must fit: {error}"))
        .unwrap_or_else(|| panic!("recorded model price must exist"));
    assert_eq!(cost.input_micros_usd, 33);
    assert_eq!(cost.output_micros_usd, 90);
    assert_eq!(cost.cache_read_micros_usd, 1);
    assert_eq!(cost.reasoning_micros_usd, 2);
    assert_eq!(cost.total_micros_usd, 126);
    remove_temp_directory(directory).await;
}

#[tokio::test]
async fn openai_responses_http_tool_stream_records_and_replays_raw_frames() {
    let server = spawn_sse_origin("/v1/responses", OPENAI_RESPONSES_SSE, 1).await;
    let provider = openai_provider_with_options(
        "openai-responses",
        server.endpoint.clone(),
        None,
        None,
        OpenAiWireMode::Responses,
    );
    let directory = unique_temp_directory("openai-responses");
    let recorder = Recorder::new(provider, &directory, FixtureRedactor::default());

    let live = collect(&recorder, request()).await;
    assert_eq!(live, openai_responses_expected());
    assert!(matches!(
        live.last(),
        Some(Ok(ProviderEvent::Finished {
            reason: FinishReason::ToolCalls
        }))
    ));
    let captured = join_requests(server.task).await;
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].request_line, "POST /v1/responses HTTP/1.1");
    let request_body: serde_json::Value = serde_json::from_slice(&captured[0].body)
        .unwrap_or_else(|error| panic!("Responses request JSON must parse: {error}"));
    assert_eq!(request_body["tools"][0]["type"], "function");

    let fixture = read_fixture_text(&directory).await;
    assert!(fixture.contains("response.function_call_arguments.delta"));
    assert!(fixture.contains("raw_sse"));
    let replay_provider = ReplayProvider::load("openai-responses", &directory)
        .await
        .unwrap_or_else(|error| panic!("OpenAI Responses replay must load: {error}"));
    let replay = collect(&replay_provider, request()).await;
    assert_serialized_bytes_equal(&live, &replay);
    remove_temp_directory(directory).await;
}

#[tokio::test]
async fn authenticated_proxy_receives_basic_header_without_secret_leakage() {
    const PASSWORD_CANARY: &str = "proxy-password-canary";
    const EXPECTED_AUTHORIZATION: &str =
        "proxy-authorization: Basic cHJveHktdXNlcjpwcm94eS1wYXNzd29yZC1jYW5hcnk=";

    let origin = spawn_sse_origin("/authenticated-proxy", OPENAI_SSE, 1).await;
    let proxy = spawn_forward_proxy(1).await;
    let authentication = ProxyAuthentication::new("proxy-user", Secret::new(PASSWORD_CANARY));
    let provider = openai_provider_with_options(
        "openai-proxied",
        origin.endpoint.clone(),
        Some(proxy.url),
        Some(authentication.clone()),
        OpenAiWireMode::ChatCompletions,
    );
    assert!(!format!("{authentication:?}").contains(PASSWORD_CANARY));
    let directory = unique_temp_directory("authenticated-proxy");
    let recorder = Recorder::new(provider, &directory, FixtureRedactor::default());

    assert_eq!(collect(&recorder, request()).await, openai_expected());
    let proxy_requests = join_requests(proxy.task).await;
    assert_eq!(proxy_requests.len(), 1);
    let raw = String::from_utf8_lossy(&proxy_requests[0].raw).to_ascii_lowercase();
    assert!(raw.contains(&EXPECTED_AUTHORIZATION.to_ascii_lowercase()));
    assert!(!raw.contains(PASSWORD_CANARY));
    assert_eq!(join_requests(origin.task).await.len(), 1);
    let fixture = read_fixture_text(&directory).await;
    assert!(!fixture.contains(PASSWORD_CANARY));

    let Err(error) = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
        name: "invalid-proxy-auth".to_owned(),
        endpoint: parse_url("http://127.0.0.1/unused"),
        auth: Arc::new(StaticAuth::new(AuthMaterial::None)),
        proxy: None,
        proxy_authentication: Some(authentication),
        network_policy: NetworkPolicy::Deny,
        wire_mode: OpenAiWireMode::Responses,
        chat_request_profile: OpenAiChatRequestProfile::OpenAi,
        tool_calling: true,
        cache_breakpoints: CacheBreakpointSupport::Automatic,
        supported_reasoning_efforts: vec![ThinkingLevel::Off, ThinkingLevel::Low],
        supports_vision: true,
        max_context_tokens: None,
        max_output_tokens: None,
        headers: BTreeMap::new(),
        header_credentials: BTreeMap::new(),
        extra_body: BTreeMap::new(),
        model_ids: BTreeMap::new(),
        path_template: None,
    }) else {
        panic!("authentication without a proxy URL must fail closed");
    };
    assert!(!format!("{error:?}").contains(PASSWORD_CANARY));
    assert!(!error.to_string().contains(PASSWORD_CANARY));
    remove_temp_directory(directory).await;
}

#[tokio::test]
async fn openai_capabilities_reject_unsupported_requests_before_network() {
    let server = spawn_observed_sse_origin("/must-not-run", OPENAI_SSE).await;
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
        name: "limited-openai-compatible".to_owned(),
        endpoint: server.endpoint,
        auth: Arc::new(StaticAuth::new(AuthMaterial::None)),
        proxy: None,
        proxy_authentication: None,
        network_policy: NetworkPolicy::Allow,
        wire_mode: OpenAiWireMode::Responses,
        chat_request_profile: OpenAiChatRequestProfile::OpenAi,
        tool_calling: false,
        cache_breakpoints: CacheBreakpointSupport::Explicit,
        supported_reasoning_efforts: vec![ThinkingLevel::Off, ThinkingLevel::Low],
        supports_vision: false,
        max_context_tokens: Some(1_024),
        max_output_tokens: Some(256),
        headers: BTreeMap::new(),
        header_credentials: BTreeMap::new(),
        extra_body: BTreeMap::new(),
        model_ids: BTreeMap::new(),
        path_template: None,
    })
    .unwrap_or_else(|error| panic!("limited fixture provider must build: {error}"));
    let capabilities = provider.capabilities();
    assert!(!capabilities.tool_calling);
    assert!(capabilities.thinking);
    assert_eq!(
        capabilities.cache_breakpoints,
        CacheBreakpointSupport::Explicit
    );

    let mut unsupported_reasoning = request();
    unsupported_reasoning.tools.clear();
    unsupported_reasoning.thinking = ThinkingLevel::High;
    let Err(error) = provider.stream(unsupported_reasoning).await else {
        panic!("unsupported reasoning effort must fail before streaming");
    };
    assert_eq!(error.kind, ProviderErrorKind::Unsupported);

    let mut unsupported_tools = request();
    unsupported_tools.thinking = ThinkingLevel::Off;
    let Err(error) = provider.stream(unsupported_tools).await else {
        panic!("unsupported tool capability must fail before streaming");
    };
    assert_eq!(error.kind, ProviderErrorKind::Unsupported);
    assert_eq!(server.requests.load(Ordering::SeqCst), 0);
    server.task.abort();
}

#[tokio::test]
async fn global_forward_proxy_sees_both_provider_families() {
    let anthropic_origin = spawn_sse_origin("/anthropic", ANTHROPIC_SSE, 1).await;
    let openai_origin = spawn_sse_origin("/openai", OPENAI_SSE, 1).await;
    let proxy = spawn_forward_proxy(2).await;
    let settings = ProxySettings {
        global: Some(proxy.url.clone()),
        per_provider: BTreeMap::new(),
        environment: ProxyEnvironment::default(),
    };

    let anthropic = anthropic_provider(
        "anthropic",
        anthropic_origin.endpoint.clone(),
        resolved_proxy(&settings, "anthropic", &anthropic_origin.endpoint),
    );
    let openai = openai_provider(
        "openai",
        openai_origin.endpoint.clone(),
        resolved_proxy(&settings, "openai", &openai_origin.endpoint),
    );
    assert_eq!(
        collect(anthropic.as_ref(), request()).await,
        anthropic_expected()
    );
    assert_eq!(collect(openai.as_ref(), request()).await, openai_expected());

    let proxy_requests = join_requests(proxy.task).await;
    let request_lines = proxy_requests
        .iter()
        .map(|request| request.request_line.as_str())
        .collect::<Vec<_>>();
    assert_eq!(request_lines.len(), 2);
    assert!(request_lines.iter().any(|line| line.contains("/anthropic")));
    assert!(request_lines.iter().any(|line| line.contains("/openai")));
    assert_eq!(join_requests(anthropic_origin.task).await.len(), 1);
    assert_eq!(join_requests(openai_origin.task).await.len(), 1);
}

#[tokio::test]
async fn per_provider_proxy_sees_only_the_selected_provider() {
    let anthropic_origin = spawn_sse_origin("/anthropic-only", ANTHROPIC_SSE, 1).await;
    let openai_origin = spawn_sse_origin("/openai-direct", OPENAI_SSE, 1).await;
    let proxy = spawn_forward_proxy(1).await;
    let settings = ProxySettings {
        global: None,
        per_provider: BTreeMap::from([("anthropic".to_owned(), proxy.url.clone())]),
        environment: ProxyEnvironment::default(),
    };

    let anthropic = anthropic_provider(
        "anthropic",
        anthropic_origin.endpoint.clone(),
        resolved_proxy(&settings, "anthropic", &anthropic_origin.endpoint),
    );
    let openai = openai_provider(
        "openai",
        openai_origin.endpoint.clone(),
        resolved_proxy(&settings, "openai", &openai_origin.endpoint),
    );
    assert_eq!(
        collect(anthropic.as_ref(), request()).await,
        anthropic_expected()
    );
    assert_eq!(collect(openai.as_ref(), request()).await, openai_expected());

    let proxy_requests = join_requests(proxy.task).await;
    assert_eq!(proxy_requests.len(), 1);
    assert!(proxy_requests[0].request_line.contains("/anthropic-only"));
    assert!(!proxy_requests[0].request_line.contains("/openai-direct"));
    assert_eq!(join_requests(anthropic_origin.task).await.len(), 1);
    assert_eq!(join_requests(openai_origin.task).await.len(), 1);
}

#[tokio::test]
async fn unreachable_primary_routes_to_live_http_fallback() {
    let killed_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("killed-provider fixture must bind: {error}"));
    let killed_address = killed_listener
        .local_addr()
        .unwrap_or_else(|error| panic!("killed-provider address must resolve: {error}"));
    drop(killed_listener);

    let fallback_server = spawn_sse_origin("/fallback", OPENAI_SSE, 1).await;
    let primary = openai_provider(
        "primary",
        parse_url(&format!("http://{killed_address}/dead")),
        None,
    );
    let fallback = openai_provider("fallback", fallback_server.endpoint.clone(), None);
    let router = ProviderRouter::new(
        BTreeMap::from([(
            "fast".to_owned(),
            vec![
                "primary/unreachable-model".to_owned(),
                "fallback/live-model".to_owned(),
            ],
        )]),
        vec![primary, fallback],
        one_attempt_retry_policy(),
    )
    .unwrap_or_else(|error| panic!("fixture router must build: {error}"));

    let events = router
        .stream_alias("fast", request())
        .unwrap_or_else(|error| panic!("fixture alias must resolve: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        events.first(),
        Some(&Ok(ProviderEvent::RouteSelected {
            route: "fallback".to_owned(),
        }))
    );
    assert_eq!(&events[1..], openai_expected().as_slice());
    let fallback_requests = join_requests(fallback_server.task).await;
    assert_eq!(fallback_requests.len(), 1);
    assert!(String::from_utf8_lossy(&fallback_requests[0].body).contains("live-model"));
}

#[tokio::test]
async fn semantic_output_prevents_retryable_stream_failover() {
    let primary_server = spawn_sse_origin("/partial", OPENAI_PARTIAL_THEN_ERROR_SSE, 1).await;
    let fallback_server = spawn_observed_sse_origin("/must-not-run", OPENAI_SSE).await;
    let primary = openai_provider("primary", primary_server.endpoint.clone(), None);
    let fallback = openai_provider("fallback", fallback_server.endpoint.clone(), None);
    let router = ProviderRouter::new(
        BTreeMap::from([(
            "fast".to_owned(),
            vec!["primary/a".to_owned(), "fallback/b".to_owned()],
        )]),
        vec![primary, fallback],
        one_attempt_retry_policy(),
    )
    .unwrap_or_else(|error| panic!("fixture router must build: {error}"));

    let events = router
        .stream_alias("fast", request())
        .unwrap_or_else(|error| panic!("fixture alias must resolve: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert!(
        events.iter().any(
            |event| matches!(event, Ok(ProviderEvent::TextDelta { text }) if text == "partial")
        )
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(error) if error.kind == ProviderErrorKind::Server))
    );
    assert_eq!(fallback_server.requests.load(Ordering::SeqCst), 0);
    fallback_server.task.abort();
    let _ = join_requests(primary_server.task).await;
}

#[tokio::test]
async fn non_success_json_bodies_classify_only_exact_context_overflow_without_leaks() {
    let anthropic_body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 120001 tokens > 100000 maximum"}}"#;
    let anthropic_server = spawn_json_error_origin("/anthropic", 400, anthropic_body).await;
    let anthropic = anthropic_provider("anthropic", anthropic_server.endpoint.clone(), None);
    let directory = unique_temp_directory("anthropic-context-overflow");
    let recorder = Recorder::new(anthropic, &directory, FixtureRedactor::default());
    let error = recorder
        .stream(request())
        .await
        .err()
        .unwrap_or_else(|| panic!("Anthropic 400 must fail"));
    assert_eq!(error.kind, ProviderErrorKind::ContextOverflow);
    assert!(!error.to_string().contains("120001"));
    join_requests(anthropic_server.task).await;
    let replay = ReplayProvider::load("anthropic", &directory)
        .await
        .unwrap_or_else(|error| panic!("Anthropic error replay must load: {error}"));
    let replay_error = replay
        .stream(request())
        .await
        .err()
        .unwrap_or_else(|| panic!("Anthropic error replay must preserve the start error"));
    assert_eq!(replay_error, error);
    remove_temp_directory(directory).await;

    let near_miss = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long; SECRET_CANARY"}}"#;
    let near_server = spawn_json_error_origin("/anthropic", 400, near_miss).await;
    let anthropic = anthropic_provider("anthropic", near_server.endpoint.clone(), None);
    let error = anthropic
        .stream(request())
        .await
        .err()
        .unwrap_or_else(|| panic!("Anthropic near miss must fail"));
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(!error.to_string().contains("SECRET_CANARY"));
    join_requests(near_server.task).await;

    let request_too_large = r#"{"type":"error","error":{"type":"request_too_large","message":"SECRET_REQUEST_TOO_LARGE"}}"#;
    let too_large_server = spawn_json_error_origin("/anthropic", 400, request_too_large).await;
    let anthropic = anthropic_provider("anthropic", too_large_server.endpoint.clone(), None);
    let error = anthropic
        .stream(request())
        .await
        .err()
        .unwrap_or_else(|| panic!("Anthropic request_too_large must fail"));
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(!error.to_string().contains("SECRET_REQUEST_TOO_LARGE"));
    join_requests(too_large_server.task).await;

    let openai_body = r#"{"error":{"code":"context_length_exceeded","message":"SECRET_CANARY"}}"#;
    let openai_server = spawn_json_error_origin("/openai", 400, openai_body).await;
    let openai = openai_provider("openai", openai_server.endpoint.clone(), None);
    let error = openai
        .stream(request())
        .await
        .err()
        .unwrap_or_else(|| panic!("OpenAI 400 must fail"));
    assert_eq!(error.kind, ProviderErrorKind::ContextOverflow);
    assert!(!error.to_string().contains("SECRET_CANARY"));
    join_requests(openai_server.task).await;

    let openai_near_miss = r#"{"error":{"code":"context_length_exceeded.SECRET_OPENAI_NEAR_MISS","type":"invalid_request_error","message":"SECRET_OPENAI_MESSAGE"}}"#;
    let openai_near_server = spawn_json_error_origin("/openai", 400, openai_near_miss).await;
    let openai = openai_provider("openai", openai_near_server.endpoint.clone(), None);
    let error = openai
        .stream(request())
        .await
        .err()
        .unwrap_or_else(|| panic!("OpenAI near miss must fail"));
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(!error.to_string().contains("SECRET_OPENAI_NEAR_MISS"));
    assert!(!error.to_string().contains("SECRET_OPENAI_MESSAGE"));
    join_requests(openai_near_server.task).await;
}

fn one_attempt_retry_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 1,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
        jitter_fraction: 0.0,
    }
}

fn resolved_proxy(settings: &ProxySettings, provider: &str, endpoint: &Url) -> Option<Url> {
    settings
        .resolve(provider, endpoint)
        .map(|resolution| resolution.url)
}

fn assert_serialized_bytes_equal(
    live: &[Result<ProviderEvent, ProviderError>],
    replay: &[Result<ProviderEvent, ProviderError>],
) {
    let live_bytes = serde_json::to_vec(live)
        .unwrap_or_else(|error| panic!("live normalized events must serialize: {error}"));
    let replay_bytes = serde_json::to_vec(replay)
        .unwrap_or_else(|error| panic!("replay normalized events must serialize: {error}"));
    assert_eq!(live_bytes, replay_bytes);
}

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

fn unique_temp_directory(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "rw-provider-http-{label}-{}-{}",
        std::process::id(),
        NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ))
}

async fn remove_temp_directory(directory: PathBuf) {
    if let Err(error) = tokio::fs::remove_dir_all(&directory).await {
        panic!(
            "fixture directory {} must be removable: {error}",
            directory.display()
        );
    }
}

async fn read_fixture_text(directory: &Path) -> String {
    let mut entries = tokio::fs::read_dir(directory)
        .await
        .unwrap_or_else(|error| panic!("fixture directory must be readable: {error}"));
    let mut fixtures = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .unwrap_or_else(|error| panic!("fixture entry must be readable: {error}"))
    {
        if !entry
            .file_name()
            .to_string_lossy()
            .ends_with("-capabilities.json")
        {
            fixtures.push(entry.path());
        }
    }
    assert_eq!(fixtures.len(), 1, "exactly one stream fixture expected");
    tokio::fs::read_to_string(
        fixtures
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("fixture file expected")),
    )
    .await
    .unwrap_or_else(|error| panic!("fixture file must be UTF-8 JSON: {error}"))
}

async fn spawn_sse_origin(path: &str, body: &str, expected_requests: usize) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("fixture origin must bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("fixture origin address must resolve: {error}"));
    let endpoint = parse_url(&format!("http://{address}{path}"));
    let body = body.to_owned();
    let task = tokio::spawn(async move {
        let mut captured = Vec::with_capacity(expected_requests);
        for _ in 0..expected_requests {
            let (mut socket, _) = listener
                .accept()
                .await
                .unwrap_or_else(|error| panic!("fixture origin must accept: {error}"));
            captured.push(read_http_request(&mut socket).await);
            write_sse_response(&mut socket, &body).await;
        }
        captured
    });
    TestServer { endpoint, task }
}

async fn spawn_json_error_origin(path: &str, status: u16, body: &str) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("error origin must bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("error origin address must resolve: {error}"));
    let endpoint = parse_url(&format!("http://{address}{path}"));
    let body = body.to_owned();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener
            .accept()
            .await
            .unwrap_or_else(|error| panic!("error origin must accept: {error}"));
        let captured = read_http_request(&mut socket).await;
        let reason = if status == 400 {
            "Bad Request"
        } else {
            "Error"
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .unwrap_or_else(|error| panic!("error response must write: {error}"));
        vec![captured]
    });
    TestServer { endpoint, task }
}

async fn spawn_observed_sse_origin(path: &str, body: &str) -> ObservedServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("observed origin must bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("observed origin address must resolve: {error}"));
    let endpoint = parse_url(&format!("http://{address}{path}"));
    let requests = Arc::new(AtomicUsize::new(0));
    let task_requests = Arc::clone(&requests);
    let body = body.to_owned();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            task_requests.fetch_add(1, Ordering::SeqCst);
            let _ = read_http_request(&mut socket).await;
            write_sse_response(&mut socket, &body).await;
        }
    });
    ObservedServer {
        endpoint,
        requests,
        task,
    }
}

async fn spawn_forward_proxy(expected_requests: usize) -> TestProxy {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("fixture proxy must bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("fixture proxy address must resolve: {error}"));
    let url = parse_url(&format!("http://{address}"));
    let task = tokio::spawn(async move {
        let mut captured = Vec::with_capacity(expected_requests);
        for _ in 0..expected_requests {
            let (mut downstream, _) = listener
                .accept()
                .await
                .unwrap_or_else(|error| panic!("fixture proxy must accept: {error}"));
            let request = read_http_request(&mut downstream).await;
            forward_request(&request, &mut downstream).await;
            captured.push(request);
        }
        captured
    });
    TestProxy { url, task }
}

async fn forward_request(request: &CapturedRequest, downstream: &mut TcpStream) {
    let mut fields = request.request_line.split_whitespace();
    let method = fields
        .next()
        .unwrap_or_else(|| panic!("proxy method expected"));
    let target = fields
        .next()
        .unwrap_or_else(|| panic!("absolute proxy target expected"));
    let version = fields
        .next()
        .unwrap_or_else(|| panic!("proxy HTTP version expected"));
    let target = parse_url(target);
    let host = target
        .host_str()
        .unwrap_or_else(|| panic!("proxy target host expected"));
    let port = target
        .port_or_known_default()
        .unwrap_or_else(|| panic!("proxy target port expected"));
    let mut upstream = TcpStream::connect((host, port))
        .await
        .unwrap_or_else(|error| panic!("proxy must reach fixture origin: {error}"));
    let path = match target.query() {
        Some(query) => format!("{}?{query}", target.path()),
        None => target.path().to_owned(),
    };
    let first_line_end = find_bytes(&request.raw, b"\r\n")
        .unwrap_or_else(|| panic!("proxy request line terminator expected"));
    let mut forwarded = format!("{method} {path} {version}").into_bytes();
    forwarded.extend_from_slice(&request.raw[first_line_end..]);
    upstream
        .write_all(&forwarded)
        .await
        .unwrap_or_else(|error| panic!("proxy request must forward: {error}"));
    let mut response = Vec::new();
    upstream
        .read_to_end(&mut response)
        .await
        .unwrap_or_else(|error| panic!("proxy response must read: {error}"));
    downstream
        .write_all(&response)
        .await
        .unwrap_or_else(|error| panic!("proxy response must relay: {error}"));
}

async fn read_http_request(socket: &mut TcpStream) -> CapturedRequest {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = socket
            .read(&mut buffer)
            .await
            .unwrap_or_else(|error| panic!("fixture request must read: {error}"));
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
        if let Some(total_length) = complete_request_length(&raw)
            && raw.len() >= total_length
        {
            break;
        }
    }
    let header_end = find_bytes(&raw, b"\r\n\r\n")
        .unwrap_or_else(|| panic!("fixture request headers must terminate"));
    let first_line_end =
        find_bytes(&raw, b"\r\n").unwrap_or_else(|| panic!("fixture request line must terminate"));
    CapturedRequest {
        request_line: String::from_utf8_lossy(&raw[..first_line_end]).into_owned(),
        body: raw[header_end + 4..].to_vec(),
        raw,
    }
}

fn complete_request_length(raw: &[u8]) -> Option<usize> {
    let header_end = find_bytes(raw, b"\r\n\r\n")?;
    let headers = String::from_utf8_lossy(&raw[..header_end]);
    let content_length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })?;
    Some(header_end + 4 + content_length)
}

async fn write_sse_response(socket: &mut TcpStream, body: &str) {
    let headers = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    socket
        .write_all(headers.as_bytes())
        .await
        .unwrap_or_else(|error| panic!("fixture response headers must write: {error}"));
    for chunk in body.as_bytes().chunks(17) {
        socket
            .write_all(chunk)
            .await
            .unwrap_or_else(|error| panic!("fixture response body must write: {error}"));
    }
    socket
        .shutdown()
        .await
        .unwrap_or_else(|error| panic!("fixture response must close: {error}"));
}

async fn join_requests(task: JoinHandle<Vec<CapturedRequest>>) -> Vec<CapturedRequest> {
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap_or_else(|_| panic!("fixture server did not finish"))
        .unwrap_or_else(|error| panic!("fixture server task failed: {error}"))
}

fn parse_url(value: &str) -> Url {
    Url::parse(value).unwrap_or_else(|error| panic!("fixture URL must parse: {error}"))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
