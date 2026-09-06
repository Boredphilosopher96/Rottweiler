use super::*;

#[test]
fn declared_gateway_pricing_accounts_for_all_reported_token_classes() {
    let mut config = config(
        "http://127.0.0.1:1/v1/chat/completions",
        &["fixture/gateway-only-model"],
    );
    config
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"))
        .pricing
        .insert(
            "gateway-only-model".to_owned(),
            declared_pricing(2.0, 8.0, Some(0.5), Some(3.0)),
        );
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), TestCredentialStore::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Deny,
        PricingTable::default(),
    )
    .build(&config)
    .unwrap_or_else(|error| panic!("declared gateway pricing must compose: {error}"));
    let resolved = runtime
        .resolved_model("fixture/gateway-only-model")
        .unwrap_or_else(|| panic!("gateway model must resolve"));
    assert_eq!(
        resolved.pricing_source(),
        Some(ModelPricingSource::UserConfig)
    );
    assert_eq!(resolved.accounting(), UsageAccounting::ApiDollars);
    assert_eq!(
        runtime.accounting_for_alias(
            "fast",
            rw_providers::TokenUsage {
                input_tokens: 1_000_000,
                output_tokens: 500_000,
                cache_read_tokens: 250_000,
                cache_write_tokens: 100_000,
                reasoning_tokens: 0,
            },
        ),
        Cost::Monetary {
            amount_micros: 6_425_000,
            currency: "USD".to_owned(),
        }
    );
}

#[tokio::test]
async fn newly_stored_provider_credential_activates_catalog_selection_and_dispatch() {
    let models = json_response(r#"{"data":[{"id":"new-model"}]}"#);
    let server = spawn_server(
        "/v1/chat/completions",
        vec![models.clone(), models, sse_response("activated-ok")],
    );
    let mut config = extension_config("local/model-a");
    config.providers.insert(
        "extra".to_owned(),
        ProviderConfig {
            kind: "openai_compatible".to_owned(),
            base_url: Some(server.endpoint.clone()),
            api_key_credential: Some("extra-api-key".to_owned()),
            ..ProviderConfig::default()
        },
    );
    let credentials = manager(TestEnvironment::default(), TestCredentialStore::default());
    let runtime = ProviderFactory::with_backends(
        Arc::clone(&credentials),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("extra/new-model", false)]),
    )
    .with_extension_providers([("local/", extension_provider("local-private", None))])
    .build(&config)
    .unwrap_or_else(|error| panic!("runtime must start without optional credential: {error}"));

    let before = rw_core::ModelCatalogSource::discover(&runtime)
        .await
        .unwrap_or_else(|error| panic!("initial catalog must remain usable: {error}"));
    assert!(
        !before
            .providers
            .iter()
            .any(|provider| provider.name == "extra")
    );
    assert_eq!(runtime.fixture_redactor().registered_secret_count(), 0);

    credentials
        .store(
            &CredentialReference::new("extra-api-key"),
            &Secret::new("newly-stored-secret".to_owned()),
        )
        .unwrap_or_else(|error| panic!("credential must store: {error}"));
    runtime
        .activate_provider("extra")
        .unwrap_or_else(|error| panic!("provider must hot-activate: {error}"));
    assert_eq!(runtime.fixture_redactor().registered_secret_count(), 1);
    let catalog = rw_core::ModelCatalogSource::discover(&runtime)
        .await
        .unwrap_or_else(|error| panic!("refreshed catalog must discover: {error}"));
    assert!(
        catalog
            .models
            .iter()
            .any(|model| { model.id == "extra/new-model" && model.available })
    );
    runtime
        .prepare_concrete_model("extra/new-model")
        .await
        .unwrap_or_else(|error| panic!("activated concrete model must bind: {error}"));
    let events = ModelDriver::stream(
        &runtime,
        "extra/new-model",
        request("ignored"),
        invocation(),
    )
    .unwrap_or_else(|error| panic!("activated stream must start: {error}"))
    .collect::<Vec<_>>()
    .await;
    assert!(events.iter().any(|event| {
        matches!(event, Ok(ProviderEvent::TextDelta { text }) if text == "activated-ok")
    }));
    let requests = server
        .task
        .join()
        .unwrap_or_else(|_| panic!("activation server must join"));
    assert!(requests[0].starts_with("GET /v1/models "));
    assert!(requests[1].starts_with("GET /v1/models "));
    assert!(requests[2].starts_with("POST /v1/chat/completions "));
}

