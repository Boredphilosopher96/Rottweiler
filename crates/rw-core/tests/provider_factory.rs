use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use rw_core::{ModelAccounting, ProviderFactory};
use rw_providers::{
    ModelPricing, NetworkPolicy, PricingTable, Provider, ProviderErrorKind, ProviderRequest,
    ProxyEnvironment, Recorder, ReplayProvider, ThinkingLevel, ToolChoice, ToolDefinition,
};
use rw_store::credentials::{
    CredentialEnvironment, CredentialError, CredentialKeychain, CredentialManager,
    KEYCHAIN_VAULT_ID, KeychainUnavailable, Secret,
};
use rw_types::{
    Block, ImageRef, Role, ToolCallId, ToolOutput, ToolOutputPart, Turn, TurnMeta,
    config::ProviderConfig,
};
use serde_json::json;
use tempfile::tempdir;

const API_CANARY: &str = "rw-api-secret-canary";
const OAUTH_CANARY: &str = "rw-oauth-secret-canary";
const REFRESH_CANARY: &str = "rw-refresh-secret-canary";
const ROTATED_CANARY: &str = "rw-rotated-refresh-canary";
const REFRESHED_ACCESS_CANARY: &str = "rw-refreshed-access-canary";

#[derive(Clone, Default)]
struct TestEnvironment(BTreeMap<String, String>);

impl CredentialEnvironment for TestEnvironment {
    fn get(&self, name: &str) -> Result<Option<String>, CredentialError> {
        Ok(self.0.get(name).cloned())
    }
}

#[derive(Clone, Default)]
struct TestKeychain(Arc<Mutex<BTreeMap<String, String>>>);

impl TestKeychain {
    fn insert(&self, identifier: &str, value: &str) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(identifier.to_owned(), value.to_owned());
    }
}

impl CredentialKeychain for TestKeychain {
    fn get(&self, identifier: &str) -> Result<Option<Secret<String>>, KeychainUnavailable> {
        self.0
            .lock()
            .map_err(|_| KeychainUnavailable)
            .map(|values| values.get(identifier).cloned().map(Secret::new))
    }

    fn set(&self, identifier: &str, secret: &Secret<String>) -> Result<(), KeychainUnavailable> {
        self.0
            .lock()
            .map_err(|_| KeychainUnavailable)?
            .insert(identifier.to_owned(), secret.expose_secret().clone());
        Ok(())
    }
}

#[derive(Clone)]
struct FallbackOnSetKeychain(TestKeychain);

impl CredentialKeychain for FallbackOnSetKeychain {
    fn get(&self, identifier: &str) -> Result<Option<Secret<String>>, KeychainUnavailable> {
        self.0.get(identifier)
    }

    fn set(&self, _identifier: &str, _secret: &Secret<String>) -> Result<(), KeychainUnavailable> {
        Err(KeychainUnavailable)
    }
}

struct TestServer {
    endpoint: String,
    task: thread::JoinHandle<Vec<String>>,
}

fn spawn_server(path: &str, responses: Vec<String>) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .unwrap_or_else(|error| panic!("fixture listener must bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("fixture address must resolve: {error}"));
    let endpoint = format!("http://{address}{path}");
    listener
        .set_nonblocking(true)
        .unwrap_or_else(|error| panic!("fixture listener must become nonblocking: {error}"));
    let task = thread::spawn(move || {
        responses
            .into_iter()
            .map(|response| {
                let deadline = Instant::now() + Duration::from_secs(5);
                let (mut socket, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "fixture request did not arrive before timeout"
                            );
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("fixture request must arrive: {error}"),
                    }
                };
                socket
                    .set_nonblocking(false)
                    .unwrap_or_else(|error| panic!("fixture socket must become blocking: {error}"));
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap_or_else(|error| panic!("fixture timeout must configure: {error}"));
                let request = read_request(&mut socket);
                socket
                    .write_all(response.as_bytes())
                    .unwrap_or_else(|error| panic!("fixture response must write: {error}"));
                request
            })
            .collect()
    });
    TestServer { endpoint, task }
}

