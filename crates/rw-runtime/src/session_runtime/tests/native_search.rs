#![cfg(test)]
use super::AliasAwareWebSearchModel;
use super::Arc;
use super::CacheHint;
use super::CancellationToken;
use super::CapturingModel;
use super::Config;
use super::FixtureWebSearcher;
use super::ModelDriver;
use super::Mutex;
use super::PromptRecordingModel;
use super::PromptShapeJournal;
use super::ProviderRequest;
use super::RuntimeWebSearcher;
use super::ThinkingLevel;
use super::ToolChoice;
use super::ToolDefinition;
use super::WebSearchRequest;
use super::WebSearchResponse;
use super::WebSearchSource;
use super::historical_tool_registry;
use super::provider_model_for_alias;
use super::provider_native_search_available;
use super::tempdir;
use super::test_provider_invocation;

#[tokio::test]
async fn runtime_websearch_resolves_native_backend_for_each_turn_alias() {
    let aliases = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&aliases);
    let searcher = RuntimeWebSearcher::new(None);
    searcher.bind_native_resolver(Some(Arc::new(move |alias| {
        seen.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(alias.to_owned());
        Some(native_factory())
    })));
    for alias in ["fast", "slow", "command-override"] {
        searcher
            .bind(alias, test_provider_invocation())
            .expect("accounted binding")
            .search(
                WebSearchRequest {
                    model_alias: Some(alias.to_owned()),
                    query: "query".to_owned(),
                    max_results: 5,
                    recency_days: None,
                    allowed_domains: Vec::new(),
                },
                CancellationToken::default(),
            )
            .await
            .expect("native search");
    }
    assert_eq!(
        aliases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        ["fast", "slow", "command-override"]
    );
}

#[tokio::test]
async fn mixed_alias_websearch_schema_is_reachable_for_the_selected_model() {
    let mut config = Config::default();
    config
        .providers
        .entry("anthropic".to_owned())
        .or_default()
        .kind = "anthropic".to_owned();
    config
        .providers
        .entry("openai".to_owned())
        .or_default()
        .kind = "openai".to_owned();
    config
        .models
        .aliases
        .insert("local".to_owned(), vec!["anthropic/claude".to_owned()]);
    config
        .models
        .aliases
        .insert("cloud".to_owned(), vec!["openai/gpt-5".to_owned()]);
    assert!(provider_native_search_available(&config));

    let native = Arc::new(RuntimeWebSearcher::new(None));
    native.bind_native_resolver(Some(Arc::new(|alias| {
        (alias == "cloud").then(native_factory)
    })));
    let captured = Arc::new(Mutex::new(None));
    let model = AliasAwareWebSearchModel::wrap(
        Arc::new(CapturingModel {
            request: Arc::clone(&captured),
        }),
        Some(&native),
    );
    let request = || ProviderRequest {
        model: "fixture".to_owned(),
        turns: Vec::new(),
        tools: vec![ToolDefinition {
            name: "websearch".to_owned(),
            description: "search".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 128,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    };

    drop(
        model
            .stream("local", request(), test_provider_invocation())
            .expect("local request"),
    );
    assert!(
        captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|request| request.tools.is_empty())
    );
    drop(
        model
            .stream("cloud", request(), test_provider_invocation())
            .expect("cloud request"),
    );
    assert!(
        captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|request| request.tools.iter().any(|tool| tool.name == "websearch"))
    );
    model
        .native_web_searcher("cloud", test_provider_invocation())
        .expect("accounted turn binding")
        .search(
            WebSearchRequest {
                model_alias: Some("cloud".to_owned()),
                query: "reachable".to_owned(),
                max_results: 1,
                recency_days: None,
                allowed_domains: Vec::new(),
            },
            CancellationToken::default(),
        )
        .await
        .expect("selected native backend works");
}

