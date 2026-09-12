use super::*;

#[tokio::test]
async fn approved_extension_alias_stream_is_model_bound_and_replay_compatible() {
    let private_name = "private-adapter-secret-name";
    let runtime = extension_factory()
        .with_extension_providers([("custom/", extension_provider(private_name, None))])
        .build(&extension_config("custom/model-a"))
        .unwrap_or_else(|error| panic!("extension factory must build: {error}"));

    let events = runtime
        .stream_alias("fast", request("model-a"), invocation())
        .unwrap_or_else(|error| panic!("extension alias must route: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|event| {
        matches!(event, Ok(ProviderEvent::TextDelta { text }) if text == "extension:model-a")
    }));
    let bound = runtime
        .provider("custom/model-a")
        .unwrap_or_else(|| panic!("extension candidate must be registered"));
    assert_eq!(bound.name(), "custom/model-a");
    assert_ne!(bound.name(), private_name);
    let mismatch = bound
        .stream(request("model-b"))
        .await
        .err()
        .unwrap_or_else(|| panic!("model-bound extension must reject another model"));
    assert_eq!(mismatch.kind, ProviderErrorKind::InvalidRequest);

    let directory = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let recorder = Recorder::new(bound, directory.path(), runtime.fixture_redactor());
    let live = recorder
        .stream(request("model-a"))
        .await
        .unwrap_or_else(|error| panic!("extension recording must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    recorder
        .flush()
        .await
        .unwrap_or_else(|error| panic!("extension recording must flush: {error}"));
    let replay = ReplayProvider::load("custom/model-a", directory.path())
        .await
        .unwrap_or_else(|error| panic!("extension replay must load: {error}"));
    let replayed = replay
        .stream(request("model-a"))
        .await
        .unwrap_or_else(|error| panic!("extension replay must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(live, replayed);
}

#[tokio::test]
async fn approved_unaliased_extension_is_catalogued_bindable_and_dispatchable() {
    let runtime = extension_factory()
        .with_extension_providers([
            ("alpha/", extension_provider("alpha-private", None)),
            ("custom/", extension_provider("custom-private", None)),
        ])
        .build(&extension_config("alpha/model-a"))
        .unwrap_or_else(|error| panic!("extension runtime must build: {error}"));

    let catalog = rw_core::ModelCatalogSource::discover(&runtime)
        .await
        .unwrap_or_else(|error| panic!("session catalog must discover: {error}"));
    assert!(catalog.providers.iter().any(|provider| {
        provider.name == "custom" && provider.reachable && provider.model_count == 2
    }));
    assert!(catalog.models.iter().any(|model| {
        model.id == "custom/new-model" && model.available && model.aliases.is_empty()
    }));
    assert!(
        !serde_json::to_string(&catalog)
            .unwrap_or_else(|error| panic!("catalog must encode: {error}"))
            .contains("custom-private")
    );

    runtime
        .prepare_concrete_model("custom/new-model")
        .await
        .unwrap_or_else(|error| panic!("live extension model must bind: {error}"));
    let events = ModelDriver::stream(
        &runtime,
        "custom/new-model",
        request("ignored"),
        invocation(),
    )
    .unwrap_or_else(|error| panic!("concrete extension stream must start: {error}"))
    .collect::<Vec<_>>()
    .await;
    assert!(events.iter().any(|event| {
        matches!(event, Ok(ProviderEvent::TextDelta { text }) if text == "extension:new-model")
    }));
}

#[tokio::test]
async fn extension_metadata_is_preserved_and_unknown_pricing_stays_unpriced() {
    let capabilities = Capabilities {
        vision: true,
        max_context_tokens: Some(65_536),
        ..extension_capabilities()
    };
    let metadata = ProviderModelMetadata {
        capabilities: capabilities.clone(),
        pricing: Some(ModelPricing {
            display_name: "Custom Model".to_owned(),
            max_context_tokens: Some(65_536),
            max_output_tokens: Some(2_048),
            supports_tools: true,
            supports_thinking: false,
            supports_vision: false,
            reasoning_efforts: Vec::new(),
            input_per_million_micros_usd: 4,
            output_per_million_micros_usd: 8,
            cache_read_per_million_micros_usd: None,
            cache_write_per_million_micros_usd: None,
            reasoning_per_million_micros_usd: None,
        }),
        accounting: UsageAccounting::ApiDollars,
    };
    let runtime = extension_factory()
        .with_extension_providers([(
            "custom/",
            extension_provider("private-plugin", Some(metadata.clone())),
        )])
        .build(&extension_config("custom/model-a"))
        .unwrap_or_else(|error| panic!("metadata extension must build: {error}"));
    let resolved = runtime
        .resolved_model("custom/model-a")
        .unwrap_or_else(|| panic!("extension model must resolve"));
    assert_eq!(resolved.provider(), "custom");
    assert_eq!(resolved.capabilities(), &capabilities);
    assert_eq!(resolved.pricing(), metadata.pricing.as_ref());
    assert_eq!(resolved.accounting(), UsageAccounting::ApiDollars);
    assert_eq!(
        runtime
            .model_metadata("custom/model-a")
            .await
            .unwrap_or_else(|error| panic!("extension metadata must resolve: {error}")),
        metadata
    );
    assert_eq!(
        runtime.accounting_for_alias(
            "fast",
            rw_providers::TokenUsage {
                output_tokens: 1_000_000,
                ..rw_providers::TokenUsage::default()
            },
        ),
        Cost::Monetary {
            amount_micros: 8,
            currency: "USD".to_owned(),
        }
    );

    let unknown = extension_factory()
        .with_extension_providers([("custom/", extension_provider("private-plugin", None))])
        .build(&extension_config("custom/model-a"))
        .unwrap_or_else(|error| panic!("unpriced extension must build: {error}"));
    let resolved = unknown
        .resolved_model("custom/model-a")
        .unwrap_or_else(|| panic!("unpriced extension model must resolve"));
    assert_eq!(resolved.capabilities(), &extension_capabilities());
    assert_eq!(resolved.pricing(), None);
    assert_eq!(resolved.accounting(), UsageAccounting::UnpricedApi);
    assert!(matches!(
        unknown.accounting_for_alias("fast", rw_providers::TokenUsage::default()),
        Cost::Unavailable { .. }
    ));
}

#[tokio::test]
async fn multi_model_extension_binds_each_models_cached_metadata() {
    let metadata =
        |display_name: &str, capabilities: Capabilities, input, output| ProviderModelMetadata {
            pricing: Some(ModelPricing {
                display_name: display_name.to_owned(),
                max_context_tokens: capabilities.max_context_tokens,
                max_output_tokens: capabilities.max_output_tokens,
                supports_tools: capabilities.tool_calling,
                supports_thinking: capabilities.thinking,
                supports_vision: capabilities.vision,
                reasoning_efforts: Vec::new(),
                input_per_million_micros_usd: input,
                output_per_million_micros_usd: output,
                cache_read_per_million_micros_usd: None,
                cache_write_per_million_micros_usd: None,
                reasoning_per_million_micros_usd: None,
            }),
            accounting: UsageAccounting::ApiDollars,
            capabilities,
        };
    let text_capabilities = Capabilities {
        tool_calling: false,
        ..extension_capabilities()
    };
    let vision_capabilities = Capabilities {
        vision: true,
        thinking: true,
        ..extension_capabilities()
    };
    let text_metadata = metadata("Text", text_capabilities.clone(), 1, 2);
    let vision_metadata = metadata("Vision", vision_capabilities.clone(), 3, 4);
    let provider: Arc<dyn Provider> = Arc::new(ExtensionFixtureProvider {
        private_name: "private-multi-model".to_owned(),
        capabilities: Capabilities {
            tool_calling: false,
            ..extension_capabilities()
        },
        metadata: None,
        metadata_by_model: BTreeMap::from([
            ("model-a".to_owned(), text_metadata.clone()),
            ("new-model".to_owned(), vision_metadata.clone()),
        ]),
    });
    let mut config = extension_config("custom/model-a");
    config.models.aliases.insert(
        "fast".to_owned(),
        vec!["custom/model-a".to_owned(), "custom/new-model".to_owned()],
    );
    let runtime = extension_factory()
        .with_extension_providers([("custom/", provider)])
        .build(&config)
        .unwrap_or_else(|error| panic!("multi-model extension must build: {error}"));

    let text = runtime
        .resolved_model("custom/model-a")
        .unwrap_or_else(|| panic!("text model must resolve"));
    assert_eq!(text.capabilities(), &text_capabilities);
    assert_eq!(text.pricing(), text_metadata.pricing.as_ref());
    let vision = runtime
        .resolved_model("custom/new-model")
        .unwrap_or_else(|| panic!("vision model must resolve"));
    assert_eq!(vision.capabilities(), &vision_capabilities);
    assert_eq!(vision.pricing(), vision_metadata.pricing.as_ref());
    assert_eq!(
        runtime
            .model_metadata("custom/new-model")
            .await
            .unwrap_or_else(|error| panic!("vision metadata must resolve: {error}")),
        vision_metadata
    );
}

#[test]
fn extension_alias_prefixes_reject_collisions_overlap_and_unregistered_candidates() {
    let provider = || extension_provider("private-plugin", None);

    let mut built_in_collision = extension_config("custom/model-a");
    built_in_collision.providers.insert(
        "custom".to_owned(),
        ProviderConfig {
            kind: "openai_compatible".to_owned(),
            base_url: Some("http://127.0.0.1:1/v1/chat/completions".to_owned()),
            ..ProviderConfig::default()
        },
    );
    let collision = extension_factory()
        .with_extension_providers([("custom/", provider())])
        .build(&built_in_collision)
        .err()
        .unwrap_or_else(|| panic!("built-in prefix collision must fail"));
    assert!(collision.to_string().contains("collides"));

    let overlap = extension_factory()
        .with_extension_providers([("custom/", provider()), ("custom/", provider())])
        .build(&extension_config("custom/model-a"))
        .err()
        .unwrap_or_else(|| panic!("overlapping extension prefixes must fail"));
    assert!(overlap.to_string().contains("overlaps"));

    let unregistered = extension_factory()
        .with_extension_providers([("custom/", provider())])
        .build(&extension_config("other/model-a"))
        .err()
        .unwrap_or_else(|| panic!("unregistered alias must fail"));
    assert!(unregistered.to_string().contains("unconfigured provider"));

    let invalid = extension_factory()
        .with_extension_providers([("Custom/", provider())])
        .build(&extension_config("custom/model-a"))
        .err()
        .unwrap_or_else(|| panic!("non-canonical extension prefix must fail"));
    let diagnostic = format!("{invalid:?} {invalid}");
    assert!(!diagnostic.contains("private-plugin"));
}

#[test]
fn extension_alias_prefix_uses_the_plugin_protocol_length_limit() {
    let prefix = format!("{}/", "a".repeat(MAX_PROVIDER_ALIAS_PREFIX_BYTES - 1));
    let candidate = format!("{prefix}model-a");
    extension_factory()
        .with_extension_providers([(prefix, extension_provider("private-plugin", None))])
        .build(&extension_config(&candidate))
        .unwrap_or_else(|error| panic!("protocol-valid extension prefix must compose: {error}"));

    let too_long = format!("{}/", "a".repeat(MAX_PROVIDER_ALIAS_PREFIX_BYTES));
    let error = extension_factory()
        .with_extension_providers([(too_long.clone(), extension_provider("private-plugin", None))])
        .build(&extension_config(&format!("{too_long}model-a")))
        .err()
        .unwrap_or_else(|| panic!("overlong extension prefix must fail"));
    assert!(error.to_string().contains("bounded canonical"));
}