fn read_request(socket: &mut std::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 2048];
    let header_end = loop {
        let read = socket
            .read(&mut chunk)
            .unwrap_or_else(|error| panic!("fixture request must read: {error}"));
        assert_ne!(read, 0, "request closed before headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let read = socket
            .read(&mut chunk)
            .unwrap_or_else(|error| panic!("fixture body must read: {error}"));
        assert_ne!(read, 0, "request closed before body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn sse_response(text: &str) -> String {
    let body = format!(
        "data: {{\"id\":\"chat-1\",\"model\":\"fixture\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
        serde_json::to_string(text)
            .unwrap_or_else(|error| panic!("fixture text must encode: {error}"))
    );
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn status_response(status: &str) -> String {
    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

fn pricing(models: impl IntoIterator<Item = (&'static str, bool)>) -> PricingTable {
    PricingTable {
        source_url: "https://models.dev/api.json".to_owned(),
        snapshot_date: "2026-07-10".to_owned(),
        revision: "provider-factory-test".to_owned(),
        models: models
            .into_iter()
            .map(|(model, tools)| {
                (
                    model.to_owned(),
                    ModelPricing {
                        display_name: model.to_owned(),
                        max_context_tokens: Some(100_000),
                        max_output_tokens: Some(8_192),
                        supports_tools: tools,
                        supports_thinking: false,
                        reasoning_efforts: Vec::new(),
                        input_per_million_micros_usd: 1,
                        output_per_million_micros_usd: 1,
                        cache_read_per_million_micros_usd: None,
                        cache_write_per_million_micros_usd: None,
                        reasoning_per_million_micros_usd: None,
                    },
                )
            })
            .collect(),
    }
}

fn config(endpoint: &str, candidates: &[&str]) -> rw_types::config::Config {
    let mut config = rw_types::config::Config::default();
    "fast".clone_into(&mut config.models.default);
    config.models.aliases.insert(
        "fast".to_owned(),
        candidates.iter().map(ToString::to_string).collect(),
    );
    config.providers.insert(
        "fixture".to_owned(),
        ProviderConfig {
            kind: "openai_compatible".to_owned(),
            base_url: Some(endpoint.to_owned()),
            ..ProviderConfig::default()
        },
    );
    config
}

fn subscription_config(model: &str) -> rw_types::config::Config {
    let mut config = rw_types::config::Config::default();
    "fast".clone_into(&mut config.models.default);
    config
        .models
        .aliases
        .insert("fast".to_owned(), vec![format!("fixture/{model}")]);
    config.providers.insert(
        "fixture".to_owned(),
        ProviderConfig {
            kind: "openai_codex".to_owned(),
            ..ProviderConfig::default()
        },
    );
    config
}

fn subscription_keychain() -> TestKeychain {
    let keychain = TestKeychain::default();
    keychain.insert(
        "providers.fixture.openai_subscription",
        r#"{"version":1,"access_token":"subscription-access-canary","refresh_token":"subscription-refresh-canary","account_id":"acct-fixture"}"#,
    );
    keychain
}

fn copilot_config(model: &str) -> rw_types::config::Config {
    let mut config = rw_types::config::Config::default();
    "fast".clone_into(&mut config.models.default);
    config
        .models
        .aliases
        .insert("fast".to_owned(), vec![format!("github-copilot/{model}")]);
    config.providers.insert(
        "github-copilot".to_owned(),
        ProviderConfig {
            kind: "github_copilot".to_owned(),
            ..ProviderConfig::default()
        },
    );
    config
}

fn copilot_keychain() -> TestKeychain {
    let keychain = TestKeychain::default();
    keychain.insert(
        "providers.github-copilot.github_copilot",
        r#"{"version":1,"oauth_client_id":"rottweiler-test-client","access_token":"copilot-token-canary"}"#,
    );
    keychain
}

fn unused_copilot_test_origin() -> url::Url {
    url::Url::parse("http://127.0.0.1:9/")
        .unwrap_or_else(|error| panic!("static Copilot test origin must parse: {error}"))
}

fn copilot_catalog(supports_vision: bool, reasoning_efforts: &[&str]) -> String {
    json!({
        "data": [{
            "model_picker_enabled": true,
            "id": "fixture-model",
            "name": "Fixture Copilot",
            "version": "fixture-model-2026-07-10",
            "supported_endpoints": ["/chat/completions"],
            "policy": {"state": "enabled"},
            "capabilities": {
                "family": "gpt",
                "limits": {
                    "max_context_window_tokens": 100_000,
                    "max_output_tokens": 4_096,
                    "max_prompt_tokens": 90_000
                },
                "supports": {
                    "tool_calls": true,
                    "vision": supports_vision,
                    "reasoning_effort": reasoning_efforts
                }
            },
            "billing": {
                "token_prices": {
                    "batch_size": 1_000,
                    "default": {
                        "input_price": 0.25,
                        "cache_price": 0.1,
                        "output_price": 1.0
                    }
                }
            }
        }]
    })
    .to_string()
}

fn manager(
    environment: TestEnvironment,
    keychain: TestKeychain,
) -> Arc<CredentialManager<TestEnvironment, TestKeychain>> {
    Arc::new(CredentialManager::with_backends(
        environment,
        keychain,
        PathBuf::from("unused-provider-factory-credentials.toml"),
    ))
}

fn request(model: &str) -> ProviderRequest {
    ProviderRequest {
        model: model.to_owned(),
        turns: vec![Turn {
            role: Role::User,
            blocks: vec![Block::Text {
                text: "respond once".to_owned(),
            }],
            meta: TurnMeta::default(),
        }],
        tools: Vec::new(),
        tool_choice: rw_providers::ToolChoice::Auto,
        max_output_tokens: 32,
        temperature: None,
        thinking: ThinkingLevel::Off,
    }
}

#[tokio::test]
async fn environment_api_key_wins_and_recorder_redacts_known_secret() {
    let server = spawn_server("/v1/chat/completions", vec![sse_response(API_CANARY)]);
    let mut config = config(&server.endpoint, &["fixture/model-a"]);
    let provider = config
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"));
    provider.api_key_env = Some("FIXTURE_API_KEY".to_owned());
    provider.api_key_credential = Some("fixture-api-key".to_owned());
    let keychain = TestKeychain::default();
    keychain.insert("fixture-api-key", "keychain-must-lose");
    let environment = TestEnvironment(BTreeMap::from([(
        "FIXTURE_API_KEY".to_owned(),
        API_CANARY.to_owned(),
    )]));
    let runtime = ProviderFactory::with_backends(
        manager(environment, keychain),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("fixture/model-a", false)]),
    )
    .build(&config)
    .unwrap_or_else(|error| panic!("factory must build: {error}"));
    let directory = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let recorder = Recorder::new(
        runtime
            .provider("fixture/model-a")
            .unwrap_or_else(|| panic!("model-bound provider must exist")),
        directory.path(),
        runtime.fixture_redactor(),
    );
    let events = recorder
        .stream(request("model-a"))
        .await
        .unwrap_or_else(|error| panic!("stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok));
    recorder
        .flush()
        .await
        .unwrap_or_else(|error| panic!("fixture must flush: {error}"));
    let captured = server
        .task
        .join()
        .unwrap_or_else(|_| panic!("fixture server must join"));
    assert!(captured[0].contains(&format!("Bearer {API_CANARY}")));
    assert!(!captured[0].contains("keychain-must-lose"));
    let fixture_text = fs::read_dir(directory.path())
        .unwrap_or_else(|error| panic!("fixture directory must read: {error}"))
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().contains("capabilities"))
        .map(|entry| {
            fs::read_to_string(entry.path())
                .unwrap_or_else(|error| panic!("fixture must read: {error}"))
        })
        .collect::<String>();
    assert!(fixture_text.contains("[REDACTED]"));
    assert!(!fixture_text.contains(API_CANARY));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn static_oauth_and_refresh_rotation_use_shared_credential_boundary() {
    let oauth_server = spawn_server("/v1/chat/completions", vec![sse_response("oauth-ok")]);
    let mut oauth_config = config(&oauth_server.endpoint, &["fixture/model-a"]);
    oauth_config
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"))
        .oauth_token_env = Some("FIXTURE_OAUTH_TOKEN".to_owned());
    let runtime = ProviderFactory::with_backends(
        manager(
            TestEnvironment(BTreeMap::from([(
                "FIXTURE_OAUTH_TOKEN".to_owned(),
                OAUTH_CANARY.to_owned(),
            )])),
            TestKeychain::default(),
        ),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("fixture/model-a", false)]),
    )
    .build(&oauth_config)
    .unwrap_or_else(|error| panic!("static OAuth factory must build: {error}"));
    runtime
        .provider("fixture/model-a")
        .unwrap_or_else(|| panic!("OAuth provider must exist"))
        .stream(request("model-a"))
        .await
        .unwrap_or_else(|error| panic!("OAuth stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    let captured = oauth_server
        .task
        .join()
        .unwrap_or_else(|_| panic!("OAuth server must join"));
    assert!(captured[0].contains(&format!("Bearer {OAUTH_CANARY}")));

    let token_body = format!(
        "{{\"access_token\":\"{REFRESHED_ACCESS_CANARY}\",\"refresh_token\":\"{ROTATED_CANARY}\",\"expires_in\":3600,\"token_type\":\"Bearer\"}}"
    );
    let token_server = spawn_server("/oauth/token", vec![json_response(&token_body)]);
    let api_server = spawn_server(
        "/v1/chat/completions",
        vec![
            sse_response(&format!("echo {REFRESHED_ACCESS_CANARY} {ROTATED_CANARY}")),
            sse_response("refresh-b"),
        ],
    );
    let mut refresh_config = config(
        &api_server.endpoint,
        &["fixture/model-a", "fixture/model-b"],
    );
    let provider = refresh_config
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"));
    provider.oauth_token_endpoint = Some(token_server.endpoint.clone());
    provider.oauth_client_id = Some("public-client".to_owned());
    provider.oauth_refresh_token_credential = Some("fixture-refresh".to_owned());
    let keychain = TestKeychain::default();
    keychain.insert(
        KEYCHAIN_VAULT_ID,
        &format!("version = 1\n[credentials]\nfixture-refresh = {REFRESH_CANARY:?}\n"),
    );
    let fallback = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let credentials_path = fallback.path().join("credentials.toml");
    let runtime = ProviderFactory::with_backends(
        Arc::new(CredentialManager::with_backends(
            TestEnvironment::default(),
            FallbackOnSetKeychain(keychain),
            credentials_path.clone(),
        )),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("fixture/model-a", false), ("fixture/model-b", true)]),
    )
    .build(&refresh_config)
    .unwrap_or_else(|error| panic!("refresh factory must build: {error}"));
    let directory = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let redactor = runtime.fixture_redactor();
    let recorder = Recorder::new(
        runtime
            .provider("fixture/model-a")
            .unwrap_or_else(|| panic!("refresh provider must exist")),
        directory.path(),
        redactor.clone(),
    );
    recorder
        .stream(request("model-a"))
        .await
        .unwrap_or_else(|error| panic!("refresh stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    recorder
        .flush()
        .await
        .unwrap_or_else(|error| panic!("refresh fixture must flush: {error}"));
    runtime
        .provider("fixture/model-b")
        .unwrap_or_else(|| panic!("second refresh provider must exist"))
        .stream(request("model-b"))
        .await
        .unwrap_or_else(|error| panic!("second refresh stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    let fallback_text = fs::read_to_string(&credentials_path)
        .unwrap_or_else(|error| panic!("rotated fallback credential must read: {error}"));
    assert!(fallback_text.contains(ROTATED_CANARY));
    assert!(
        runtime
            .warnings()
            .iter()
            .any(|warning| warning.contains("plaintext fallback file"))
    );
    let token_request = token_server
        .task
        .join()
        .unwrap_or_else(|_| panic!("token server must join"));
    assert!(token_request[0].contains(REFRESH_CANARY));
    let api_request = api_server
        .task
        .join()
        .unwrap_or_else(|_| panic!("API server must join"));
    assert_eq!(api_request.len(), 2);
    assert!(
        api_request
            .iter()
            .all(|request| request.contains(&format!("Bearer {REFRESHED_ACCESS_CANARY}")))
    );
    assert!(redactor.registered_secret_count() >= 3);
    let fixture_text = fs::read_dir(directory.path())
        .unwrap_or_else(|error| panic!("fixture directory must read: {error}"))
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().contains("capabilities"))
        .map(|entry| {
            fs::read_to_string(entry.path())
                .unwrap_or_else(|error| panic!("fixture must read: {error}"))
        })
        .collect::<String>();
    assert!(fixture_text.contains("[REDACTED]"));
    let runtime_debug = format!("{runtime:?} {redactor:?}");
    for canary in [REFRESH_CANARY, ROTATED_CANARY, REFRESHED_ACCESS_CANARY] {
        assert!(!fixture_text.contains(canary));
        assert!(!runtime_debug.contains(canary));
    }
}

#[tokio::test]
async fn keychain_api_key_and_stored_oauth_access_are_real_request_paths() {
    let api_server = spawn_server("/v1/chat/completions", vec![sse_response("api-key")]);
    let mut api_config = config(&api_server.endpoint, &["fixture/model-a"]);
    api_config
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"))
        .api_key_credential = Some("stored-api-key".to_owned());
    let keychain = TestKeychain::default();
    keychain.insert("stored-api-key", API_CANARY);
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), keychain),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("fixture/model-a", false)]),
    )
    .build(&api_config)
    .unwrap_or_else(|error| panic!("stored API key factory must build: {error}"));
    runtime
        .provider("fixture/model-a")
        .unwrap_or_else(|| panic!("stored API provider must exist"))
        .stream(request("model-a"))
        .await
        .unwrap_or_else(|error| panic!("stored API stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    let requests = api_server
        .task
        .join()
        .unwrap_or_else(|_| panic!("stored API server must join"));
    assert!(requests[0].contains(&format!("Bearer {API_CANARY}")));

    let oauth_server = spawn_server("/v1/chat/completions", vec![sse_response("oauth")]);
    let mut oauth_config = config(&oauth_server.endpoint, &["fixture/model-a"]);
    oauth_config
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"))
        .oauth_access_token_credential = Some("stored-oauth-access".to_owned());
    let keychain = TestKeychain::default();
    keychain.insert("stored-oauth-access", OAUTH_CANARY);
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), keychain),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("fixture/model-a", false)]),
    )
    .build(&oauth_config)
    .unwrap_or_else(|error| panic!("stored OAuth factory must build: {error}"));
    runtime
        .provider("fixture/model-a")
        .unwrap_or_else(|| panic!("stored OAuth provider must exist"))
        .stream(request("model-a"))
        .await
        .unwrap_or_else(|error| panic!("stored OAuth stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    let requests = oauth_server
        .task
        .join()
        .unwrap_or_else(|_| panic!("stored OAuth server must join"));
    assert!(requests[0].contains(&format!("Bearer {OAUTH_CANARY}")));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn model_caps_binding_conflicts_and_alias_invariants_fail_closed() {
    let endpoint = "http://127.0.0.1:9/v1/chat/completions";
    let mut runtime_config = config(endpoint, &["fixture/model-a", "fixture/model-b"]);
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestKeychain::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Deny,
        pricing([("fixture/model-a", true), ("fixture/model-b", false)]),
    )
    .build(&runtime_config)
    .unwrap_or_else(|error| panic!("local unauthenticated factory must build: {error}"));
    assert!(
        runtime
            .resolved_model("fixture/model-a")
            .unwrap_or_else(|| panic!("model a must resolve"))
            .capabilities()
            .tool_calling
    );
    assert!(
        !runtime
            .resolved_model("fixture/model-b")
            .unwrap_or_else(|| panic!("model b must resolve"))
            .capabilities()
            .tool_calling
    );
    let error = runtime
        .provider("fixture/model-a")
        .unwrap_or_else(|| panic!("model a provider must exist"))
        .stream(request("model-b"))
        .await
        .err()
        .unwrap_or_else(|| panic!("model-bound provider must reject a different model"));
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);

    let mut tool_request = request("model-b");
    tool_request.tools.push(ToolDefinition {
        name: "read".to_owned(),
        description: "read".to_owned(),
        input_schema: json!({"type":"object"}),
    });
    let error = runtime
        .provider("fixture/model-b")
        .unwrap_or_else(|| panic!("model b provider must exist"))
        .stream(tool_request)
        .await
        .err()
        .unwrap_or_else(|| panic!("non-tool model must reject tools before network"));
    assert_eq!(error.kind, ProviderErrorKind::Unsupported);

    let mut image_request = request("model-b");
    image_request.turns[0].blocks = vec![Block::ToolResult {
        id: ToolCallId("image-tool".to_owned()),
        output: ToolOutput::Mixed {
            parts: vec![ToolOutputPart::Image {
                media_type: "image/png".to_owned(),
                data: ImageRef::InlineBase64 {
                    data: "aW1hZ2U=".to_owned(),
                },
            }],
        },
        is_error: false,
    }];
    let error = runtime
        .provider("fixture/model-b")
        .unwrap_or_else(|| panic!("model b provider must exist"))
        .stream(image_request)
        .await
        .err()
        .unwrap_or_else(|| panic!("model without vision metadata must reject nested images"));
    assert_eq!(error.kind, ProviderErrorKind::Unsupported);

    let provider = runtime_config
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"));
    provider.api_key_env = Some("SECRET_ENV".to_owned());
    provider.oauth_token_env = Some("OAUTH_ENV".to_owned());
    let conflict = ProviderFactory::with_backends(
        manager(
            TestEnvironment(BTreeMap::from([
                ("SECRET_ENV".to_owned(), API_CANARY.to_owned()),
                ("OAUTH_ENV".to_owned(), OAUTH_CANARY.to_owned()),
            ])),
            TestKeychain::default(),
        ),
        ProxyEnvironment::default(),
        NetworkPolicy::Deny,
        pricing([("fixture/model-a", true), ("fixture/model-b", false)]),
    )
    .build(&runtime_config)
    .err()
    .unwrap_or_else(|| panic!("mixed auth families must fail"));
    let diagnostic = format!("{conflict:?} {conflict}");
    assert!(!diagnostic.contains(API_CANARY));
    assert!(!diagnostic.contains(OAUTH_CANARY));

    let mut invalid = config(endpoint, &["fixture/model-a"]);
    "missing".clone_into(&mut invalid.models.default);
    assert!(
        ProviderFactory::with_backends(
            manager(TestEnvironment::default(), TestKeychain::default()),
            ProxyEnvironment::default(),
            NetworkPolicy::Deny,
            pricing([("fixture/model-a", false)]),
        )
        .build(&invalid)
        .is_err()
    );

    let unknown = config(endpoint, &["fixture/unknown-model"]);
    let unknown_runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestKeychain::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Deny,
        pricing([("unrelated/catalog-entry", true)]),
    )
    .build(&unknown)
    .unwrap_or_else(|error| panic!("unknown local model must degrade safely: {error}"));
    let capabilities = unknown_runtime
        .resolved_model("fixture/unknown-model")
        .unwrap_or_else(|| panic!("unknown model must resolve"))
        .capabilities();
    assert!(!capabilities.tool_calling);
    assert!(!capabilities.vision);
    assert!(!capabilities.thinking);
    assert_eq!(
        capabilities.cache_breakpoints,
        rw_providers::CacheBreakpointSupport::None
    );
}

