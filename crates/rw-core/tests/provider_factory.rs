use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::StreamExt;
use rw_core::{ModelAccounting, ModelDriver, ModelPricingSource, ProviderFactory};
use rw_plugin_protocol::MAX_PROVIDER_ALIAS_PREFIX_BYTES;
use rw_providers::{
    BoxEventStream, CacheBreakpointSupport, CacheHint, Capabilities, DiscoveredModel,
    DiscoveredProviderCatalog, FinishReason, ModelPricing, NetworkPolicy, PricingTable, Provider,
    ProviderError, ProviderErrorKind, ProviderEvent, ProviderModelMetadata, ProviderRequest,
    ProxyEnvironment, Recorder, ReplayProvider, RetryPolicy, ToolChoice, ToolDefinition,
    UsageAccounting, WireMode,
};
use rw_store::credentials::{
    CREDENTIAL_VAULT_ID, CredentialEnvironment, CredentialError, CredentialManager,
    CredentialReference, CredentialStore, CredentialStoreUnavailable, Secret,
};
use rw_types::{
    Block, Cost, ImageRef, Role, ToolCallId, ToolOutput, ToolOutputPart, Turn, TurnMeta,
    config::{ProviderAuthScheme, ProviderConfig, ProviderModelPricingConfig, ThinkingLevel},
};
use serde_json::json;
use tempfile::tempdir;

const API_CANARY: &str = "rw-api-secret-canary";
const OAUTH_CANARY: &str = "rw-oauth-secret-canary";
const REFRESH_CANARY: &str = "rw-refresh-secret-canary";
const ROTATED_CANARY: &str = "rw-rotated-refresh-canary";
const REFRESHED_ACCESS_CANARY: &str = "rw-refreshed-access-canary";
const HEADER_CANARY: &str = "rw-header-secret-canary";

struct ExtensionFixtureProvider {
    private_name: String,
    capabilities: Capabilities,
    metadata: Option<ProviderModelMetadata>,
    metadata_by_model: BTreeMap<String, ProviderModelMetadata>,
}

struct StartFailProvider;

struct AuthoritativeCatalogProvider {
    streamed_models: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for AuthoritativeCatalogProvider {
    fn name(&self) -> &'static str {
        "authoritative-catalog-fixture"
    }

    fn capabilities(&self) -> Capabilities {
        extension_capabilities()
    }

    async fn discover_models(&self) -> Result<Option<DiscoveredProviderCatalog>, ProviderError> {
        Ok(Some(DiscoveredProviderCatalog {
            provider: "live".to_owned(),
            models: vec![DiscoveredModel {
                id: "current".to_owned(),
                display_name: Some("Current".to_owned()),
                description: None,
                capabilities: Some(extension_capabilities()),
                pricing: None,
            }],
        }))
    }

    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        self.streamed_models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.model.clone());
        Ok(Box::pin(futures_util::stream::iter([
            Ok(ProviderEvent::MessageStart {
                model: request.model,
            }),
            Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop,
            }),
        ])))
    }
}

#[async_trait]
impl Provider for StartFailProvider {
    fn name(&self) -> &'static str {
        "private-start-failure"
    }

    fn capabilities(&self) -> Capabilities {
        extension_capabilities()
    }

    async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        Err(ProviderError::new(
            ProviderErrorKind::Server,
            "fixture start failure",
        ))
    }
}

#[async_trait]
impl Provider for ExtensionFixtureProvider {
    fn name(&self) -> &str {
        &self.private_name
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn model_metadata(&self) -> Result<Option<ProviderModelMetadata>, ProviderError> {
        Ok(self.metadata.clone())
    }

    fn cached_model_metadata(&self) -> Option<ProviderModelMetadata> {
        self.metadata.clone()
    }

    fn cached_model_metadata_for(&self, model: &str) -> Option<ProviderModelMetadata> {
        self.metadata_by_model
            .get(model)
            .cloned()
            .or_else(|| self.metadata.clone())
    }

    async fn discover_models(&self) -> Result<Option<DiscoveredProviderCatalog>, ProviderError> {
        Ok(Some(DiscoveredProviderCatalog {
            provider: "private-extension".to_owned(),
            models: vec![
                DiscoveredModel {
                    id: "model-a".to_owned(),
                    display_name: Some("Model A".to_owned()),
                    description: None,
                    capabilities: Some(
                        self.cached_model_metadata_for("model-a")
                            .map_or_else(|| self.capabilities.clone(), |value| value.capabilities),
                    ),
                    pricing: self
                        .cached_model_metadata_for("model-a")
                        .as_ref()
                        .and_then(|value| value.pricing.clone()),
                },
                DiscoveredModel {
                    id: "new-model".to_owned(),
                    display_name: Some("New Model".to_owned()),
                    description: None,
                    capabilities: Some(
                        self.cached_model_metadata_for("new-model")
                            .map_or_else(|| self.capabilities.clone(), |value| value.capabilities),
                    ),
                    pricing: self
                        .cached_model_metadata_for("new-model")
                        .and_then(|value| value.pricing),
                },
            ],
        }))
    }

    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        Ok(Box::pin(futures_util::stream::iter([
            Ok(ProviderEvent::MessageStart {
                model: request.model.clone(),
            }),
            Ok(ProviderEvent::TextDelta {
                text: format!("extension:{}", request.model),
            }),
            Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop,
            }),
        ])))
    }
}