#[tokio::test]
async fn missing_first_provider_credential_preserves_healthy_fallback_route() {
    let mut config = extension_config("missing/model-a");
    config.models.aliases.insert(
        "fast".to_owned(),
        vec!["missing/model-a".to_owned(), "healthy/model-b".to_owned()],
    );
    config.providers.insert(
        "missing".to_owned(),
        ProviderConfig {
            kind: "openai_compatible".to_owned(),
            base_url: Some("https://example.invalid/v1/chat/completions".to_owned()),
            api_key_credential: Some("missing-provider-key".to_owned()),
            ..ProviderConfig::default()
        },
    );
    let runtime = extension_factory()
        .with_extension_providers([("healthy/", extension_provider("healthy-private", None))])
        .build(&config)
        .unwrap_or_else(|error| panic!("healthy fallback must keep runtime available: {error}"));

    assert!(runtime.provider("missing/model-a").is_none());
    let events = runtime
        .stream_alias("fast", request("ignored"), invocation())
        .unwrap_or_else(|error| panic!("healthy fallback must stream: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|event| {
        matches!(event, Ok(ProviderEvent::MessageStart { model }) if model == "healthy/model-b")
    }));
    assert!(events.iter().any(|event| {
        matches!(event, Ok(ProviderEvent::TextDelta { text }) if text == "extension:model-b")
    }));
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
    let credential_store = TestCredentialStore::default();
    credential_store.insert("fixture-api-key", "credential_store-must-lose");
    let environment = TestEnvironment(BTreeMap::from([(
        "FIXTURE_API_KEY".to_owned(),
        API_CANARY.to_owned(),
    )]));
    let runtime = ProviderFactory::with_backends(
        manager(environment, credential_store),
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
    assert!(!captured[0].contains("credential_store-must-lose"));
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
async fn azure_gateway_config_maps_model_path_query_and_primary_header() {
    let server = spawn_server("/unused", vec![sse_response("azure-ok")]);
    let credential_store = TestCredentialStore::default();
    credential_store.insert("providers.azure.api_key", "azure-key-canary");
    let mut config = config(&server.endpoint, &["fixture/canonical-model"]);
    let provider = config
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"));
    provider.path_template = Some("/openai/deployments/{model}/chat/completions".to_owned());
    provider.extra_query =
        BTreeMap::from([("api-version".to_owned(), "2026-01-01-preview".to_owned())]);
    provider.api_key_credential = Some("providers.azure.api_key".to_owned());
    provider.auth_scheme = Some(ProviderAuthScheme::Header {
        name: "api-key".to_owned(),
        value_prefix: String::new(),
    });
    provider.model_ids =
        BTreeMap::from([("canonical-model".to_owned(), "deployment-west".to_owned())]);

    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), credential_store),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("fixture/canonical-model", false)]),
    )
    .build(&config)
    .unwrap_or_else(|error| panic!("Azure-shaped config must compose: {error}"));
    let events = runtime
        .provider("fixture/canonical-model")
        .unwrap_or_else(|| panic!("model-bound provider must exist"))
        .stream(request("canonical-model"))
        .await
        .unwrap_or_else(|error| panic!("Azure-shaped request must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let captured = server
        .task
        .join()
        .unwrap_or_else(|_| panic!("fixture server must join"));
    let request = &captured[0];
    assert!(request.starts_with(
        "POST /openai/deployments/deployment-west/chat/completions?api-version=2026-01-01-preview HTTP/1.1"
    ));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("\r\napi-key: azure-key-canary\r\n")
    );
    let body = request
        .split_once("\r\n\r\n")
        .map_or_else(|| panic!("request must contain a body"), |(_, body)| body);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body)
            .unwrap_or_else(|error| panic!("request body must be JSON: {error}"))["model"],
        json!("deployment-west")
    );
}

