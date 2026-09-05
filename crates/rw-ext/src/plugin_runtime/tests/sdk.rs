use super::*;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn typescript_tool_hook_event_push_and_provider_cross_rust_host() {
    let (sdk, tool_config) = sdk_fixture_config("pre-tool-deny-custom-tool.ts");
    let tool_host = approved_fixture_host(&tool_config, &sdk, Arc::new(DenyPushHandler)).await;
    let declaration = tool_host.manifest().capabilities.tools[0].clone();
    let adapter = RpcToolAdapter::new(declaration, tool_host.client(), tool_host.enforcer())
        .expect("approved tool adapter");
    let context = ToolContext::new(&sdk).expect("tool context");
    let result = adapter
        .execute(&context, json!({"text":"hello"}))
        .await
        .expect("TypeScript tool result");
    assert_eq!(result.content, "hello");
    let hook = crate::RpcHookHandler::new(tool_host.client(), tool_host.enforcer());
    let mut dispatcher = crate::HookDispatcher::new();
    dispatcher
        .register(
            crate::plugin_hook_registration(
                tool_host.manifest().capabilities.hooks[0],
                "typescript:pre-tool",
            ),
            hook,
        )
        .expect("register RPC hook");
    assert!(matches!(
        dispatcher
            .dispatch(crate::HookEvent::PreTool, json!({"name":"bash"}))
            .await
            .status(),
        crate::HookDispatchStatus::Blocked { .. }
    ));
    tool_host.shutdown().await.expect("tool host shutdown");

    let (sdk, event_config) = sdk_fixture_config("event-subscriber.ts");
    let pushes = Arc::new(RecordingPush::default());
    let event_host = approved_fixture_host(&event_config, &sdk, pushes.clone()).await;
    PluginEventRouter::new(event_host.client(), event_host.enforcer())
        .publish("TurnFinished", json!({"session_id":"s"}))
        .await
        .expect("publish event");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !pushes.0.lock().expect("push lock").is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("event push deadline");
    assert_eq!(
        pushes.0.lock().expect("push lock")[0].0,
        METHOD_SESSION_SET_STATUS
    );
    event_host.shutdown().await.expect("event host shutdown");

    let (sdk, provider_config) = sdk_fixture_config("provider.ts");
    let provider_config = provider_config
        .with_allowed_domains(["example.com"])
        .expect("provider domains");
    let provider_host =
        approved_fixture_host(&provider_config, &sdk, Arc::new(DenyPushHandler)).await;
    let provider = RpcProviderAdapter::new(
        "typescript-fixture",
        "fixture/",
        Capabilities {
            tool_calling: true,
            vision: false,
            thinking: false,
            cache_breakpoints: CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: WireMode::NormalizedReplay,
        },
        provider_host.client(),
        provider_host.enforcer(),
    );
    let mut events = provider
        .stream(ProviderRequest {
            model: "model".to_owned(),
            turns: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 64,
            temperature: None,
            thinking: ThinkingLevel::Off,
            cache_hint: None,
        })
        .await
        .expect("provider response");
    let stream_started = std::time::Instant::now();
    assert!(matches!(
        events.next().await,
        Some(Ok(ProviderEvent::MessageStart { .. }))
    ));
    assert!(matches!(
        events.next().await,
        Some(Ok(ProviderEvent::TextDelta { text })) if text.contains("fixture/model")
    ));
    let first_delta = stream_started.elapsed();
    while events.next().await.is_some() {}
    let completed = stream_started.elapsed();
    assert!(
        completed.saturating_sub(first_delta) >= Duration::from_millis(50),
        "provider completion was not observably delayed after its first delta: delta={first_delta:?} complete={completed:?}"
    );

    let cancelled = provider
        .stream(ProviderRequest {
            model: "cancelled".to_owned(),
            turns: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 64,
            temperature: None,
            thinking: ThinkingLevel::Off,
            cache_hint: None,
        })
        .await
        .expect("cancelled provider stream admission");
    drop(cancelled);
    tokio::time::timeout(Duration::from_secs(4), provider.settle_effects())
        .await
        .expect("cancelled provider effect settlement");
    assert!(provider_host.client.closed.load(Ordering::Acquire));
    assert!(
        provider_host
            .client
            .provider_streams
            .lock()
            .expect("streams")
            .is_empty()
    );
    provider_host
        .shutdown()
        .await
        .expect("provider host shutdown");
}

#[tokio::test]
async fn typescript_numeric_and_escaped_events_replenish_exact_wire_credit() {
    let (sdk, config) = sdk_fixture_config("provider-v3.ts");
    let config = config
        .with_allowed_domains(["example.com"])
        .expect("fixture domains");
    let host = approved_fixture_host(&config, &sdk, Arc::new(DenyPushHandler)).await;
    let mut events = host
        .client()
        .provider_stream(json!({
            "alias": "fixture-v3/numeric-credit", "request": {
                "model": "numeric-credit", "turns": [], "tools": [],
                "tool_choice": {"mode":"auto"}, "max_output_tokens":64,
                "temperature":null, "thinking":"off"
            }
        }))
        .await
        .expect("stream admission");
    let mut count = 0;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("credit progress")
    {
        let event = event.expect("valid numeric event");
        if event["type"] == "tool_call_end" {
            count += 1;
        }
    }
    assert_eq!(count, 256);
    assert!(!host.client.closed.load(Ordering::Acquire));
    host.shutdown().await.expect("settled shutdown");
}