#[tokio::test]
async fn provider_proxy_credentials_win_and_are_redactor_registered() {
    let proxy = spawn_server("/", vec![sse_response("proxied")]);
    let mut config = config(
        "http://127.0.0.1:9/v1/chat/completions",
        &["fixture/model-a"],
    );
    let provider = config
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"));
    provider.api_key_env = Some("FIXTURE_API_KEY".to_owned());
    provider.proxy = Some(proxy.endpoint.clone());
    provider.proxy_username = Some("proxy-user".to_owned());
    provider.proxy_password_credential = Some("proxy-password".to_owned());
    let keychain = TestKeychain::default();
    keychain.insert("proxy-password", "proxy-secret");
    let runtime = ProviderFactory::with_backends(
        manager(
            TestEnvironment(BTreeMap::from([(
                "FIXTURE_API_KEY".to_owned(),
                API_CANARY.to_owned(),
            )])),
            keychain,
        ),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("fixture/model-a", false)]),
    )
    .build(&config)
    .unwrap_or_else(|error| panic!("proxied factory must build: {error}"));
    runtime
        .provider("fixture/model-a")
        .unwrap_or_else(|| panic!("proxied provider must exist"))
        .stream(request("model-a"))
        .await
        .unwrap_or_else(|error| panic!("proxied stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    let captured = proxy
        .task
        .join()
        .unwrap_or_else(|_| panic!("proxy server must join"));
    assert!(
        captured[0]
            .to_ascii_lowercase()
            .contains("proxy-authorization: basic")
    );
    assert!(captured[0].contains("cHJveHktdXNlcjpwcm94eS1zZWNyZXQ="));
    assert!(captured[0].contains(&format!("Bearer {API_CANARY}")));
}

#[test]
fn official_kind_uses_canonical_catalog_namespace_while_compatible_is_explicit() {
    let endpoint = "http://127.0.0.1:9/v1/chat/completions";
    let mut official = config(endpoint, &["fixture/model-a"]);
    let mut provider = official
        .providers
        .remove("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"));
    "openai".clone_into(&mut provider.kind);
    official.providers.insert("misleading".to_owned(), provider);
    official
        .models
        .aliases
        .insert("fast".to_owned(), vec!["misleading/model-a".to_owned()]);
    let table = pricing([("misleading/model-a", true), ("openai/model-a", false)]);
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestKeychain::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Deny,
        table.clone(),
    )
    .build(&official)
    .unwrap_or_else(|error| panic!("official provider must build: {error}"));
    let model = runtime
        .resolved_model("misleading/model-a")
        .unwrap_or_else(|| panic!("official model must resolve"));
    assert_eq!(model.catalog_model(), Some("openai/model-a"));
    assert!(!model.capabilities().tool_calling);

    official
        .providers
        .get_mut("misleading")
        .unwrap_or_else(|| panic!("misleading provider must exist"))
        .kind = "openai_compatible".to_owned();
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestKeychain::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Deny,
        table,
    )
    .build(&official)
    .unwrap_or_else(|error| panic!("compatible provider must build: {error}"));
    let model = runtime
        .resolved_model("misleading/model-a")
        .unwrap_or_else(|| panic!("compatible model must resolve"));
    assert_eq!(model.catalog_model(), Some("misleading/model-a"));
    assert!(model.capabilities().tool_calling);
}

