use super::*;

#[tokio::test]
async fn live_catalog_excludes_stale_alias_candidate_before_inference() {
    let streamed_models = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn Provider> = Arc::new(AuthoritativeCatalogProvider {
        streamed_models: Arc::clone(&streamed_models),
    });
    let mut config = rw_types::config::Config::default();
    config.providers.clear();
    config.models.default = "fast".to_owned();
    config.models.aliases = BTreeMap::from([(
        "fast".to_owned(),
        vec!["live/retired".to_owned(), "live/current".to_owned()],
    )]);
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestCredentialStore::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Deny,
        PricingTable::default(),
    )
    .with_extension_providers([("live/", provider)])
    .build(&config)
    .unwrap_or_else(|error| panic!("runtime must compose: {error}"));

    ModelDriver::prepare_model(&runtime, "fast")
        .await
        .unwrap_or_else(|error| panic!("live alias must validate: {error}"));
    let events = ModelDriver::stream(&runtime, "fast", request("ignored"))
        .unwrap_or_else(|error| panic!("validated stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;

    assert!(events.iter().all(Result::is_ok));
    assert_eq!(
        streamed_models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        ["current"],
        "the configured-but-undiscovered retired model must never receive inference"
    );
}

#[tokio::test]
async fn provider_reactivation_revalidates_cached_alias_and_concrete_catalog_authority() {
    let current = json_response(r#"{"data":[{"id":"model-a"}]}"#);
    let retired = json_response(r#"{"data":[{"id":"replacement"}]}"#);
    let server = spawn_server(
        "/v1/chat/completions",
        vec![current.clone(), current, retired.clone(), retired],
    );
    let config = config(&server.endpoint, &["fixture/model-a"]);
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestCredentialStore::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("fixture/model-a", false)]),
    )
    .build(&config)
    .unwrap_or_else(|error| panic!("runtime must compose: {error}"));

    ModelDriver::prepare_model(&runtime, "fixture/model-a")
        .await
        .unwrap_or_else(|error| panic!("initial concrete route must validate: {error}"));
    ModelDriver::prepare_model(&runtime, "fast")
        .await
        .unwrap_or_else(|error| panic!("initial alias route must validate: {error}"));

    runtime
        .activate_provider("fixture")
        .unwrap_or_else(|error| panic!("provider must reactivate: {error}"));
    let Err(concrete_error) = ModelDriver::prepare_model(&runtime, "fixture/model-a").await else {
        panic!("reactivation must invalidate concrete catalog authority");
    };
    assert!(
        concrete_error
            .to_string()
            .contains("not in the live catalog")
    );
    let Err(alias_error) = ModelDriver::prepare_model(&runtime, "fast").await else {
        panic!("reactivation must invalidate alias catalog authority");
    };
    assert!(alias_error.to_string().contains("not in the live catalog"));

    let requests = server
        .task
        .join()
        .unwrap_or_else(|_| panic!("catalog server must join"));
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|request| request.starts_with("GET /v1/models "))
    );
}