#[tokio::test]
async fn openrouter_gateway_config_applies_static_headers_and_extra_body() {
    let server = spawn_server("/v1/chat/completions", vec![sse_response("router-ok")]);
    let mut config = config(&server.endpoint, &["fixture/openai/gpt-route"]);
    let provider = config
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"));
    provider.api_key_env = Some("OPENROUTER_API_KEY".to_owned());
    provider.headers = BTreeMap::from([
        ("HTTP-Referer".to_owned(), "https://app.example".to_owned()),
        ("X-Title".to_owned(), "Rottweiler".to_owned()),
    ]);
    provider.extra_body = BTreeMap::from([(
        "provider".to_owned(),
        json!({"order": ["azure", "openai"], "allow_fallbacks": false}),
    )]);
    let environment = TestEnvironment(BTreeMap::from([(
        "OPENROUTER_API_KEY".to_owned(),
        "router-key-canary".to_owned(),
    )]));
    let runtime = ProviderFactory::with_backends(
        manager(environment, TestCredentialStore::default()),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        PricingTable::default(),
    )
    .build(&config)
    .unwrap_or_else(|error| panic!("OpenRouter-shaped config must compose: {error}"));
    let events = runtime
        .provider("fixture/openai/gpt-route")
        .unwrap_or_else(|| panic!("model-bound provider must exist"))
        .stream(request("openai/gpt-route"))
        .await
        .unwrap_or_else(|error| panic!("OpenRouter-shaped request must start: {error}"))
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let captured = server
        .task
        .join()
        .unwrap_or_else(|_| panic!("fixture server must join"));
    let request = &captured[0];
    let lower = request.to_ascii_lowercase();
    assert!(lower.contains("\r\nauthorization: bearer router-key-canary\r\n"));
    assert!(lower.contains("\r\nhttp-referer: https://app.example\r\n"));
    assert!(lower.contains("\r\nx-title: rottweiler\r\n"));
    let body = request
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("request must contain a body"))
        .1;
    let body: serde_json::Value = serde_json::from_str(body)
        .unwrap_or_else(|error| panic!("request body must be JSON: {error}"));
    assert_eq!(body["model"], json!("openai/gpt-route"));
    assert_eq!(body["provider"]["order"], json!(["azure", "openai"]));
    assert_eq!(body["provider"]["allow_fallbacks"], json!(false));
}