#[tokio::test]
async fn typescript_protocol_three_catalog_crosses_rust_host() {
    let (sdk, provider_config) = sdk_fixture_config("provider-v3.ts");
    let provider_config = provider_config
        .with_allowed_domains(["example.com"])
        .expect("provider domains");
    let host = approved_fixture_host(&provider_config, &sdk, Arc::new(DenyPushHandler)).await;
    assert_eq!(
        host.manifest().protocol,
        rw_plugin_protocol::PROTOCOL_VERSION
    );
    let provider = RpcProviderAdapter::new(
        "typescript-fixture-v3",
        "fixture-v3/",
        Capabilities {
            tool_calling: true,
            vision: false,
            thinking: false,
            cache_breakpoints: CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: WireMode::NormalizedReplay,
        },
        host.client(),
        host.enforcer(),
    )
    .with_model_catalog();
    let catalog = provider
        .discover_models()
        .await
        .expect("catalog request")
        .expect("protocol 3 catalog");
    assert_eq!(catalog.provider, "fixture-v3");
    assert_eq!(catalog.models[0].id, "vision-thinking");
    let metadata = provider
        .cached_model_metadata()
        .expect("single model metadata");
    assert!(metadata.capabilities.vision);
    assert!(metadata.capabilities.thinking);
    assert_eq!(metadata.accounting, UsageAccounting::ApiDollars);
    assert_eq!(
        metadata
            .pricing
            .expect("catalog pricing")
            .input_per_million_micros_usd,
        3_000_000
    );
    host.shutdown().await.expect("provider host shutdown");
}

#[tokio::test]
async fn protocol_three_provider_auth_streams_through_host_without_secret_delivery() {
    let (sdk, config) = sdk_fixture_config("provider-auth-v3.ts");
    let config = config
        .with_allowed_domains(["api.example.test"])
        .expect("provider domains");
    let http = Arc::new(FixtureProviderHttp::default());
    let host = approved_fixture_host_with_http(
        &config,
        &sdk,
        Arc::new(DenyPushHandler),
        http.clone(),
        Arc::new(HttpSecretRedactor),
    )
    .await;
    assert_eq!(
        host.manifest().capabilities.providers[0].credential_references,
        ["fixture-token"]
    );
    let provider = RpcProviderAdapter::new(
        "typescript-auth-v3",
        "auth-v3/",
        Capabilities {
            tool_calling: true,
            vision: false,
            thinking: false,
            cache_breakpoints: CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: WireMode::NormalizedReplay,
        },
        host.client(),
        host.enforcer(),
    );
    let events = provider
        .stream(ProviderRequest {
            model: "tool-model".to_owned(),
            turns: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 64,
            temperature: None,
            thinking: ThinkingLevel::Off,
            cache_hint: None,
        })
        .await
        .expect("host-mediated provider stream")
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|event| matches!(
        event,
        Ok(ProviderEvent::ToolCallEnd { id, arguments })
            if id == "call-1" && arguments["city"] == "Chicago"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Ok(ProviderEvent::TextDelta { text }) if text == "[REDACTED]"
    )));
    let serialized_requests = serde_json::to_string(&*http.requests.lock().expect("request lock"))
        .expect("serialized captured requests");
    assert!(!serialized_requests.contains(HTTP_SECRET));
    assert!(serialized_requests.contains("fixture-token"));
    let cancelled = provider
        .stream(ProviderRequest {
            model: "cancelled".to_owned(),
            turns: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 64,
            temperature: None,
            thinking: ThinkingLevel::Off,
            cache_hint: None,
        })
        .await
        .expect("cancelled HTTP provider admission");
    tokio::time::sleep(Duration::from_millis(25)).await;
    drop(cancelled);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !http.cancelled.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("host-mediated HTTP cancellation deadline");
    host.shutdown().await.expect("auth provider host shutdown");
}

#[tokio::test]
async fn protocol_three_provider_refuses_undeclared_credential_reference_at_call_time() {
    let (sdk, config) = sdk_fixture_config("provider-auth-v3.ts");
    let config = config
        .with_allowed_domains(["api.example.test"])
        .expect("provider domains");
    let http = Arc::new(FixtureProviderHttp::default());
    let host = approved_fixture_host_with_http(
        &config,
        &sdk,
        Arc::new(DenyPushHandler),
        http.clone(),
        Arc::new(HttpSecretRedactor),
    )
    .await;
    let provider = RpcProviderAdapter::new(
        "typescript-auth-v3",
        "auth-v3/",
        Capabilities {
            tool_calling: true,
            vision: false,
            thinking: false,
            cache_breakpoints: CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: WireMode::NormalizedReplay,
        },
        host.client(),
        host.enforcer(),
    );
    let result = provider
        .stream(ProviderRequest {
            model: "undeclared".to_owned(),
            turns: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 64,
            temperature: None,
            thinking: ThinkingLevel::Off,
            cache_hint: None,
        })
        .await;
    if let Ok(mut stream) = result {
        assert!(stream.next().await.is_some_and(|item| item.is_err()));
    }
    assert!(host.enforcer().violated());
    assert!(http.requests.lock().expect("request lock").is_empty());
}