#[test]
fn subscription_kind_has_independent_capabilities_and_no_dollar_pricing() {
    let config = subscription_config("gpt-5.4-mini");
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), subscription_keychain()),
        ProxyEnvironment::default(),
        NetworkPolicy::Deny,
        pricing([("openai/gpt-5.4-mini", false)]),
    )
    .build(&config)
    .unwrap_or_else(|error| panic!("subscription provider must build: {error}"));
    let model = runtime
        .resolved_model("fixture/gpt-5.4-mini")
        .unwrap_or_else(|| panic!("subscription model must resolve"));
    assert_eq!(model.catalog_model(), Some("openai/gpt-5.4-mini"));
    assert!(model.pricing().is_none());
    assert_eq!(model.accounting(), ModelAccounting::SubscriptionQuota);
    assert!(model.capabilities().tool_calling);
    assert!(model.capabilities().thinking);
    assert!(!model.capabilities().vision);
    assert_eq!(
        model.capabilities().wire_mode,
        rw_providers::WireMode::OpenAiResponses
    );
    assert!(runtime.provider("fixture/gpt-5.4-mini").is_some());

    let debug = format!("{runtime:?}");
    assert!(!debug.contains("subscription-access-canary"));
    assert!(!debug.contains("subscription-refresh-canary"));
    assert!(!debug.contains("acct-fixture"));
}

