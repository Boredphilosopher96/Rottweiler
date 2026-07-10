//! Paid, ignored two-family live smoke using Rottweiler's credential store.
//!
//! Normal test runs never execute this test. Even when ignored tests are
//! requested, every non-secret input and both provider credentials are resolved
//! before either production adapter is built or any request can be sent.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

use futures_util::StreamExt;
use rw_core::{ProviderFactory, ProviderRuntime};
use rw_providers::{
    FinishReason, FixtureRedactor, NetworkPolicy, PricingTable, Provider, ProviderError,
    ProviderEvent, ProviderRequest, ProxyEnvironment, Recorder, ReplayProvider, ThinkingLevel,
    ToolChoice, ToolDefinition, default_models_path, deny_outbound_network_for_process,
};
use rw_store::{config::ConfigLoader, credentials::CredentialManager};
use rw_types::{
    Block, Role, Turn, TurnMeta,
    config::{Config, ThinkingLevel as ConfigThinkingLevel},
};
use serde_json::json;

const LIVE_ACKNOWLEDGEMENT: &str = "accept-paid-requests";
const ANTHROPIC_PROVIDER: &str = "anthropic";
const OPENAI_PROVIDER: &str = "openai";

struct LiveSettings {
    fixture_root: std::path::PathBuf,
    anthropic_model: String,
    openai_model: String,
}

impl LiveSettings {
    fn from_environment() -> Self {
        let acknowledgement = required_environment("RW_LIVE_SMOKE");
        assert_eq!(
            acknowledgement, LIVE_ACKNOWLEDGEMENT,
            "RW_LIVE_SMOKE must equal {LIVE_ACKNOWLEDGEMENT:?}; this explicit opt-in confirms that the ignored test may make paid API requests"
        );
        let fixture_root =
            validate_fixture_root(&required_environment("RW_LIVE_SMOKE_FIXTURE_DIR"));
        Self {
            fixture_root,
            anthropic_model: required_environment("RW_LIVE_ANTHROPIC_MODEL"),
            openai_model: required_environment("RW_LIVE_OPENAI_MODEL"),
        }
    }
}

fn required_environment(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            panic!(
                "missing {name}; see the live_smoke_credentials test documentation for the complete paid opt-in invocation"
            )
        })
}

fn validate_fixture_root(value: &str) -> std::path::PathBuf {
    let fixture_root = std::path::PathBuf::from(value);
    assert!(
        fixture_root.is_absolute(),
        "RW_LIVE_SMOKE_FIXTURE_DIR must be an absolute path outside the repository"
    );
    let fixture_root = std::fs::canonicalize(&fixture_root).unwrap_or_else(|error| {
        panic!(
            "RW_LIVE_SMOKE_FIXTURE_DIR must already exist so its real path can be checked: {error}"
        )
    });
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| panic!("workspace root must contain crates/rw-core"));
    let workspace = std::fs::canonicalize(workspace)
        .unwrap_or_else(|error| panic!("workspace root must have a real path: {error}"));
    assert!(
        !fixture_root.starts_with(workspace),
        "RW_LIVE_SMOKE_FIXTURE_DIR must be outside the repository so paid fixtures cannot be committed accidentally"
    );
    fixture_root
}

fn tool_request(model: String) -> ProviderRequest {
    ProviderRequest {
        model,
        turns: vec![Turn {
            role: Role::User,
            blocks: vec![Block::Text {
                text: "Call live_smoke_ping exactly once with {\"value\":\"ok\"}. Do not write text or call any other tool."
                    .to_owned(),
            }],
            meta: TurnMeta::default(),
        }],
        tools: vec![ToolDefinition {
            name: "live_smoke_ping".to_owned(),
            description: "Return a fixed canary value for a provider integration test".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": { "value": { "type": "string", "enum": ["ok"] } },
                "required": ["value"],
                "additionalProperties": false,
            }),
        }],
        tool_choice: ToolChoice::Named {
            name: "live_smoke_ping".to_owned(),
        },
        max_output_tokens: 64,
        temperature: None,
        thinking: ThinkingLevel::Off,
    }
}

struct PreparedRuntime {
    runtime: ProviderRuntime,
    anthropic_candidate: String,
    openai_candidate: String,
}

