use super::*;

#[test]
fn opaque_router_route_prices_the_actual_failover_candidate() {
    let mut config = rw_types::config::Config::default();
    config.models.default = "fast".to_owned();
    config.models.aliases.insert(
        "fast".to_owned(),
        vec!["a/cheap".to_owned(), "b/expensive".to_owned()],
    );
    for (provider, port) in [("a", 1), ("b", 2)] {
        config.providers.insert(
            provider.to_owned(),
            ProviderConfig {
                kind: "openai_compatible".to_owned(),
                base_url: Some(format!("http://127.0.0.1:{port}/v1/chat/completions")),
                ..ProviderConfig::default()
            },
        );
    }
    let mut table = pricing([("a/cheap", false), ("b/expensive", false)]);
    table
        .models
        .get_mut("a/cheap")
        .unwrap_or_else(|| panic!("cheap pricing"))
        .output_per_million_micros_usd = 10;
    table
        .models
        .get_mut("b/expensive")
        .unwrap_or_else(|| panic!("expensive pricing"))
        .output_per_million_micros_usd = 100;
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestCredentialStore::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Deny,
        table,
    )
    .build(&config)
    .unwrap_or_else(|error| panic!("factory must build: {error}"));
    let usage = rw_providers::TokenUsage {
        output_tokens: 1_000_000,
        ..rw_providers::TokenUsage::default()
    };
    assert!(matches!(
        runtime.accounting_for_alias("fast", usage),
        Cost::Unavailable { .. }
    ));
    assert_eq!(
        runtime.accounting_for_route(Some("__model_00000000"), usage),
        Cost::Monetary {
            amount_micros: 10,
            currency: "USD".to_owned(),
        }
    );
    assert_eq!(
        runtime.accounting_for_route(Some("__model_00000001"), usage),
        Cost::Monetary {
            amount_micros: 100,
            currency: "USD".to_owned(),
        }
    );
}

#[tokio::test]
async fn explicit_provider_route_excludes_other_alias_candidates() {
    let mut config = extension_config("alpha/model-a");
    config.models.aliases.insert(
        "fast".to_owned(),
        vec!["alpha/model-a".to_owned(), "beta/model-b".to_owned()],
    );
    let runtime = extension_factory()
        .with_extension_providers([
            ("alpha/", extension_provider("alpha-private", None)),
            ("beta/", extension_provider("beta-private", None)),
        ])
        .build(&config)
        .unwrap_or_else(|error| panic!("two-provider extension runtime must build: {error}"));

    assert!(runtime.has_provider_for_alias("fast", "alpha"));
    assert!(runtime.has_provider_for_alias("fast", "beta"));
    assert!(!runtime.has_provider_for_alias("fast", "missing"));
    let events = runtime
        .stream_alias_provider("fast", "beta", request("ignored"))
        .unwrap_or_else(|error| panic!("explicit beta route must resolve: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(
        |event| matches!(event, Ok(ProviderEvent::TextDelta { text }) if text == "extension:model-b")
    ));
    assert!(events.iter().all(
        |event| !matches!(event, Ok(ProviderEvent::TextDelta { text }) if text == "extension:model-a")
    ));
    assert!(
        runtime
            .stream_alias_provider("fast", "missing", request("ignored"))
            .is_err()
    );
}

