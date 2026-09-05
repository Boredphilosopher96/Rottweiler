#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};

use super::*;
use rw_tools::{CancellationToken, WebSearchRequest};

#[derive(Clone, Default)]
struct EmptyEnvironment;

impl CredentialEnvironment for EmptyEnvironment {
    fn get(&self, _name: &str) -> Result<Option<String>, CredentialError> {
        Ok(None)
    }
}

#[derive(Clone, Default)]
struct CountingCredentialStore(Arc<Mutex<(usize, usize)>>);

impl CredentialStore for CountingCredentialStore {
    fn get(
        &self,
        _identifier: &str,
    ) -> Result<Option<StoredSecret<String>>, rw_store::credentials::CredentialStoreUnavailable>
    {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0 += 1;
        Ok(Some(StoredSecret::new(
            "version = 1\n[credentials]\n'providers.work.api_key' = 'fixture-key'\n".to_owned(),
        )))
    }

    fn get_authorized(
        &self,
        _identifier: &str,
    ) -> Result<Option<StoredSecret<String>>, rw_store::credentials::CredentialStoreUnavailable>
    {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1 += 1;
        Ok(Some(StoredSecret::new(
            "version = 1\n[credentials]\n'providers.work.api_key' = 'fixture-key'\n".to_owned(),
        )))
    }

    fn set(
        &self,
        _identifier: &str,
        _secret: &StoredSecret<String>,
    ) -> Result<(), rw_store::credentials::CredentialStoreUnavailable> {
        Ok(())
    }
}

fn configured_work_provider() -> Config {
    let mut config = Config::default();
    config.providers.insert(
        "work".to_owned(),
        ProviderConfig {
            kind: "openai".to_owned(),
            base_url: Some("http://127.0.0.1:9/v1/responses".to_owned()),
            api_key_credential: Some("providers.work.api_key".to_owned()),
            ..ProviderConfig::default()
        },
    );
    config.models.default = "fast".to_owned();
    config
        .models
        .aliases
        .insert("fast".to_owned(), vec!["work/fixture".to_owned()]);
    config
}

fn capability_pricing() -> ModelPricing {
    ModelPricing {
        display_name: "Fixture".to_owned(),
        max_context_tokens: Some(400_000),
        max_output_tokens: Some(128_000),
        supports_tools: true,
        supports_thinking: true,
        supports_vision: true,
        reasoning_efforts: vec![ThinkingLevel::Low, ThinkingLevel::High],
        input_per_million_micros_usd: 1,
        output_per_million_micros_usd: 1,
        cache_read_per_million_micros_usd: None,
        cache_write_per_million_micros_usd: None,
        reasoning_per_million_micros_usd: None,
    }
}

#[test]
fn subscription_and_copilot_pre_discovery_caps_use_catalog_enrichment() {
    let pricing = capability_pricing();
    let subscription = subscription_model_capabilities(Some(&pricing));
    assert_eq!(subscription.max_context_tokens, Some(400_000));
    assert_eq!(subscription.max_output_tokens, Some(128_000));
    assert!(subscription.tool_calling);
    assert!(subscription.thinking);
    assert!(subscription.vision);

    let copilot = github_copilot_capabilities(Some(&pricing));
    assert_eq!(copilot.max_context_tokens, Some(400_000));
    assert_eq!(copilot.max_output_tokens, Some(128_000));
    assert!(copilot.tool_calling);
    assert!(copilot.thinking);
}

#[test]
fn unknown_subscription_caps_remain_explicitly_unbounded() {
    let capabilities = subscription_model_capabilities(None);
    assert_eq!(capabilities.max_context_tokens, None);
    assert_eq!(capabilities.max_output_tokens, None);
    assert!(capabilities.tool_calling);
}

#[test]
fn subscription_tools_do_not_depend_on_pricing_metadata() {
    let mut pricing = capability_pricing();
    pricing.supports_tools = false;
    assert!(subscription_model_capabilities(Some(&pricing)).tool_calling);
}