#[derive(Clone, Default)]
struct TestEnvironment(BTreeMap<String, String>);

impl CredentialEnvironment for TestEnvironment {
    fn get(&self, name: &str) -> Result<Option<String>, CredentialError> {
        Ok(self.0.get(name).cloned())
    }
}

#[derive(Clone, Default)]
struct TestCredentialStore(Arc<Mutex<BTreeMap<String, String>>>);

impl TestCredentialStore {
    fn insert(&self, identifier: &str, value: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if identifier == CREDENTIAL_VAULT_ID {
            values.insert(identifier.to_owned(), value.to_owned());
            return;
        }
        let vault = values
            .entry(CREDENTIAL_VAULT_ID.to_owned())
            .or_insert_with(|| "version = 1\n[credentials]\n".to_owned());
        let _ = writeln!(vault, "{identifier:?} = {value:?}");
    }
}

impl CredentialStore for TestCredentialStore {
    fn get(&self, identifier: &str) -> Result<Option<Secret<String>>, CredentialStoreUnavailable> {
        self.0
            .lock()
            .map_err(|_| CredentialStoreUnavailable)
            .map(|values| values.get(identifier).cloned().map(Secret::new))
    }

    fn set(
        &self,
        identifier: &str,
        secret: &Secret<String>,
    ) -> Result<(), CredentialStoreUnavailable> {
        self.0
            .lock()
            .map_err(|_| CredentialStoreUnavailable)?
            .insert(identifier.to_owned(), secret.expose_secret().clone());
        Ok(())
    }
}

#[derive(Clone)]
struct UnavailableOnSetCredentialStore(TestCredentialStore);

impl CredentialStore for UnavailableOnSetCredentialStore {
    fn get(&self, identifier: &str) -> Result<Option<Secret<String>>, CredentialStoreUnavailable> {
        self.0.get(identifier)
    }

    fn set(
        &self,
        _identifier: &str,
        _secret: &Secret<String>,
    ) -> Result<(), CredentialStoreUnavailable> {
        Err(CredentialStoreUnavailable)
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

fn anthropic_sse_response() -> String {
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-fixture\",\"usage\":{\"input_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"cached fallback\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
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
                        supports_vision: false,
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

fn declared_pricing(
    input: f64,
    output: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
) -> ProviderModelPricingConfig {
    ProviderModelPricingConfig {
        currency: Some("USD".to_owned()),
        input_per_million: serde_json::Number::from_f64(input),
        output_per_million: serde_json::Number::from_f64(output),
        cache_read_per_million: cache_read.and_then(serde_json::Number::from_f64),
        cache_write_per_million: cache_write.and_then(serde_json::Number::from_f64),
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

fn subscription_credential_store() -> TestCredentialStore {
    let credential_store = TestCredentialStore::default();
    credential_store.insert(
        "providers.fixture.openai_codex",
        r#"{"version":1,"access_token":"subscription-access-canary","refresh_token":"subscription-refresh-canary","account_id":"acct-fixture"}"#,
    );
    credential_store
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

fn copilot_credential_store() -> TestCredentialStore {
    let credential_store = TestCredentialStore::default();
    credential_store.insert(
        "providers.github-copilot.github_copilot",
        r#"{"version":1,"oauth_client_id":"rottweiler-test-client","access_token":"copilot-token-canary"}"#,
    );
    credential_store
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
    credential_store: TestCredentialStore,
) -> Arc<CredentialManager<TestEnvironment, TestCredentialStore>> {
    Arc::new(CredentialManager::with_backends(
        environment,
        credential_store,
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
        tool_choice: rw_providers::ToolChoice::Auto {},
        max_output_tokens: 32,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    }
}

fn extension_capabilities() -> Capabilities {
    Capabilities {
        tool_calling: true,
        vision: false,
        thinking: false,
        cache_breakpoints: CacheBreakpointSupport::None,
        max_context_tokens: Some(32_768),
        max_output_tokens: Some(2_048),
        wire_mode: WireMode::NormalizedReplay,
    }
}

fn extension_provider(
    private_name: &str,
    metadata: Option<ProviderModelMetadata>,
) -> Arc<dyn Provider> {
    Arc::new(ExtensionFixtureProvider {
        private_name: private_name.to_owned(),
        capabilities: extension_capabilities(),
        metadata,
        metadata_by_model: BTreeMap::new(),
    })
}

fn extension_config(candidate: &str) -> rw_types::config::Config {
    let mut config = rw_types::config::Config::default();
    "fast".clone_into(&mut config.models.default);
    config
        .models
        .aliases
        .insert("fast".to_owned(), vec![candidate.to_owned()]);
    config
}

fn extension_factory() -> ProviderFactory<TestEnvironment, TestCredentialStore> {
    ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestCredentialStore::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Deny,
        pricing([("unrelated/catalog-model", false)]),
    )
}

#[path = "provider_factory/catalog.rs"]
mod catalog;
#[path = "provider_factory/copilot.rs"]
mod copilot;
#[path = "provider_factory/credentials.rs"]
mod credentials;
#[path = "provider_factory/extensions.rs"]
mod extensions;
#[path = "provider_factory/routing.rs"]
mod routing;

#[path = "provider_factory/admission.rs"]
mod admission;
use admission::invocation;