#[tokio::test]
async fn alias_fallback_message_start_uses_exact_provider_qualified_candidate() {
    let mut config = extension_config("alpha/model-a");
    config.models.aliases.insert(
        "fast".to_owned(),
        vec!["alpha/model-a".to_owned(), "beta/model-b".to_owned()],
    );
    let failing: Arc<dyn Provider> = Arc::new(StartFailProvider);
    let runtime = extension_factory()
        .with_retry_policy(RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            jitter_fraction: 0.0,
        })
        .with_extension_providers([
            ("alpha/", failing),
            ("beta/", extension_provider("beta-private", None)),
        ])
        .build(&config)
        .unwrap_or_else(|error| panic!("fallback runtime must build: {error}"));

    let events = runtime
        .stream_alias("fast", request("ignored"))
        .unwrap_or_else(|error| panic!("fallback stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|event| {
        matches!(event, Ok(ProviderEvent::MessageStart { model }) if model == "beta/model-b")
    }));
    assert!(!events.iter().any(|event| {
        matches!(event, Ok(ProviderEvent::MessageStart { model }) if model == "model-b" || model == "beta/beta/model-b")
    }));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn mixed_automatic_to_explicit_fallback_preserves_anthropic_cache_control() {
    let killed_listener = TcpListener::bind("127.0.0.1:0")
        .unwrap_or_else(|error| panic!("dead OpenAI listener must bind: {error}"));
    let killed_address = killed_listener
        .local_addr()
        .unwrap_or_else(|error| panic!("dead OpenAI address must resolve: {error}"));
    drop(killed_listener);
    let anthropic = spawn_server(
        "/v1/messages",
        (0..20).map(|_| anthropic_sse_response()).collect(),
    );
    let mut config = rw_types::config::Config::default();
    config.models.default = "fast".to_owned();
    config.models.aliases.insert(
        "fast".to_owned(),
        vec![
            "automatic/gpt-fixture".to_owned(),
            "explicit/claude-fixture".to_owned(),
        ],
    );
    config.providers.insert(
        "automatic".to_owned(),
        ProviderConfig {
            kind: "openai".to_owned(),
            base_url: Some(format!("http://{killed_address}/v1/responses")),
            api_key_env: Some("OPENAI_FIXTURE_KEY".to_owned()),
            ..ProviderConfig::default()
        },
    );
    config.providers.insert(
        "explicit".to_owned(),
        ProviderConfig {
            kind: "anthropic".to_owned(),
            base_url: Some(anthropic.endpoint.clone()),
            api_key_env: Some("ANTHROPIC_FIXTURE_KEY".to_owned()),
            ..ProviderConfig::default()
        },
    );
    let runtime = ProviderFactory::with_backends(
        manager(
            TestEnvironment(BTreeMap::from([
                ("OPENAI_FIXTURE_KEY".to_owned(), "openai-fixture".to_owned()),
                (
                    "ANTHROPIC_FIXTURE_KEY".to_owned(),
                    "anthropic-fixture".to_owned(),
                ),
            ])),
            TestCredentialStore::default(),
        ),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([
            ("openai/gpt-fixture", true),
            ("anthropic/claude-fixture", true),
        ]),
    )
    .with_retry_policy(RetryPolicy {
        max_attempts: 1,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
        jitter_fraction: 0.0,
    })
    .build(&config)
    .unwrap_or_else(|error| panic!("mixed provider factory must build: {error}"));
    assert_eq!(
        runtime
            .resolved_model("automatic/gpt-fixture")
            .unwrap_or_else(|| panic!("OpenAI model must resolve"))
            .capabilities()
            .cache_breakpoints,
        rw_providers::CacheBreakpointSupport::Automatic
    );
    assert_eq!(
        runtime
            .resolved_model("explicit/claude-fixture")
            .unwrap_or_else(|| panic!("Anthropic model must resolve"))
            .capabilities()
            .cache_breakpoints,
        rw_providers::CacheBreakpointSupport::Explicit
    );
    for turn in 0..20 {
        let mut routed = request("fast");
        for history in 0..turn {
            routed.turns.extend([
                Turn {
                    role: Role::Assistant,
                    blocks: vec![Block::Text {
                        text: format!("history answer {history}"),
                    }],
                    meta: TurnMeta::default(),
                },
                Turn {
                    role: Role::User,
                    blocks: vec![Block::Text {
                        text: format!("history question {history}"),
                    }],
                    meta: TurnMeta::default(),
                },
            ]);
        }
        routed.tools.push(ToolDefinition {
            name: "read_file".to_owned(),
            description: "Read a file".to_owned(),
            input_schema: json!({"type": "object"}),
        });
        routed.cache_hint = Some(CacheHint {
            stable_prefix_turns: 1,
            tools_in_prefix: true,
        });
        let events = runtime
            .stream_alias("fast", routed)
            .unwrap_or_else(|error| panic!("mixed alias must route: {error}"))
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(Result::is_ok));
    }
    let captured = anthropic
        .task
        .join()
        .unwrap_or_else(|_| panic!("Anthropic fallback server must join"));
    assert_eq!(captured.len(), 20);
    let bodies = captured
        .iter()
        .map(|request| {
            let (_, body) = request
                .split_once("\r\n\r\n")
                .unwrap_or_else(|| panic!("Anthropic request must contain a body"));
            serde_json::from_str::<serde_json::Value>(body)
                .unwrap_or_else(|error| panic!("Anthropic request body must parse: {error}"))
        })
        .collect::<Vec<_>>();
    let mut stable_wire_prefix = bodies[0]["messages"][0].clone();
    stable_wire_prefix["content"][0]
        .as_object_mut()
        .unwrap_or_else(|| panic!("stable message block must be an object"))
        .remove("cache_control");
    let stable_wire_tools = bodies[0]["tools"].clone();
    for body in &bodies {
        let mut first_message = body["messages"][0].clone();
        first_message["content"][0]
            .as_object_mut()
            .unwrap_or_else(|| panic!("stable message block must be an object"))
            .remove("cache_control");
        assert_eq!(first_message, stable_wire_prefix);
        assert_eq!(body["tools"], stable_wire_tools);
        assert_eq!(
            body["tools"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        let final_content = body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .and_then(|message| message["content"].as_array())
            .and_then(|content| content.last())
            .unwrap_or_else(|| panic!("conversation cache boundary must exist"));
        assert_eq!(final_content["cache_control"], json!({"type": "ephemeral"}));
    }
    assert!(
        bodies[19]["messages"]
            .as_array()
            .is_some_and(|messages| messages.len() > 20)
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn model_caps_binding_conflicts_and_alias_invariants_fail_closed() {
    let endpoint = "http://127.0.0.1:9/v1/chat/completions";
    let mut runtime_config = config(endpoint, &["fixture/model-a", "fixture/model-b"]);
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestCredentialStore::default()),
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
            TestCredentialStore::default(),
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
            manager(TestEnvironment::default(), TestCredentialStore::default()),
            ProxyEnvironment::default(),
            NetworkPolicy::Deny,
            pricing([("fixture/model-a", false)]),
        )
        .build(&invalid)
        .is_err()
    );

    let unknown = config(endpoint, &["fixture/unknown-model"]);
    let unknown_runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestCredentialStore::default()),
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