#[test]
fn live_ids_are_availability_truth_and_pricing_only_enriches() {
    let mut config = Config::default();
    config.providers.insert(
        "work".to_owned(),
        ProviderConfig {
            kind: "openai".to_owned(),
            ..ProviderConfig::default()
        },
    );
    config.models.default = "fast".to_owned();
    config
        .models
        .aliases
        .insert("fast".to_owned(), vec!["work/live-model".to_owned()]);
    let mut pricing = PricingTable::default();
    pricing
        .models
        .insert("openai/live-model".to_owned(), capability_pricing());
    pricing
        .models
        .insert("openai/stale-model".to_owned(), capability_pricing());
    let catalog = project_model_catalog(
        &config,
        &pricing,
        vec![(
            "work".to_owned(),
            "work/live-model".to_owned(),
            true,
            Ok(rw_providers::DiscoveredProviderCatalog {
                provider: "work/live-model".to_owned(),
                models: vec![rw_providers::DiscoveredModel {
                    id: "live-model".to_owned(),
                    display_name: Some("Live".to_owned()),
                    description: None,
                    capabilities: None,
                    pricing: None,
                }],
            }),
        )],
    );
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(catalog.models[0].id, "work/live-model");
    assert_eq!(
        catalog.models[0].capabilities.max_context_tokens,
        Some(400_000)
    );
    assert!(catalog.models[0].current);
    assert!(catalog.models[0].available);
    assert!(
        catalog
            .models
            .iter()
            .all(|model| !model.id.contains("stale"))
    );
    let provider = catalog
        .providers
        .iter()
        .find(|provider| provider.name == "work")
        .expect("provider");
    assert_eq!(provider.auth_kind, ProviderAuthKind::ApiKey);
    assert_eq!(provider.next_action, ProviderNextAction::SelectModels);
}

#[test]
fn one_provider_failure_remains_visible_without_fabricating_models() {
    let mut config = Config::default();
    config.providers.insert(
        "broken".to_owned(),
        ProviderConfig {
            kind: "anthropic".to_owned(),
            ..ProviderConfig::default()
        },
    );
    config.models.default = "fast".to_owned();
    config
        .models
        .aliases
        .insert("fast".to_owned(), vec!["broken/model".to_owned()]);
    let catalog = project_model_catalog(
        &config,
        &PricingTable::default(),
        vec![(
            "broken".to_owned(),
            "broken/model".to_owned(),
            true,
            Err("network unavailable".to_owned()),
        )],
    );
    assert!(
        catalog.models.is_empty(),
        "configured aliases must never masquerade as a live model catalog"
    );
    assert!(catalog.providers.iter().any(|provider| {
        provider.name == "broken" && !provider.reachable && provider.status.is_some()
    }));
    assert!(catalog.providers.iter().any(|provider| {
        provider.name == "github_copilot"
            && !provider.configured
            && provider.auth_kind == ProviderAuthKind::DeviceFlow
            && provider.next_action == ProviderNextAction::Configure
    }));
}

#[test]
fn file_configured_extension_alias_never_seeds_the_live_catalog() {
    let mut config = Config::default();
    config.providers.clear();
    config.models.default = "fast".to_owned();
    config.models.aliases =
        BTreeMap::from([("fast".to_owned(), vec!["my_plugin/file_model".to_owned()])]);

    let catalog = project_model_catalog(&config, &PricingTable::default(), Vec::new());

    assert!(catalog.models.is_empty());
    assert!(
        catalog
            .providers
            .iter()
            .all(|provider| provider.name != "my_plugin"),
        "an alias file cannot invent a provider or concrete model row"
    );
}

#[test]
fn first_run_placeholder_has_provider_inventory_but_no_model_rows() {
    let mut config = Config::default();
    config.providers.insert(
        "github_copilot".to_owned(),
        ProviderConfig {
            kind: "github_copilot".to_owned(),
            ..ProviderConfig::default()
        },
    );
    config.models.default = "fast".to_owned();
    config.models.aliases = BTreeMap::from([(
        "fast".to_owned(),
        vec!["github_copilot/a-configured-route-is-not-a-catalog".to_owned()],
    )]);

    let catalog = ProviderModelCatalogSource::placeholder(&config);

    assert!(catalog.models.is_empty());
    let provider = catalog
        .providers
        .iter()
        .find(|provider| provider.name == "github_copilot")
        .expect("configured provider row");
    assert!(provider.configured);
    assert!(!provider.authenticated);
    assert!(!provider.reachable);
    assert_eq!(provider.model_count, 0);
}

