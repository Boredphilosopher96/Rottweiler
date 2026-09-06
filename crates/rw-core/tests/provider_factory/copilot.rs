use super::*;

#[test]
fn copilot_kind_is_credit_accounted_redacted_and_conflict_closed() {
    let build = |config: &rw_types::config::Config| {
        ProviderFactory::with_backends(
            manager(TestEnvironment::default(), copilot_credential_store()),
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
        manager(TestEnvironment::default(), copilot_credential_store()),
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
        manager(TestEnvironment::default(), copilot_credential_store()),
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
    required.tool_choice = ToolChoice::Required {};
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
        manager(TestEnvironment::default(), copilot_credential_store()),
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
            manager(TestEnvironment::default(), copilot_credential_store()),
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
        manager(TestEnvironment::default(), copilot_credential_store()),
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
            manager(TestEnvironment::default(), copilot_credential_store()),
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
        manager(TestEnvironment::default(), copilot_credential_store()),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("unrelated/model", false)]),
    )
    .with_github_copilot_test_origin("github-copilot", origin, "rottweiler-test-client")
    .build(&config)
    .unwrap_or_else(|error| panic!("metadata Copilot factory must build: {error}"));
    let undiscovered = runtime.context_metadata("fast");
    assert_eq!(undiscovered.max_context_tokens, None);
    assert_eq!(undiscovered.max_output_tokens, None);
    let metadata = runtime
        .model_metadata("github-copilot/fixture-model")
        .await
        .unwrap_or_else(|error| panic!("dynamic Copilot metadata must resolve: {error}"));
    assert!(metadata.capabilities.tool_calling);
    assert!(metadata.capabilities.vision);
    assert!(metadata.capabilities.thinking);
    assert_eq!(metadata.capabilities.max_context_tokens, Some(100_000));
    assert_eq!(metadata.capabilities.max_output_tokens, Some(4_096));
    let discovered = runtime.context_metadata("fast");
    assert_eq!(discovered.max_context_tokens, Some(100_000));
    assert_eq!(discovered.max_output_tokens, Some(4_096));
    assert_eq!(
        discovered.cache_breakpoints,
        Some(rw_providers::CacheBreakpointSupport::None)
    );
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
    let runtime_cost = runtime.accounting_for_alias(
        "fast",
        rw_providers::TokenUsage {
            input_tokens: 2_000,
            output_tokens: 500,
            cache_read_tokens: 1_000,
            ..rw_providers::TokenUsage::default()
        },
    );
    assert_eq!(
        runtime_cost,
        rw_types::Cost::AiCredits {
            credits_micros: 1_100_000,
            nominal_amount_micros: Some("11000".to_owned()),
            currency: Some("USD".to_owned()),
        }
    );
    // = 1.1 AI Credits, expressed exactly as an 11/10 rational.
    assert_eq!(cost.total_micros_usd * 10, micros_usd_per_credit * 11);
    server
        .task
        .join()
        .unwrap_or_else(|_| panic!("metadata Copilot server must join"));
}