async fn prepare_runtime(settings: &LiveSettings) -> PreparedRuntime {
    let loader = ConfigLoader::from_environment()
        .unwrap_or_else(|error| panic!("user config discovery must succeed: {error}"));
    let credentials_path = loader.credentials_path();
    let mut config = loader
        .load()
        .unwrap_or_else(|error| panic!("user config must load before live smoke: {error}"))
        .config;
    let (anthropic_candidate, openai_candidate) = inject_live_models(
        &mut config,
        &settings.anthropic_model,
        &settings.openai_model,
    );

    let factory = ProviderFactory::with_backends(
        Arc::new(CredentialManager::system(credentials_path)),
        ProxyEnvironment::capture(),
        NetworkPolicy::Allow,
        load_pricing().await,
    );
    // Factory construction is the credential/proxy preflight. It resolves all
    // selected providers before returning any runnable adapter, so a missing
    // second key cannot produce a paid first-provider request.
    let runtime = factory
        .build(&config)
        .unwrap_or_else(|error| panic!("both live providers must preflight: {error}"));
    for candidate in [&anthropic_candidate, &openai_candidate] {
        assert!(runtime.provider(candidate).is_some());
        assert!(
            runtime
                .resolved_model(candidate)
                .is_some_and(|model| model.capabilities().tool_calling),
            "live model {candidate:?} must be present in models.toml with tool support; run `rw models refresh` before the paid smoke"
        );
    }
    PreparedRuntime {
        runtime,
        anthropic_candidate,
        openai_candidate,
    }
}

fn inject_live_models(
    config: &mut Config,
    anthropic_model: &str,
    openai_model: &str,
) -> (String, String) {
    let anthropic_candidate = format!("{ANTHROPIC_PROVIDER}/{anthropic_model}");
    let openai_candidate = format!("{OPENAI_PROVIDER}/{openai_model}");
    config.models.aliases = BTreeMap::from([
        (
            "live-anthropic".to_owned(),
            vec![anthropic_candidate.clone()],
        ),
        ("live-openai".to_owned(), vec![openai_candidate.clone()]),
    ]);
    "live-anthropic".clone_into(&mut config.models.default);
    config.models.thinking = BTreeMap::from([
        ("live-anthropic".to_owned(), ConfigThinkingLevel::Off),
        ("live-openai".to_owned(), ConfigThinkingLevel::Off),
    ]);
    (anthropic_candidate, openai_candidate)
}

#[test]
fn live_alias_injection_replaces_stale_thinking_configuration() {
    let mut config = Config::default();
    config.models.aliases.insert(
        "old-alias".to_owned(),
        vec!["old-provider/old-model".to_owned()],
    );
    config
        .models
        .thinking
        .insert("old-alias".to_owned(), ConfigThinkingLevel::High);

    let (anthropic, openai) = inject_live_models(&mut config, "current-a", "current-o");

    assert_eq!(config.models.default, "live-anthropic");
    assert_eq!(config.models.aliases.len(), 2);
    assert_eq!(config.models.aliases["live-anthropic"], [anthropic]);
    assert_eq!(config.models.aliases["live-openai"], [openai]);
    assert_eq!(
        config.models.thinking,
        BTreeMap::from([
            ("live-anthropic".to_owned(), ConfigThinkingLevel::Off),
            ("live-openai".to_owned(), ConfigThinkingLevel::Off),
        ])
    );
}

async fn load_pricing() -> PricingTable {
    let path = default_models_path()
        .unwrap_or_else(|error| panic!("user model-catalog path must resolve: {error}"));
    if path.is_file() {
        PricingTable::load(&path)
            .await
            .unwrap_or_else(|error| panic!("refreshed user model catalog must load: {error}"))
    } else {
        PricingTable::bundled()
            .unwrap_or_else(|error| panic!("bundled model catalog must load: {error}"))
    }
}

async fn collect(
    provider: &dyn Provider,
    request: ProviderRequest,
) -> Vec<Result<ProviderEvent, ProviderError>> {
    provider
        .stream(request)
        .await
        .unwrap_or_else(|error| panic!("credentialed live-smoke stream must start: {error}"))
        .collect()
        .await
}

