use super::*;

#[tokio::test]
async fn protocol_three_provider_catalog_is_bounded_and_cached() {
    let provider = catalog_adapter(json!({"models":[{
        "id":"capable",
        "display_name":"Capable",
        "capabilities":{
            "tool_calling":true,
            "vision":true,
            "thinking":true,
            "cache_breakpoints":"explicit"
        },
        "max_context_tokens":u64::MAX,
        "max_output_tokens":0,
        "pricing":{
            "input_per_million_micros_usd":u64::MAX,
            "output_per_million_micros_usd":15_000_000
        }
    }]}));
    let catalog = provider
        .discover_models()
        .await
        .expect("valid bounded catalog")
        .expect("protocol 3 catalog");
    assert_eq!(catalog.provider, "fixture");
    assert_eq!(catalog.models[0].id, "capable");
    let capabilities = provider.capabilities();
    assert!(capabilities.vision);
    assert!(capabilities.thinking);
    assert_eq!(
        capabilities.cache_breakpoints,
        CacheBreakpointSupport::Explicit
    );
    assert_eq!(
        capabilities.max_context_tokens,
        Some(MAX_PLUGIN_MODEL_TOKENS)
    );
    assert_eq!(capabilities.max_output_tokens, Some(1));
    let metadata = provider
        .cached_model_metadata()
        .expect("single-model metadata cache");
    assert_eq!(metadata.accounting, UsageAccounting::ApiDollars);
    assert_eq!(
        metadata
            .pricing
            .expect("catalog pricing")
            .input_per_million_micros_usd,
        MAX_PLUGIN_PRICE_MICROS_USD
    );
}

#[tokio::test]
async fn protocol_three_catalog_caches_metadata_per_model() {
    let provider = catalog_adapter(json!({"models":[{
        "id":"text-only",
        "capabilities":{
            "tool_calling":false,"vision":false,"thinking":false,"cache_breakpoints":"none"
        },
        "pricing":{
            "input_per_million_micros_usd":1_000_000,
            "output_per_million_micros_usd":2_000_000
        }
    },{
        "id":"vision-thinking",
        "capabilities":{
            "tool_calling":true,"vision":true,"thinking":true,"cache_breakpoints":"explicit"
        },
        "pricing":{
            "input_per_million_micros_usd":3_000_000,
            "output_per_million_micros_usd":4_000_000
        }
    }]}));
    provider
        .discover_models()
        .await
        .expect("valid multi-model catalog");

    assert!(provider.cached_model_metadata().is_none());
    let text = provider
        .cached_model_metadata_for("text-only")
        .expect("text model metadata");
    assert!(!text.capabilities.tool_calling);
    assert!(!text.capabilities.vision);
    assert_eq!(
        text.pricing
            .expect("text pricing")
            .input_per_million_micros_usd,
        1_000_000
    );
    let vision = provider
        .cached_model_metadata_for("vision-thinking")
        .expect("vision model metadata");
    assert!(vision.capabilities.tool_calling);
    assert!(vision.capabilities.vision);
    assert!(vision.capabilities.thinking);
    assert_eq!(
        vision
            .pricing
            .expect("vision pricing")
            .input_per_million_micros_usd,
        3_000_000
    );
    assert!(provider.cached_model_metadata_for("missing").is_none());
}

#[tokio::test]
async fn malformed_provider_catalog_degrades_only_that_adapter() {
    let provider = catalog_adapter(json!({"models":[{
        "id":"duplicate",
        "capabilities":{
            "tool_calling":true,"vision":false,"thinking":false,"cache_breakpoints":"none"
        }
    },{
        "id":"duplicate",
        "capabilities":{
            "tool_calling":true,"vision":false,"thinking":false,"cache_breakpoints":"none"
        }
    }]}));
    let error = provider
        .discover_models()
        .await
        .expect_err("duplicate model ids must fail discovery");
    assert_eq!(error.kind, ProviderErrorKind::Protocol);
    assert!(provider.cached_model_metadata().is_none());
    assert_eq!(
        provider.capabilities(),
        Capabilities {
            tool_calling: true,
            vision: false,
            thinking: false,
            cache_breakpoints: CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: WireMode::NormalizedReplay,
        }
    );
}