#[test]
fn subscription_kind_rejects_auth_endpoint_and_model_conflicts() {
    let build = |config: &rw_types::config::Config| {
        ProviderFactory::with_backends(
            manager(TestEnvironment::default(), subscription_keychain()),
            ProxyEnvironment::default(),
            NetworkPolicy::Deny,
            pricing([("openai/gpt-5.4-mini", false)]),
        )
        .build(config)
    };

    let mut api_key = subscription_config("gpt-5.4-mini");
    api_key
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"))
        .api_key_env = Some("OPENAI_API_KEY".to_owned());
    assert!(build(&api_key).is_err());

    let mut endpoint = subscription_config("gpt-5.4-mini");
    endpoint
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"))
        .base_url = Some("https://example.com/v1/responses".to_owned());
    assert!(build(&endpoint).is_err());
    assert!(build(&subscription_config("gpt-5.5-pro")).is_err());
    assert!(build(&subscription_config("gpt-5.6-luna")).is_err());
}

#[test]
fn copilot_kind_is_credit_accounted_redacted_and_conflict_closed() {
    let build = |config: &rw_types::config::Config| {
        ProviderFactory::with_backends(
            manager(TestEnvironment::default(), copilot_keychain()),
            ProxyEnvironment::default(),
            NetworkPolicy::Deny,
            pricing([("unrelated/model", false)]),
        )
        .with_github_copilot_test_origin(
            "github-copilot",
            unused_copilot_test_origin(),
            "rottweiler-test-client",
        )
        .build(config)
    };

    let config = copilot_config("fixture-model");
    let runtime = build(&config)
        .unwrap_or_else(|error| panic!("Copilot provider must compose offline: {error}"));
    let model = runtime
        .resolved_model("github-copilot/fixture-model")
        .unwrap_or_else(|| panic!("Copilot model must resolve"));
    assert_eq!(
        model.accounting(),
        ModelAccounting::AiCredits {
            micros_usd_per_credit: 10_000,
        }
    );
    assert_eq!(model.catalog_model(), None);
    assert!(model.pricing().is_none());
    assert_eq!(
        model.capabilities().wire_mode,
        rw_providers::WireMode::GitHubCopilot
    );
    assert!(model.capabilities().tool_calling);
    assert!(!model.capabilities().vision);
    assert!(!model.capabilities().thinking);
    assert!(runtime.fixture_redactor().registered_secret_count() >= 1);
    assert!(!format!("{runtime:?}").contains("copilot-token-canary"));

    let mut api_key = copilot_config("fixture-model");
    api_key
        .providers
        .get_mut("github-copilot")
        .unwrap_or_else(|| panic!("Copilot provider must exist"))
        .api_key_env = Some("COPILOT_API_KEY".to_owned());
    assert!(build(&api_key).is_err());

    let mut endpoint = copilot_config("fixture-model");
    endpoint
        .providers
        .get_mut("github-copilot")
        .unwrap_or_else(|| panic!("Copilot provider must exist"))
        .base_url = Some("https://example.com".to_owned());
    assert!(build(&endpoint).is_err());

    let identity_mismatch = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), copilot_keychain()),
        ProxyEnvironment::default(),
        NetworkPolicy::Deny,
        pricing([("unrelated/model", false)]),
    )
    .with_github_copilot_test_origin(
        "github-copilot",
        unused_copilot_test_origin(),
        "different-test-client",
    )
    .build(&config)
    .err()
    .unwrap_or_else(|| panic!("mismatched Copilot OAuth identity must fail"));
    assert!(!format!("{identity_mismatch:?}").contains("copilot-token-canary"));
}