#[tokio::test]
async fn configured_unaliased_concrete_model_rebinds_after_runtime_restart_and_dispatches() {
    let models = json_response(r#"{"data":[{"id":"new-model"}]}"#);
    let server = spawn_server(
        "/v1/chat/completions",
        vec![models.clone(), models, sse_response("dynamic-ok")],
    );
    let mut config = config(
        "http://127.0.0.1:1/v1/chat/completions",
        &["fixture/model-a"],
    );
    config.providers.insert(
        "extra".to_owned(),
        ProviderConfig {
            kind: "openai_compatible".to_owned(),
            base_url: Some(server.endpoint.clone()),
            ..ProviderConfig::default()
        },
    );
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestCredentialStore::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("fixture/model-a", false), ("extra/new-model", false)]),
    )
    .build(&config)
    .unwrap_or_else(|error| panic!("factory must build: {error}"));

    runtime
        .prepare_concrete_model("extra/new-model")
        .await
        .unwrap_or_else(|error| panic!("concrete model must bind: {error}"));
    drop(runtime);

    let resumed = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestCredentialStore::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("fixture/model-a", false), ("extra/new-model", false)]),
    )
    .build(&config)
    .unwrap_or_else(|error| panic!("resumed factory must build: {error}"));
    resumed
        .prepare_concrete_model("extra/new-model")
        .await
        .unwrap_or_else(|error| panic!("persisted concrete model must rebind: {error}"));
    let events = ModelDriver::stream(&resumed, "extra/new-model", request("ignored"))
        .unwrap_or_else(|error| panic!("concrete stream must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok));
    assert!(events.iter().any(|event| {
        matches!(event, Ok(ProviderEvent::TextDelta { text }) if text == "dynamic-ok")
    }));
    let requests = server
        .task
        .join()
        .unwrap_or_else(|_| panic!("dynamic server must join"));
    assert!(requests[0].starts_with("GET /v1/models "));
    assert!(requests[1].starts_with("GET /v1/models "));
    assert!(requests[2].starts_with("POST /v1/chat/completions "));
}

#[tokio::test]
async fn stalled_concrete_discovery_is_bounded_and_existing_alias_remains_usable() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .unwrap_or_else(|error| panic!("stall listener must bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("stall address: {error}"));
    let mut config = config(
        "http://127.0.0.1:1/v1/chat/completions",
        &["fixture/model-a"],
    );
    config.providers.insert(
        "extra".to_owned(),
        ProviderConfig {
            kind: "openai_compatible".to_owned(),
            base_url: Some(format!("http://{address}/v1/chat/completions")),
            ..ProviderConfig::default()
        },
    );
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestCredentialStore::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("fixture/model-a", false)]),
    )
    .with_model_discovery_timeout(Duration::from_millis(25))
    .build(&config)
    .unwrap_or_else(|error| panic!("factory must build: {error}"));
    let started = Instant::now();
    let Err(error) = runtime.prepare_concrete_model("extra/new-model").await else {
        panic!("stalled discovery must reject");
    };
    assert!(error.to_string().contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(ModelDriver::has_model_alias(&runtime, "fast"));
    drop(listener);
}

#[tokio::test]
async fn configured_route_without_live_catalog_defers_unknown_tool_capability_to_endpoint() {
    let server = spawn_server(
        "/v1/chat/completions",
        vec![
            status_response("501 Not Implemented"),
            sse_response("configured-route-ok"),
        ],
    );
    let config = config(&server.endpoint, &["fixture/model-a"]);
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestCredentialStore::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        PricingTable::default(),
    )
    .build(&config)
    .unwrap_or_else(|error| panic!("runtime must compose: {error}"));

    ModelDriver::prepare_model(&runtime, "fast")
        .await
        .unwrap_or_else(|error| panic!("configured route must remain selectable: {error}"));
    let mut routed = request("fast");
    routed.tools.push(ToolDefinition {
        name: "read".to_owned(),
        description: "Read a workspace file".to_owned(),
        input_schema: json!({"type": "object"}),
    });
    let events = ModelDriver::stream(&runtime, "fast", routed)
        .unwrap_or_else(|error| panic!("configured route must start: {error}"))
        .collect::<Vec<_>>()
        .await;

    assert!(events.iter().all(Result::is_ok));
    let requests = server
        .task
        .join()
        .unwrap_or_else(|_| panic!("fixture server must join"));
    assert!(requests[0].starts_with("GET /v1/models "));
    assert!(requests[1].starts_with("POST /v1/chat/completions "));
    assert!(requests[1].contains("\"tools\""));
}