#[tokio::test]
async fn credential_header_is_registered_for_recording_redaction() {
    let server = spawn_server("/v1/chat/completions", vec![sse_response(HEADER_CANARY)]);
    let mut config = config(&server.endpoint, &["fixture/model-a"]);
    let provider = config
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"));
    provider.auth_scheme = Some(ProviderAuthScheme::None);
    provider.header_credentials = BTreeMap::from([(
        "X-Gateway-Key".to_owned(),
        "providers.fixture.gateway_key".to_owned(),
    )]);
    let credential_store = TestCredentialStore::default();
    credential_store.insert("providers.fixture.gateway_key", HEADER_CANARY);
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), credential_store),
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        pricing([("fixture/model-a", false)]),
    )
    .build(&config)
    .unwrap_or_else(|error| panic!("header credential must compose: {error}"));
    assert_eq!(runtime.fixture_redactor().registered_secret_count(), 1);
    let directory = tempdir()
        .unwrap_or_else(|error| panic!("temporary fixture directory must create: {error}"));
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
        .unwrap_or_else(|error| panic!("recorded stream must start: {error}"))
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
    assert!(captured[0].to_ascii_lowercase().contains(&format!(
        "\r\nx-gateway-key: {}\r\n",
        HEADER_CANARY.to_ascii_lowercase()
    )));
    let fixture_text = fs::read_dir(directory.path())
        .unwrap_or_else(|error| panic!("fixture directory must read: {error}"))
        .filter_map(Result::ok)
        .map(|entry| fs::read_to_string(entry.path()).unwrap_or_default())
        .collect::<String>();
    assert!(fixture_text.contains("[REDACTED]"));
    assert!(!fixture_text.contains(HEADER_CANARY));
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
            TestCredentialStore::default(),
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
    let credential_store = TestCredentialStore::default();
    credential_store.insert(
        CREDENTIAL_VAULT_ID,
        &format!("version = 1\n[credentials]\nfixture-refresh = {REFRESH_CANARY:?}\n"),
    );
    let credential_directory =
        tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let credentials_path = credential_directory.path().join("credentials.toml");
    let runtime = ProviderFactory::with_backends(
        Arc::new(CredentialManager::with_backends(
            TestEnvironment::default(),
            UnavailableOnSetCredentialStore(credential_store),
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
    let credential_file_text = fs::read_to_string(&credentials_path)
        .unwrap_or_else(|error| panic!("rotated credential file must read: {error}"));
    assert!(credential_file_text.contains(ROTATED_CANARY));
    assert!(
        runtime
            .warnings()
            .iter()
            .any(|warning| warning.contains("owner-private file"))
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
async fn credential_store_api_key_and_stored_oauth_access_are_real_request_paths() {
    let api_server = spawn_server("/v1/chat/completions", vec![sse_response("api-key")]);
    let mut api_config = config(&api_server.endpoint, &["fixture/model-a"]);
    api_config
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("fixture provider must exist"))
        .api_key_credential = Some("stored-api-key".to_owned());
    let credential_store = TestCredentialStore::default();
    credential_store.insert("stored-api-key", API_CANARY);
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), credential_store),
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
    let credential_store = TestCredentialStore::default();
    credential_store.insert("stored-oauth-access", OAUTH_CANARY);
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), credential_store),
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
    let credential_store = TestCredentialStore::default();
    credential_store.insert("proxy-password", "proxy-secret");
    let runtime = ProviderFactory::with_backends(
        manager(
            TestEnvironment(BTreeMap::from([(
                "FIXTURE_API_KEY".to_owned(),
                API_CANARY.to_owned(),
            )])),
            credential_store,
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
fn subscription_kind_has_independent_capabilities_and_no_dollar_pricing() {
    let config = subscription_config("gpt-5.4-mini");
    let runtime = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), subscription_credential_store()),
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
    assert_eq!(
        runtime.accounting_for_alias(
            "fast",
            rw_providers::TokenUsage {
                input_tokens: 40,
                output_tokens: 2,
                ..rw_providers::TokenUsage::default()
            },
        ),
        Cost::SubscriptionQuota {
            used: Some("42".to_owned()),
            unit: Some("tokens".to_owned()),
        }
    );
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

    let mut overridden = subscription_config("gpt-5.4-mini");
    overridden
        .providers
        .get_mut("fixture")
        .unwrap_or_else(|| panic!("subscription provider must exist"))
        .pricing
        .insert(
            "gpt-5.4-mini".to_owned(),
            declared_pricing(1.0, 1.0, None, None),
        );
    let error = ProviderFactory::with_backends(
        manager(TestEnvironment::default(), subscription_credential_store()),
        ProxyEnvironment::default(),
        NetworkPolicy::Deny,
        pricing([("openai/gpt-5.4-mini", false)]),
    )
    .build(&overridden)
    .err()
    .unwrap_or_else(|| panic!("subscription pricing override must fail"));
    assert!(
        error
            .to_string()
            .contains("subscription or credit accounting")
    );
}

#[test]
fn subscription_kind_rejects_auth_endpoint_conflicts_without_static_model_allowlist() {
    let build = |config: &rw_types::config::Config| {
        ProviderFactory::with_backends(
            manager(TestEnvironment::default(), subscription_credential_store()),
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
    assert!(build(&subscription_config("catalog-discovery")).is_ok());
    assert!(build(&subscription_config("future-live-model")).is_ok());
}