#[test]
fn configured_websearch_schema_is_exposed_for_an_unsupported_alias() {
    let configured = Arc::new(RuntimeWebSearcher::new(Some(Arc::new(FixtureWebSearcher(
        WebSearchResponse {
            source: WebSearchSource::ConfiguredApi,
            results: Vec::new(),
        },
    )))));
    let configured_capture = Arc::new(Mutex::new(None));
    let configured_model = AliasAwareWebSearchModel::wrap(
        Arc::new(CapturingModel {
            request: Arc::clone(&configured_capture),
        }),
        Some(&configured),
    );
    let request = ProviderRequest {
        model: "fixture".to_owned(),
        turns: Vec::new(),
        tools: vec![ToolDefinition {
            name: "websearch".to_owned(),
            description: "search".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 128,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    };
    drop(
        configured_model
            .stream("local", request, test_provider_invocation())
            .expect("configured fallback request"),
    );
    assert!(
        configured_capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|request| request.tools.iter().any(|tool| tool.name == "websearch"))
    );
}

#[test]
fn unsupported_alias_prompt_shape_omits_dead_websearch_schema() {
    let root = tempdir().expect("prompt shape root");
    let session_id = "alias-websearch-shape";
    std::fs::create_dir_all(root.path().join("sessions").join(session_id))
        .expect("session directory");
    let journal =
        Arc::new(PromptShapeJournal::open(root.path(), session_id).expect("prompt shape journal"));
    journal.set_active_turn(rw_core::TurnId("1".to_owned()));
    let captured = Arc::new(Mutex::new(None));
    let recording: Arc<dyn ModelDriver> = Arc::new(PromptRecordingModel {
        inner: Arc::new(CapturingModel {
            request: Arc::clone(&captured),
        }),
        journal: Arc::clone(&journal),
    });
    let searcher = Arc::new(RuntimeWebSearcher::new(None));
    searcher.bind_native_resolver(Some(Arc::new(|_| None)));
    let model = AliasAwareWebSearchModel::wrap(recording, Some(&searcher));
    let request = ProviderRequest {
        model: "local".to_owned(),
        turns: Vec::new(),
        tools: vec![ToolDefinition {
            name: "websearch".to_owned(),
            description: "search".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 128,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: Some(CacheHint {
            stable_prefix_turns: 0,
            tools_in_prefix: true,
        }),
    };
    drop(
        model
            .stream("local", request, test_provider_invocation())
            .expect("filtered request"),
    );

    let (profile, _) = journal
        .shape_for_turn(1)
        .expect("shape lookup")
        .expect("recorded shape");
    assert!(profile.tools.is_empty());
    assert_eq!(profile.cache_hint, None);
    drop(journal);
    let reopened =
        PromptShapeJournal::open(root.path(), session_id).expect("filtered prompt shape reopens");
    let (profile, _) = reopened
        .shape_for_turn(1)
        .expect("reopened shape lookup")
        .expect("reopened shape");
    assert!(profile.tools.is_empty());
    assert_eq!(profile.cache_hint, None);
    assert!(
        historical_tool_registry(&profile)
            .expect("historical tools")
            .resolve("websearch")
            .is_none()
    );
    assert!(
        captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|request| request.tools.is_empty())
    );
}

#[test]
fn replay_native_search_resolves_recorded_provider_model_not_alias() {
    let mut config = Config::default();
    config.models.aliases.insert(
        "fast".to_owned(),
        vec!["other/first".to_owned(), "recorded/gpt-actual".to_owned()],
    );
    assert_eq!(
        provider_model_for_alias(&config, "fast", "recorded").as_deref(),
        Some("gpt-actual")
    );
}

fn native_factory() -> rw_core::ProviderNativeWebSearchFactory {
    rw_core::ProviderNativeWebSearchFactory::single(Arc::new(NativeProvider), "fixture".into())
        .expect("route")
        .expect("capability")
}
struct NativeProvider;
#[async_trait::async_trait]
impl rw_providers::Provider for NativeProvider {
    async fn settle_effects(&self) -> Result<(), rw_providers::ProviderError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "native-fixture"
    }
    fn capabilities(&self) -> rw_providers::Capabilities {
        rw_providers::Capabilities {
            tool_calling: true,
            vision: false,
            thinking: false,
            cache_breakpoints: rw_providers::CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: rw_providers::WireMode::OpenAiResponses,
        }
    }
    fn native_web_search_capability(&self) -> rw_providers::NativeWebSearchCapability {
        rw_providers::NativeWebSearchCapability::Supported
    }
    async fn stream(
        &self,
        _request: ProviderRequest,
    ) -> Result<rw_providers::BoxEventStream, rw_providers::ProviderError> {
        Ok(Box::pin(futures_util::stream::iter([Ok(
            rw_providers::ProviderEvent::Finished {
                reason: rw_providers::FinishReason::Stop,
            },
        )])))
    }
}