#[test]
#[allow(clippy::too_many_lines)]
fn pricing_precedence_is_user_then_provider_then_models_dev() {
    let mut table = pricing([("custom/model-a", false), ("fixture/model-a", false)]);
    table
        .models
        .get_mut("custom/model-a")
        .unwrap_or_else(|| panic!("catalog model must exist"))
        .output_per_million_micros_usd = 10;
    table
        .models
        .get_mut("fixture/model-a")
        .unwrap_or_else(|| panic!("catalog model must exist"))
        .output_per_million_micros_usd = 10;

    let catalog_runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestCredentialStore::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Deny,
        table.clone(),
    )
    .build(&config(
        "http://127.0.0.1:1/v1/chat/completions",
        &["fixture/model-a"],
    ))
    .unwrap_or_else(|error| panic!("catalog-priced route must build: {error}"));
    let catalog_model = catalog_runtime
        .resolved_model("fixture/model-a")
        .unwrap_or_else(|| panic!("catalog model must resolve"));
    assert_eq!(
        catalog_model.pricing_source(),
        Some(ModelPricingSource::ModelsDev)
    );
    assert_eq!(
        catalog_model
            .pricing()
            .map(|pricing| pricing.output_per_million_micros_usd),
        Some(10)
    );

    let discovered_pricing = ModelPricing {
        output_per_million_micros_usd: 20,
        ..table.models["custom/model-a"].clone()
    };
    let metadata = ProviderModelMetadata {
        capabilities: extension_capabilities(),
        pricing: Some(discovered_pricing),
        accounting: UsageAccounting::ApiDollars,
    };
    let build_extension = |config: &rw_types::config::Config| {
        ProviderFactory::with_backends(
            manager(TestEnvironment::default(), TestCredentialStore::default()),
            ProxyEnvironment::default(),
            NetworkPolicy::Deny,
            table.clone(),
        )
        .with_extension_providers([(
            "custom/",
            extension_provider("private-plugin", Some(metadata.clone())),
        )])
        .build(config)
    };
    let discovered_runtime = build_extension(&extension_config("custom/model-a"))
        .unwrap_or_else(|error| panic!("discovered-priced extension must build: {error}"));
    let discovered_model = discovered_runtime
        .resolved_model("custom/model-a")
        .unwrap_or_else(|| panic!("discovered model must resolve"));
    assert_eq!(
        discovered_model.pricing_source(),
        Some(ModelPricingSource::ProviderDiscovered)
    );
    assert_eq!(
        discovered_model
            .pricing()
            .map(|pricing| pricing.output_per_million_micros_usd),
        Some(20)
    );
    assert_eq!(
        discovered_runtime.accounting_for_alias(
            "fast",
            rw_providers::TokenUsage {
                output_tokens: 1_000_000,
                ..rw_providers::TokenUsage::default()
            },
        ),
        Cost::Monetary {
            amount_micros: 20,
            currency: "USD".to_owned(),
        }
    );

    let mut user_config = extension_config("custom/model-a");
    user_config.providers.insert(
        "custom".to_owned(),
        ProviderConfig {
            kind: "extension".to_owned(),
            pricing: BTreeMap::from([(
                "model-a".to_owned(),
                declared_pricing(0.000_03, 0.000_03, None, None),
            )]),
            ..ProviderConfig::default()
        },
    );
    let user_runtime = build_extension(&user_config)
        .unwrap_or_else(|error| panic!("user-priced extension must build: {error}"));
    let user_model = user_runtime
        .resolved_model("custom/model-a")
        .unwrap_or_else(|| panic!("user-priced model must resolve"));
    assert_eq!(
        user_model.pricing_source(),
        Some(ModelPricingSource::UserConfig)
    );
    assert_eq!(
        user_model
            .pricing()
            .map(|pricing| pricing.output_per_million_micros_usd),
        Some(30)
    );
    assert_eq!(
        user_runtime.accounting_for_alias(
            "fast",
            rw_providers::TokenUsage {
                output_tokens: 1_000_000,
                ..rw_providers::TokenUsage::default()
            },
        ),
        Cost::Monetary {
            amount_micros: 30,
            currency: "USD".to_owned(),
        }
    );
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
        manager(TestEnvironment::default(), TestCredentialStore::default()),
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
        manager(TestEnvironment::default(), TestCredentialStore::default()),
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