#[test]
fn rejected_chatgpt_catalog_does_not_claim_reachability() {
    let mut config = Config::default();
    config.providers.clear();
    config.providers.insert(
        "openai_codex".to_owned(),
        ProviderConfig {
            kind: "openai_codex".to_owned(),
            ..ProviderConfig::default()
        },
    );
    config.models.default = "fast".to_owned();
    config.models.aliases = BTreeMap::from([(
        "fast".to_owned(),
        vec!["openai_codex/gpt-5.4-mini".to_owned()],
    )]);
    let catalog = project_model_catalog(
        &config,
        &PricingTable::default(),
        vec![(
            "openai_codex".to_owned(),
            "openai_codex/gpt-5.4-mini".to_owned(),
            true,
            Err("provider model discovery request was rejected".to_owned()),
        )],
    );

    let provider = catalog
        .providers
        .iter()
        .find(|provider| provider.name == "openai_codex")
        .expect("ChatGPT provider");
    assert_eq!(provider.auth_kind, ProviderAuthKind::Oauth);
    assert!(provider.authenticated);
    assert!(!provider.reachable);
    assert_eq!(provider.model_count, 0);
    assert!(
        catalog.models.is_empty(),
        "a rejected live catalog must not expose configured fallback model rows"
    );
    assert!(
        catalog
            .providers
            .iter()
            .any(|provider| { provider.name == "openai" && !provider.configured })
    );
}

#[tokio::test]
async fn production_catalog_uses_one_authorized_vault_read() {
    let credential_store = CountingCredentialStore::default();
    let manager = Arc::new(CredentialManager::with_backends(
        EmptyEnvironment,
        credential_store.clone(),
        std::path::PathBuf::from("/nonexistent/rottweiler-test-credentials.toml"),
    ));
    let factory = ProviderFactory::with_backends(
        manager,
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        PricingTable::default(),
    )
    .with_model_discovery_timeout(std::time::Duration::from_millis(5));

    let catalog = factory
        .discover_model_catalog(&configured_work_provider())
        .await
        .expect("catalog failures remain visible rows");
    let calls = *credential_store
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(calls, (0, 1));
    assert!(catalog.providers.iter().any(|provider| {
        provider.name == "work" && provider.authenticated && !provider.reachable
    }));
}

#[test]
fn active_provider_build_uses_authorized_credentials() {
    let credential_store = CountingCredentialStore::default();
    let manager = Arc::new(CredentialManager::with_backends(
        EmptyEnvironment,
        credential_store.clone(),
        std::path::PathBuf::from("/nonexistent/rottweiler-test-credentials.toml"),
    ));
    let factory = ProviderFactory::with_backends(
        manager,
        ProxyEnvironment::default(),
        NetworkPolicy::Allow,
        PricingTable::default(),
    );

    factory
        .build(&configured_work_provider())
        .expect("active provider composition");
    let calls = *credential_store
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(calls, (0, 1));
}

#[test]
fn provider_discovery_status_never_exposes_private_endpoint_text() {
    let error = ProviderError::new(
        ProviderErrorKind::Network,
        "request to https://private.example.invalid/secret failed",
    );
    let status = provider_discovery_status(&error);
    assert_eq!(status, "provider model discovery network request failed");
    assert!(!status.contains("private.example"));
}

#[test]
fn catalog_projection_is_globally_bounded_and_marks_truncation() {
    let mut config = Config::default();
    config.providers.insert(
        "work".to_owned(),
        ProviderConfig {
            kind: "openai".to_owned(),
            ..ProviderConfig::default()
        },
    );
    let models = (0..(MAX_CATALOG_MODELS + 8))
        .map(|index| rw_providers::DiscoveredModel {
            id: format!("model-{index:04}"),
            display_name: Some("x".repeat(MAX_CATALOG_TEXT_BYTES + 10)),
            description: None,
            capabilities: None,
            pricing: None,
        })
        .collect();
    let catalog = project_model_catalog(
        &config,
        &PricingTable::default(),
        vec![(
            "work".to_owned(),
            "work/catalog-discovery".to_owned(),
            true,
            Ok(rw_providers::DiscoveredProviderCatalog {
                provider: "work".to_owned(),
                models,
            }),
        )],
    );
    assert!(catalog.truncated);
    assert!(catalog.models.len() <= MAX_CATALOG_MODELS);
    assert!(
        serde_json::to_vec(&catalog).is_ok_and(|encoded| encoded.len() <= MAX_CATALOG_WIRE_BYTES)
    );
    assert!(
        catalog
            .models
            .iter()
            .all(|model| model.display_name.len() <= MAX_CATALOG_TEXT_BYTES)
    );
}

