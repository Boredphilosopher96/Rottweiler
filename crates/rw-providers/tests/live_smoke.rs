//! Opt-in, credentialed acceptance capture for the two M1 API families.
//!
//! This test is ignored by default because it makes paid remote requests. It
//! validates every required environment variable before opening either stream,
//! then records one minimal tool call per provider and replays the normalized
//! IR without retaining a live adapter.

use std::{collections::BTreeSet, path::Path, sync::Arc};

use futures_util::StreamExt;
use rw_providers::{
    AnthropicConfig, AnthropicProvider, AuthMaterial, CacheBreakpointSupport, FinishReason,
    FixtureRedactor, NetworkPolicy, OpenAiCompatibleConfig, OpenAiCompatibleProvider,
    OpenAiWireMode, Provider, ProviderError, ProviderEvent, ProviderRequest, Recorder,
    ReplayProvider, Secret, StaticAuth, ThinkingLevel, ToolChoice, ToolDefinition,
    deny_outbound_network_for_process,
};
use rw_types::{Block, Role, Turn, TurnMeta};
use serde_json::json;
use url::Url;

const LIVE_ACKNOWLEDGEMENT: &str = "accept-paid-requests";
const ANTHROPIC_PROVIDER: &str = "anthropic-live-smoke";
const OPENAI_PROVIDER: &str = "openai-live-smoke";

struct LiveSettings {
    fixture_root: std::path::PathBuf,
    anthropic_key: String,
    anthropic_model: String,
    openai_key: String,
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
            std::path::PathBuf::from(required_environment("RW_LIVE_SMOKE_FIXTURE_DIR"));
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
            .unwrap_or_else(|| panic!("workspace root must contain crates/rw-providers"));
        let workspace = std::fs::canonicalize(workspace)
            .unwrap_or_else(|error| panic!("workspace root must have a real path: {error}"));
        assert!(
            !fixture_root.starts_with(workspace),
            "RW_LIVE_SMOKE_FIXTURE_DIR must be outside the repository so paid live fixtures cannot be committed accidentally"
        );
        Self {
            fixture_root,
            anthropic_key: required_environment("ANTHROPIC_API_KEY"),
            anthropic_model: required_environment("RW_LIVE_ANTHROPIC_MODEL"),
            openai_key: required_environment("OPENAI_API_KEY"),
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
                "missing {name}; see crates/rw-providers/tests/live_smoke.rs for the complete opt-in invocation (credentials must be exported, never pasted into test output)"
            )
        })
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

fn anthropic_live(key: &str) -> Arc<dyn Provider> {
    let endpoint = Url::parse("https://api.anthropic.com/v1/messages")
        .unwrap_or_else(|error| panic!("built-in Anthropic endpoint must parse: {error}"));
    Arc::new(
        AnthropicProvider::new(AnthropicConfig {
            name: ANTHROPIC_PROVIDER.to_owned(),
            endpoint,
            auth: Arc::new(StaticAuth::new(AuthMaterial::ApiKey(Secret::new(key)))),
            proxy: None,
            proxy_authentication: None,
            network_policy: NetworkPolicy::Allow,
            thinking_strategy: None,
            max_context_tokens: None,
            max_output_tokens: None,
        })
        .unwrap_or_else(|error| panic!("Anthropic live adapter must construct: {error}")),
    )
}

fn openai_live(key: &str) -> Arc<dyn Provider> {
    let endpoint = Url::parse("https://api.openai.com/v1/responses")
        .unwrap_or_else(|error| panic!("built-in OpenAI endpoint must parse: {error}"));
    Arc::new(
        OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
            name: OPENAI_PROVIDER.to_owned(),
            endpoint,
            auth: Arc::new(StaticAuth::new(AuthMaterial::ApiKey(Secret::new(key)))),
            proxy: None,
            proxy_authentication: None,
            network_policy: NetworkPolicy::Allow,
            wire_mode: OpenAiWireMode::Responses,
            tool_calling: true,
            cache_breakpoints: CacheBreakpointSupport::Automatic,
            supported_reasoning_efforts: Vec::new(),
            supports_vision: false,
            max_context_tokens: None,
            max_output_tokens: None,
        })
        .unwrap_or_else(|error| panic!("OpenAI live adapter must construct: {error}")),
    )
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
        "{provider} must start exactly one live_smoke_ping tool call"
    );
    let completed = events
        .iter()
        .filter(|item| {
            matches!(
                item,
                Ok(ProviderEvent::ToolCallEnd { id, arguments })
                    if starts.contains(id.as_str()) && arguments == &json!({"value": "ok"})
            )
        })
        .count();
    assert_eq!(
        completed, 1,
        "{provider} must complete exactly one live_smoke_ping tool call"
    );
    assert!(
        events.iter().any(|item| matches!(
            item,
            Ok(ProviderEvent::Finished {
                reason: FinishReason::ToolCalls
            })
        )),
        "{provider} did not terminate with the normalized tool-calls finish reason"
    );
}