#[tokio::test]
async fn copilot_invalid_tool_choices_fail_before_model_discovery_socket() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .unwrap_or_else(|error| panic!("Copilot discovery canary must bind: {error}"));
    listener
        .set_nonblocking(true)
        .unwrap_or_else(|error| panic!("Copilot discovery canary must be nonblocking: {error}"));
    let origin = url::Url::parse(&format!(
        "http://{}/",
        listener
            .local_addr()
            .unwrap_or_else(|error| panic!("Copilot canary address must resolve: {error}"))
    ))
    .unwrap_or_else(|error| panic!("Copilot canary origin must parse: {error}"));
    let config = copilot_config("fixture-model");
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), copilot_keychain()),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("unrelated/model", false)]),
    )
    .with_github_copilot_test_origin("github-copilot", origin, "rottweiler-test-client")
    .build(&config)
    .unwrap_or_else(|error| panic!("Copilot canary factory must build: {error}"));
    let provider = runtime
        .provider("github-copilot/fixture-model")
        .unwrap_or_else(|| panic!("Copilot provider must exist"));
    let mut required = request("fixture-model");
    required.tool_choice = ToolChoice::Required;
    let mut named_without_tools = request("fixture-model");
    named_without_tools.tool_choice = ToolChoice::Named {
        name: "missing".to_owned(),
    };
    let mut named_missing = request("fixture-model");
    named_missing.tools.push(ToolDefinition {
        name: "available".to_owned(),
        description: "available fixture tool".to_owned(),
        input_schema: json!({"type": "object"}),
    });
    named_missing.tool_choice = ToolChoice::Named {
        name: "missing".to_owned(),
    };
    for invalid in [required, named_without_tools, named_missing] {
        let error = provider
            .stream(invalid)
            .await
            .err()
            .unwrap_or_else(|| panic!("invalid tool choice must fail"));
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    }
    assert!(
        listener
            .accept()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
        "invalid tool choice unexpectedly opened a /models socket"
    );
}