fn assert_complete_tool_call(events: &[Result<ProviderEvent, ProviderError>], provider: &str) {
    for item in events {
        if let Err(error) = item {
            panic!(
                "{provider} live-smoke stream returned {}: {error}",
                error.kind
            );
        }
    }
    let starts = events
        .iter()
        .filter_map(|item| match item {
            Ok(ProviderEvent::ToolCallStart { id, name }) if name == "live_smoke_ping" => {
                Some(id.as_str())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        starts.len(),
        1,
        "{provider} must start exactly one named tool call"
    );
    assert_eq!(
        events
            .iter()
            .filter(|item| matches!(
                item,
                Ok(ProviderEvent::ToolCallEnd { id, arguments })
                    if starts.contains(id.as_str()) && arguments == &json!({"value": "ok"})
            ))
            .count(),
        1,
        "{provider} must complete exactly one named tool call"
    );
    assert!(events.iter().any(|item| matches!(
        item,
        Ok(ProviderEvent::Finished {
            reason: FinishReason::ToolCalls
        })
    )));
}

async fn assert_fixtures_redacted(directory: &Path, redactor: &FixtureRedactor) {
    let mut entries = tokio::fs::read_dir(directory)
        .await
        .unwrap_or_else(|error| panic!("fixture directory must be readable: {error}"));
    let mut count = 0_u64;
    while let Some(entry) = entries
        .next_entry()
        .await
        .unwrap_or_else(|error| panic!("fixture entry must be readable: {error}"))
    {
        if entry
            .path()
            .extension()
            .is_some_and(|value| value == "json")
        {
            count += 1;
            let bytes = tokio::fs::read(entry.path())
                .await
                .unwrap_or_else(|error| panic!("fixture must be readable: {error}"));
            let text = std::str::from_utf8(&bytes)
                .unwrap_or_else(|error| panic!("fixture must be UTF-8: {error}"));
            assert!(
                !redactor.contains_registered_secret(text),
                "fixture retained credential material registered by the production factory"
            );
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .unwrap_or_else(|error| panic!("fixture must remain valid redacted JSON: {error}"));
        }
    }
    assert!(count > 0, "recorder did not write a JSON fixture");
}

async fn capture_and_replay(
    live_provider: Arc<dyn Provider>,
    request: ProviderRequest,
    directory: &Path,
    redactor: FixtureRedactor,
) {
    let provider_name = live_provider.name().to_owned();
    let verification_redactor = redactor.clone();
    let recorder = Recorder::new(live_provider, directory, redactor);
    let live = collect(&recorder, request.clone()).await;
    recorder
        .flush()
        .await
        .unwrap_or_else(|error| panic!("{provider_name} fixture must flush: {error}"));
    assert_complete_tool_call(&live, &provider_name);
    assert_fixtures_redacted(directory, &verification_redactor).await;

    drop(recorder);
    let _network_guard = deny_outbound_network_for_process();
    let replay = ReplayProvider::load(&provider_name, directory)
        .await
        .unwrap_or_else(|error| panic!("{provider_name} replay must load: {error}"));
    let offline = collect(&replay, request).await;
    assert_eq!(
        serde_json::to_vec(&offline)
            .unwrap_or_else(|error| panic!("replay IR must serialize: {error}")),
        serde_json::to_vec(&live).unwrap_or_else(|error| panic!("live IR must serialize: {error}")),
        "{provider_name} replay must be byte-identical"
    );
}

/// Exact credential-backed invocation:
///
/// ```text
/// # User config must define providers `anthropic` (kind `anthropic`) and
/// # `openai` (kind `openai`). Refresh models.toml if the chosen model ids are
/// # newer than the bundled catalog.
/// rw auth set-key anthropic
/// rw auth set-key openai
/// RW_LIVE_SMOKE=accept-paid-requests \
/// RW_LIVE_SMOKE_FIXTURE_DIR=/absolute/existing/path/outside/repository \
/// RW_LIVE_ANTHROPIC_MODEL=<current-tool-capable-model> \
/// RW_LIVE_OPENAI_MODEL=<current-tool-capable-model> \
/// cargo test --locked -p rw-core --test live_smoke_credentials \
///   credential_store_two_family_record_and_offline_replay -- --ignored --exact --nocapture
/// ```
#[tokio::test]
#[ignore = "makes two explicitly authorized, paid provider requests"]
async fn credential_store_two_family_record_and_offline_replay() {
    let settings = LiveSettings::from_environment();
    let prepared = prepare_runtime(&settings).await;
    let redactor = prepared.runtime.fixture_redactor();
    assert!(
        redactor.registered_secret_count() >= 2,
        "factory must register both provider credentials before any live request"
    );
    let anthropic = prepared
        .runtime
        .provider(&prepared.anthropic_candidate)
        .unwrap_or_else(|| panic!("factory-preflighted Anthropic provider must exist"));
    let openai = prepared
        .runtime
        .provider(&prepared.openai_candidate)
        .unwrap_or_else(|| panic!("factory-preflighted OpenAI provider must exist"));
    capture_and_replay(
        anthropic,
        tool_request(settings.anthropic_model),
        &settings.fixture_root.join("anthropic"),
        redactor.clone(),
    )
    .await;
    capture_and_replay(
        openai,
        tool_request(settings.openai_model),
        &settings.fixture_root.join("openai"),
        redactor,
    )
    .await;
}