async fn assert_fixtures_redacted(directory: &Path, secret: &str) {
    let mut entries = tokio::fs::read_dir(directory)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "fixture directory {} must be readable: {error}",
                directory.display()
            )
        });
    let mut count = 0_u64;
    while let Some(entry) = entries
        .next_entry()
        .await
        .unwrap_or_else(|error| panic!("fixture directory entry must be readable: {error}"))
    {
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            count += 1;
            let bytes = tokio::fs::read(entry.path())
                .await
                .unwrap_or_else(|error| panic!("recorded fixture must be readable: {error}"));
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes()),
                "recorded fixture contained a registered credential"
            );
        }
    }
    assert!(count > 0, "recorder did not write a JSON fixture");
}

async fn capture_and_replay(
    live_provider: Arc<dyn Provider>,
    request: ProviderRequest,
    directory: &Path,
    secret: String,
) {
    let provider_name = live_provider.name().to_owned();
    let recorder = Recorder::new(
        live_provider,
        directory,
        FixtureRedactor::new([secret.clone()]),
    );
    let live = collect(&recorder, request.clone()).await;
    recorder
        .flush()
        .await
        .unwrap_or_else(|error| panic!("{provider_name} fixture must flush: {error}"));
    assert_complete_tool_call(&live, &provider_name);
    assert_fixtures_redacted(directory, &secret).await;

    // The only object capable of live transport is dropped before replay. The
    // replay provider has no HTTP client or network-policy escape hatch.
    drop(recorder);
    drop(secret);
    let _network_guard = deny_outbound_network_for_process();
    let replay = ReplayProvider::load(&provider_name, directory)
        .await
        .unwrap_or_else(|error| panic!("live-smoke replay provider must load: {error}"));
    let offline = collect(&replay, request).await;
    let live_bytes = serde_json::to_vec(&live)
        .unwrap_or_else(|error| panic!("live normalized IR must serialize: {error}"));
    let offline_bytes = serde_json::to_vec(&offline)
        .unwrap_or_else(|error| panic!("replayed normalized IR must serialize: {error}"));
    assert_eq!(
        offline_bytes, live_bytes,
        "{provider_name} replay did not reproduce the normalized IR byte-for-byte"
    );
}

/// Exact invocation (with API keys already exported in the environment):
///
/// ```text
/// RW_LIVE_SMOKE=accept-paid-requests \
/// RW_LIVE_SMOKE_FIXTURE_DIR=/absolute/path/to/m1-live-fixtures \
/// RW_LIVE_ANTHROPIC_MODEL=<current-tool-capable-model> \
/// RW_LIVE_OPENAI_MODEL=<current-tool-capable-model> \
/// cargo test --locked -p rw-providers --test live_smoke \
///   credentialed_provider_record_and_offline_replay -- --ignored --exact --nocapture
/// ```
#[tokio::test]
#[ignore = "makes two explicitly authorized, paid provider requests"]
async fn credentialed_provider_record_and_offline_replay() {
    // Load every required value before the first request so a partial setup
    // cannot accidentally spend against only one provider.
    let settings = LiveSettings::from_environment();
    capture_and_replay(
        anthropic_live(&settings.anthropic_key),
        tool_request(settings.anthropic_model),
        &settings.fixture_root.join("anthropic"),
        settings.anthropic_key,
    )
    .await;
    capture_and_replay(
        openai_live(&settings.openai_key),
        tool_request(settings.openai_model),
        &settings.fixture_root.join("openai"),
        settings.openai_key,
    )
    .await;
}