struct Candidate {
    name: &'static str,
    fail: bool,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl Provider for Candidate {
    fn name(&self) -> &'static str {
        self.name
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calling: true,
            vision: false,
            thinking: false,
            cache_breakpoints: CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: WireMode::OpenAiResponses,
        }
    }
    fn native_web_search_capability(&self) -> NativeWebSearchCapability {
        NativeWebSearchCapability::Supported
    }
    async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        self.calls.lock().expect("calls").push(self.name);
        if self.fail {
            return Err(ProviderError::new(
                ProviderErrorKind::RateLimited,
                "candidate failed",
            ));
        }
        Ok(Box::pin(futures_util::stream::iter([Ok(
            rw_providers::ProviderEvent::Finished {
                reason: rw_providers::FinishReason::Stop,
            },
        )])))
    }
}

#[derive(Default)]
struct SearchAccounting(Mutex<Vec<crate::provider_admission::ProviderCallIdentity>>);
#[async_trait]
impl crate::provider_admission::ProviderAccountingSink for SearchAccounting {
    async fn append_accounted(
        &self,
        identity: crate::provider_admission::ProviderCallIdentity,
        actuals: crate::provider_admission::ProviderCallActuals,
    ) -> Result<
        crate::provider_admission::ProviderCallReceipt,
        crate::provider_admission::BudgetReservationError,
    > {
        let mut calls = self.0.lock().expect("receipts");
        calls.push(identity.clone());
        Ok(crate::provider_admission::ProviderCallReceipt {
            identity,
            actuals,
            sequence_id: rw_types::SequenceId(calls.len() as u64),
            accounted_at: rw_store::session::UtcTimestamp::from_unix_millis(0)?,
        })
    }
}

#[tokio::test]
async fn native_search_candidates_fail_over_in_alias_order_with_distinct_accounted_attempts() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let providers: Vec<Arc<dyn Provider>> = vec![
        Arc::new(Candidate {
            name: "primary",
            fail: true,
            calls: Arc::clone(&calls),
        }),
        Arc::new(Candidate {
            name: "fallback",
            fail: false,
            calls: Arc::clone(&calls),
        }),
    ];
    let router = Arc::new(
        ProviderRouter::new(
            BTreeMap::from([(
                "fast".into(),
                vec!["primary/fixture".into(), "fallback/fixture".into()],
            )]),
            providers,
            RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::default()
            },
        )
        .expect("router"),
    );
    let factory = ProviderNativeWebSearchFactory {
        candidates: router.resolve("fast").expect("routes").to_vec(),
        router,
        alias: "fast".into(),
        metadata: BTreeMap::new(),
    };
    let accounting = Arc::new(SearchAccounting::default());
    let searcher = factory.bind(crate::provider_admission::ProviderInvocation {
        session_id: rw_types::SessionId("child".into()),
        budget_session_id: rw_types::SessionId("root".into()),
        turn_id: rw_types::TurnId("1".into()),
        attribution: rw_types::AccountingAttribution::Main,
        call_id: "binding".into(),
        input: crate::provider_admission::ProviderInputBudget::Estimated(0),
        budget: BudgetConfig::default(),
        clock: Arc::new(crate::SystemEventClock),
        admission: crate::provider_admission::testing::admission(),
        accounting: accounting.clone(),
    });
    let request = || WebSearchRequest {
        model_alias: Some("fast".into()),
        query: "query".into(),
        max_results: 5,
        recency_days: None,
        allowed_domains: Vec::new(),
    };
    searcher
        .search(request(), CancellationToken::default())
        .await
        .expect("fallback search");
    assert_eq!(
        calls.lock().expect("calls").as_slice(),
        ["primary", "fallback"]
    );
    searcher
        .search(request(), CancellationToken::default())
        .await
        .expect("second search");
    let receipts = accounting.0.lock().expect("receipts");
    assert_eq!(receipts.len(), 3);
    assert_eq!(receipts[0].attempt, 0);
    assert_eq!(receipts[1].attempt, 1);
    assert_eq!(receipts[0].call_id, receipts[1].call_id);
    assert_ne!(receipts[1].call_id, receipts[2].call_id);
    assert!(
        receipts
            .iter()
            .all(|call| call.session_id.0 == "child" && call.budget_session_id.0 == "root")
    );
}