#[tokio::test]
async fn copilot_factory_discovers_records_and_replays_without_another_socket() {
    let catalog = r#"{"data":[{"model_picker_enabled":true,"id":"fixture-model","name":"Fixture Copilot","version":"fixture-model-2026-07-10","supported_endpoints":["/chat/completions"],"policy":{"state":"enabled"},"capabilities":{"family":"gpt","limits":{"max_context_window_tokens":100000,"max_output_tokens":4096,"max_prompt_tokens":90000},"supports":{"tool_calls":true,"reasoning_effort":["none"]}}}]}"#;
    let server = spawn_server(
        "/",
        vec![json_response(catalog), sse_response("copilot-ok")],
    );
    let origin = url::Url::parse(&server.endpoint)
        .unwrap_or_else(|error| panic!("loopback Copilot origin must parse: {error}"));
    let config = copilot_config("fixture-model");
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), copilot_keychain()),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("unrelated/model", false)]),
    )
    .with_github_copilot_test_origin("github-copilot", origin, "rottweiler-test-client")
    .build(&config)
    .unwrap_or_else(|error| panic!("loopback Copilot factory must build: {error}"));
    let directory = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let candidate = "github-copilot/fixture-model";
    let recorder = Recorder::new(
        runtime
            .provider(candidate)
            .unwrap_or_else(|| panic!("Copilot provider must exist")),
        directory.path(),
        runtime.fixture_redactor(),
    );
    let live = recorder
        .stream(request("fixture-model"))
        .await
        .unwrap_or_else(|error| panic!("Copilot stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert!(live.iter().all(Result::is_ok));
    recorder
        .flush()
        .await
        .unwrap_or_else(|error| panic!("Copilot fixture must flush: {error}"));
    let captured = server
        .task
        .join()
        .unwrap_or_else(|_| panic!("Copilot fixture server must join"));
    assert!(captured[0].starts_with("GET /models HTTP/1.1"));
    assert!(captured[1].starts_with("POST /chat/completions HTTP/1.1"));
    assert!(
        captured
            .iter()
            .all(|request| request.contains("Bearer copilot-token-canary"))
    );

    let fixture_text = fs::read_dir(directory.path())
        .unwrap_or_else(|error| panic!("Copilot fixture directory must read: {error}"))
        .filter_map(Result::ok)
        .map(|entry| {
            fs::read_to_string(entry.path())
                .unwrap_or_else(|error| panic!("Copilot fixture must read: {error}"))
        })
        .collect::<String>();
    assert!(!fixture_text.contains("copilot-token-canary"));

    let replay = ReplayProvider::load(candidate, directory.path())
        .await
        .unwrap_or_else(|error| panic!("Copilot replay must load: {error}"));
    let replayed = replay
        .stream(request("fixture-model"))
        .await
        .unwrap_or_else(|error| panic!("Copilot replay must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        serde_json::to_vec(&live)
            .unwrap_or_else(|error| panic!("live events must encode: {error}")),
        serde_json::to_vec(&replayed)
            .unwrap_or_else(|error| panic!("replay events must encode: {error}"))
    );
}

#[tokio::test]
async fn copilot_discovery_fails_closed_on_auth_and_policy_denials() {
    let disabled_catalog = r#"{"data":[{"model_picker_enabled":true,"id":"fixture-model","name":"Disabled","version":"fixture-model-2026-07-10","supported_endpoints":["/chat/completions"],"policy":{"state":"disabled"},"capabilities":{"family":"gpt","limits":{"max_context_window_tokens":100000,"max_output_tokens":4096,"max_prompt_tokens":90000},"supports":{"tool_calls":true}}}]}"#;
    for (response, expected) in [
        (
            status_response("401 Unauthorized"),
            ProviderErrorKind::Authentication,
        ),
        (
            status_response("403 Forbidden"),
            ProviderErrorKind::Authentication,
        ),
        (
            json_response(disabled_catalog),
            ProviderErrorKind::Unsupported,
        ),
    ] {
        let server = spawn_server("/", vec![response]);
        let origin = url::Url::parse(&server.endpoint)
            .unwrap_or_else(|error| panic!("loopback Copilot origin must parse: {error}"));
        let config = copilot_config("fixture-model");
        let runtime = ProviderFactory::with_backends(
            manager(TestEnvironment::default(), copilot_keychain()),
            ProxyEnvironment::default(),
            NetworkPolicy::Allow,
            pricing([("unrelated/model", false)]),
        )
        .with_github_copilot_test_origin("github-copilot", origin, "rottweiler-test-client")
        .build(&config)
        .unwrap_or_else(|error| panic!("loopback Copilot factory must build: {error}"));
        let error = runtime
            .provider("github-copilot/fixture-model")
            .unwrap_or_else(|| panic!("Copilot provider must exist"))
            .stream(request("fixture-model"))
            .await
            .err()
            .unwrap_or_else(|| panic!("Copilot discovery must fail closed"));
        assert_eq!(error.kind, expected);
        assert!(!format!("{error:?} {error}").contains("copilot-token-canary"));
        server
            .task
            .join()
            .unwrap_or_else(|_| panic!("Copilot denial server must join"));
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn copilot_discovered_vision_and_thinking_bypass_only_static_capability_guards() {
    let accepting = spawn_server(
        "/",
        vec![
            json_response(&copilot_catalog(true, &["none", "high"])),
            sse_response("vision-ok"),
            sse_response("thinking-ok"),
        ],
    );
    let accepting_origin = url::Url::parse(&accepting.endpoint)
        .unwrap_or_else(|error| panic!("accepting Copilot origin must parse: {error}"));
    let config = copilot_config("fixture-model");
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), copilot_keychain()),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("unrelated/model", false)]),
    )
    .with_github_copilot_test_origin("github-copilot", accepting_origin, "rottweiler-test-client")
    .build(&config)
    .unwrap_or_else(|error| panic!("accepting Copilot factory must build: {error}"));
    let provider = runtime
        .provider("github-copilot/fixture-model")
        .unwrap_or_else(|| panic!("Copilot provider must exist"));
    let mut image_request = request("fixture-model");
    image_request.turns[0].blocks = vec![Block::Image {
        media_type: "image/png".to_owned(),
        data: ImageRef::InlineBase64 {
            data: "aW1hZ2U=".to_owned(),
        },
    }];
    let image_events = provider
        .stream(image_request)
        .await
        .unwrap_or_else(|error| panic!("discovered vision must be accepted: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert!(image_events.iter().all(Result::is_ok));
    let mut thinking_request = request("fixture-model");
    thinking_request.thinking = ThinkingLevel::High;
    let thinking_events = provider
        .stream(thinking_request)
        .await
        .unwrap_or_else(|error| panic!("discovered thinking must be accepted: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert!(thinking_events.iter().all(Result::is_ok));
    let requests = accepting
        .task
        .join()
        .unwrap_or_else(|_| panic!("accepting Copilot server must join"));
    assert_eq!(requests.len(), 3);

    for (catalog, denied_request) in [
        {
            let mut request = request("fixture-model");
            request.turns[0].blocks = vec![Block::Image {
                media_type: "image/png".to_owned(),
                data: ImageRef::InlineBase64 {
                    data: "aW1hZ2U=".to_owned(),
                },
            }];
            (copilot_catalog(false, &["none"]), request)
        },
        {
            let mut request = request("fixture-model");
            request.thinking = ThinkingLevel::High;
            (copilot_catalog(false, &["none"]), request)
        },
    ] {
        let denying = spawn_server("/", vec![json_response(&catalog)]);
        let origin = url::Url::parse(&denying.endpoint)
            .unwrap_or_else(|error| panic!("denying Copilot origin must parse: {error}"));
        let runtime = ProviderFactory::with_backends(
            manager(TestEnvironment::default(), copilot_keychain()),
            ProxyEnvironment::default(),
            NetworkPolicy::Allow,
            pricing([("unrelated/model", false)]),
        )
        .with_github_copilot_test_origin("github-copilot", origin, "rottweiler-test-client")
        .build(&config)
        .unwrap_or_else(|error| panic!("denying Copilot factory must build: {error}"));
        let error = runtime
            .provider("github-copilot/fixture-model")
            .unwrap_or_else(|| panic!("Copilot provider must exist"))
            .stream(denied_request)
            .await
            .err()
            .unwrap_or_else(|| panic!("undiscovered capability must remain denied"));
        assert_eq!(error.kind, ProviderErrorKind::Unsupported);
        denying
            .task
            .join()
            .unwrap_or_else(|_| panic!("denying Copilot server must join"));
    }
}

#[tokio::test]
async fn copilot_dynamic_metadata_exposes_caps_and_nominal_credit_rates() {
    let server = spawn_server(
        "/",
        vec![json_response(&copilot_catalog(true, &["none", "high"]))],
    );
    let origin = url::Url::parse(&server.endpoint)
        .unwrap_or_else(|error| panic!("metadata Copilot origin must parse: {error}"));
    let config = copilot_config("fixture-model");
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), copilot_keychain()),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("unrelated/model", false)]),
    )
    .with_github_copilot_test_origin("github-copilot", origin, "rottweiler-test-client")
    .build(&config)
    .unwrap_or_else(|error| panic!("metadata Copilot factory must build: {error}"));
    let metadata = runtime
        .model_metadata("github-copilot/fixture-model")
        .await
        .unwrap_or_else(|error| panic!("dynamic Copilot metadata must resolve: {error}"));
    assert!(metadata.capabilities.tool_calling);
    assert!(metadata.capabilities.vision);
    assert!(metadata.capabilities.thinking);
    assert_eq!(metadata.capabilities.max_context_tokens, Some(100_000));
    assert_eq!(metadata.capabilities.max_output_tokens, Some(4_096));
    let micros_usd_per_credit = match metadata.accounting {
        ModelAccounting::AiCredits {
            micros_usd_per_credit,
        } => micros_usd_per_credit,
        other => panic!("Copilot metadata must be credit-accounted, got {other:?}"),
    };
    assert_eq!(micros_usd_per_credit, 10_000);
    let model_pricing = metadata
        .pricing
        .unwrap_or_else(|| panic!("authenticated Copilot credit rates must be present"));
    let table = PricingTable {
        source_url: "https://api.githubcopilot.com/models".to_owned(),
        snapshot_date: "2026-07-10".to_owned(),
        revision: "authenticated-copilot-fixture".to_owned(),
        models: BTreeMap::from([("copilot/fixture-model".to_owned(), model_pricing)]),
    };
    let cost = table
        .cost(
            "copilot/fixture-model",
            rw_providers::TokenUsage {
                input_tokens: 2_000,
                output_tokens: 500,
                cache_read_tokens: 1_000,
                ..rw_providers::TokenUsage::default()
            },
        )
        .unwrap_or_else(|error| panic!("nominal credit calculation must work: {error}"))
        .unwrap_or_else(|| panic!("nominal Copilot pricing must resolve"));
    assert_eq!(cost.total_micros_usd, 11_000);
    // 2 input batches * .25 + .5 output batches * 1 + 1 cache batch * .1
    // = 1.1 AI Credits, expressed exactly as an 11/10 rational.
    assert_eq!(cost.total_micros_usd * 10, micros_usd_per_credit * 11);
    server
        .task
        .join()
        .unwrap_or_else(|_| panic!("metadata Copilot server must join"));
}
