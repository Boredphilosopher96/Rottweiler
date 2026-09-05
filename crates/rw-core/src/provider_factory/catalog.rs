use super::*;

pub(super) fn find_pricing(
    table: &PricingTable,
    provider: &str,
    model: &str,
    namespace: Option<&str>,
) -> (Option<String>, Option<ModelPricing>) {
    if let Some(namespace) = namespace {
        let canonical = format!("{namespace}/{model}");
        return table
            .models
            .get(&canonical)
            .map_or((None, None), |pricing| {
                (Some(canonical), Some(pricing.clone()))
            });
    }
    let local = format!("{provider}/{model}");
    if let Some(pricing) = table.models.get(&local) {
        return (Some(local), Some(pricing.clone()));
    }
    (None, None)
}

pub(super) fn declared_pricing(
    config: &Config,
    provider: &str,
    model: &str,
) -> Option<ModelPricing> {
    let pricing = config.providers.get(provider)?.pricing.get(model)?;
    configured_model_pricing(model, pricing)
}

pub(super) fn configured_model_pricing(
    model: &str,
    pricing: &ProviderModelPricingConfig,
) -> Option<ModelPricing> {
    let rate = |value: &serde_json::Number| {
        // Configuration validation bounds every rate before composition.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        value
            .as_f64()
            .map(|rate| (rate * 1_000_000.0).round() as u64)
    };
    Some(ModelPricing {
        display_name: model.to_owned(),
        max_context_tokens: None,
        max_output_tokens: None,
        supports_tools: false,
        supports_thinking: false,
        supports_vision: false,
        reasoning_efforts: Vec::new(),
        input_per_million_micros_usd: rate(pricing.input_per_million.as_ref()?)?,
        output_per_million_micros_usd: rate(pricing.output_per_million.as_ref()?)?,
        cache_read_per_million_micros_usd: pricing.cache_read_per_million.as_ref().and_then(rate),
        cache_write_per_million_micros_usd: pricing.cache_write_per_million.as_ref().and_then(rate),
        reasoning_per_million_micros_usd: None,
    })
}

pub(super) fn effective_pricing(
    configured: Option<ModelPricing>,
    discovered: Option<ModelPricing>,
    catalog: Option<ModelPricing>,
) -> (Option<ModelPricing>, Option<ModelPricingSource>) {
    if let Some(pricing) = configured {
        return (Some(pricing), Some(ModelPricingSource::UserConfig));
    }
    if let Some(pricing) = discovered {
        return (Some(pricing), Some(ModelPricingSource::ProviderDiscovered));
    }
    catalog.map_or((None, None), |pricing| {
        (Some(pricing), Some(ModelPricingSource::ModelsDev))
    })
}

pub(super) fn discovery_candidate(provider: &str) -> String {
    // This is an internal, never-presented binding used only to construct the
    // provider adapter before it calls the live catalog endpoint. It must not
    // seed discovery from configured aliases, a bundled list, or a model file.
    format!("{provider}/catalog-discovery")
}

#[allow(clippy::too_many_lines)]
pub(super) fn project_model_catalog(
    config: &Config,
    pricing: &PricingTable,
    mut discoveries: Vec<(
        String,
        String,
        bool,
        Result<rw_providers::DiscoveredProviderCatalog, String>,
    )>,
) -> ModelCatalogSnapshot {
    discoveries.sort_by(|left, right| left.0.cmp(&right.0));
    let reverse_aliases = config
        .models
        .aliases
        .iter()
        .flat_map(|(alias, candidates)| {
            candidates
                .iter()
                .map(move |candidate| (candidate.clone(), ModelAlias(alias.clone())))
        })
        .fold(
            BTreeMap::<String, Vec<ModelAlias>>::new(),
            |mut map, (candidate, alias)| {
                map.entry(candidate).or_default().push(alias);
                map
            },
        );
    let current_candidate = config
        .models
        .aliases
        .get(&config.models.default)
        .and_then(|candidates| candidates.first());
    let mut models = BTreeMap::new();
    let mut providers = Vec::new();
    for (provider_name, _candidate, authenticated, discovery) in discoveries {
        let provider = match discovery {
            Ok(catalog) => project_available_provider(
                config,
                pricing,
                &provider_name,
                catalog,
                &reverse_aliases,
                current_candidate,
                &mut models,
            ),
            Err(error) => {
                project_unavailable_provider(config, &provider_name, authenticated, error)
            }
        };
        providers.push(provider);
    }
    for profile in BUILTIN_PROVIDER_PROFILES
        .iter()
        .copied()
        .filter(|profile| profile.setup_exposed())
    {
        let name = profile.canonical_id();
        if !providers.iter().any(|provider| provider.name == name) {
            providers.push(ProviderDescriptor {
                name: name.to_owned(),
                auth_kind: profile.onboarding_auth_kind(),
                next_action: ProviderNextAction::Configure,
                configured: false,
                authenticated: false,
                reachable: false,
                model_count: 0,
                status: Some("provider setup required before authentication".to_owned()),
            });
        }
    }
    providers.sort_by(|left, right| left.name.cmp(&right.name));
    let aliases = config
        .models
        .aliases
        .iter()
        .map(|(alias, candidates)| ModelAliasDescriptor {
            alias: ModelAlias(alias.clone()),
            candidates: candidates.clone(),
            current: alias == &config.models.default,
        })
        .collect();
    bound_catalog_snapshot(ModelCatalogSnapshot {
        aliases,
        models: models.into_values().collect(),
        providers,
        cached: false,
        truncated: false,
    })
}

pub(super) fn project_available_provider(
    config: &Config,
    pricing: &PricingTable,
    provider_name: &str,
    catalog: rw_providers::DiscoveredProviderCatalog,
    reverse_aliases: &BTreeMap<String, Vec<ModelAlias>>,
    current_candidate: Option<&String>,
    models: &mut BTreeMap<String, ModelDescriptor>,
) -> ProviderDescriptor {
    let count = u32::try_from(catalog.models.len()).unwrap_or(u32::MAX);
    for discovered in catalog.models {
        let id = format!("{provider_name}/{}", discovered.id);
        if id.len() > MAX_CATALOG_ID_BYTES || id.chars().any(char::is_control) {
            continue;
        }
        let enriched = enrich_discovered_capabilities(
            config,
            pricing,
            provider_name,
            &discovered.id,
            discovered.capabilities,
        );
        models.insert(
            id.clone(),
            ModelDescriptor {
                id: id.clone(),
                display_name: bounded_catalog_text(
                    discovered
                        .display_name
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or(discovered.id),
                ),
                provider: provider_name.to_owned(),
                aliases: reverse_aliases.get(&id).cloned().unwrap_or_default(),
                current: current_candidate == Some(&id),
                available: true,
                status: None,
                capabilities: protocol_capabilities(&enriched),
            },
        );
    }
    let auth_kind = provider_auth_kind(config, provider_name);
    ProviderDescriptor {
        name: provider_name.to_owned(),
        auth_kind,
        next_action: ProviderNextAction::SelectModels,
        configured: true,
        authenticated: true,
        reachable: true,
        model_count: count,
        status: None,
    }
}

pub(super) fn bound_catalog_snapshot(mut snapshot: ModelCatalogSnapshot) -> ModelCatalogSnapshot {
    if snapshot.models.len() > MAX_CATALOG_MODELS {
        snapshot.models.truncate(MAX_CATALOG_MODELS);
        snapshot.truncated = true;
    }
    for provider in &mut snapshot.providers {
        provider.name = bounded_catalog_text(std::mem::take(&mut provider.name));
        if let Some(status) = provider.status.take() {
            provider.status = Some(bounded_catalog_text(status));
        }
    }
    if snapshot.providers.len() > MAX_CATALOG_PROVIDERS {
        snapshot.providers.truncate(MAX_CATALOG_PROVIDERS);
        snapshot.truncated = true;
    }
    if snapshot.aliases.len() > MAX_CATALOG_ALIASES {
        snapshot.aliases.truncate(MAX_CATALOG_ALIASES);
        snapshot.truncated = true;
    }
    for alias in &mut snapshot.aliases {
        alias.alias.0 = bounded_catalog_text(std::mem::take(&mut alias.alias.0));
        if alias.candidates.len() > MAX_CATALOG_ALIAS_CANDIDATES {
            alias.candidates.truncate(MAX_CATALOG_ALIAS_CANDIDATES);
            snapshot.truncated = true;
        }
        for candidate in &mut alias.candidates {
            *candidate = bounded_catalog_text(std::mem::take(candidate));
        }
    }
    loop {
        let encoded_len = serde_json::to_vec(&snapshot).map_or(0, |encoded| encoded.len());
        if encoded_len <= MAX_CATALOG_WIRE_BYTES {
            break;
        }
        let excess = encoded_len.saturating_sub(MAX_CATALOG_WIRE_BYTES);
        if !snapshot.models.is_empty() {
            let remove = snapshot
                .models
                .len()
                .saturating_mul(excess)
                .div_ceil(encoded_len)
                .max(1);
            snapshot
                .models
                .truncate(snapshot.models.len().saturating_sub(remove));
        } else if snapshot.aliases.pop().is_none() && snapshot.providers.pop().is_none() {
            break;
        }
        snapshot.truncated = true;
    }
    snapshot
}

pub(super) fn bounded_catalog_text(mut value: String) -> String {
    let end = value.floor_char_boundary(MAX_CATALOG_TEXT_BYTES.min(value.len()));
    value.truncate(end);
    value
}

pub(super) fn project_unavailable_provider(
    config: &Config,
    provider_name: &str,
    authenticated: bool,
    error: String,
) -> ProviderDescriptor {
    ProviderDescriptor {
        auth_kind: provider_auth_kind(config, provider_name),
        next_action: provider_next_action(provider_auth_kind(config, provider_name), authenticated),
        name: provider_name.to_owned(),
        configured: true,
        authenticated,
        reachable: false,
        model_count: 0,
        status: Some(error),
    }
}

pub(super) fn provider_auth_kind(config: &Config, provider: &str) -> ProviderAuthKind {
    let Some(entry) = config.providers.get(provider) else {
        return ProviderAuthKind::None;
    };
    let oauth_configured = entry.oauth_token_env.is_some()
        || entry.oauth_authorization_endpoint.is_some()
        || entry.oauth_access_token_credential.is_some()
        || entry.oauth_refresh_token_credential.is_some();
    match AdapterKind::parse(provider, &entry.kind) {
        Ok(kind) => kind.auth_kind(oauth_configured),
        Err(_) => ProviderAuthKind::None,
    }
}

pub(super) fn provider_next_action(
    auth_kind: ProviderAuthKind,
    authenticated: bool,
) -> ProviderNextAction {
    if authenticated {
        ProviderNextAction::SelectModels
    } else {
        match auth_kind {
            ProviderAuthKind::Oauth | ProviderAuthKind::DeviceFlow => {
                ProviderNextAction::Authenticate
            }
            ProviderAuthKind::ApiKey => ProviderNextAction::ApiKeyCli,
            ProviderAuthKind::None => ProviderNextAction::None,
        }
    }
}

pub(super) fn enrich_discovered_capabilities(
    config: &Config,
    pricing: &PricingTable,
    provider: &str,
    model: &str,
    discovered: Option<Capabilities>,
) -> Capabilities {
    let kind = config
        .providers
        .get(provider)
        .and_then(|entry| AdapterKind::parse(provider, &entry.kind).ok());
    let catalog_pricing =
        kind.and_then(|kind| find_pricing(pricing, provider, model, kind.catalog_namespace()).1);
    let fallback = kind.map_or_else(
        || model_capabilities(AdapterKind::OpenAiCompatibleChat, catalog_pricing.as_ref()),
        |kind| match kind {
            AdapterKind::OpenAiSubscription => {
                subscription_model_capabilities(catalog_pricing.as_ref())
            }
            AdapterKind::GitHubCopilot => github_copilot_capabilities(catalog_pricing.as_ref()),
            _ => model_capabilities(kind, catalog_pricing.as_ref()),
        },
    );
    discovered.map_or(fallback.clone(), |mut live| {
        live.max_context_tokens = live.max_context_tokens.or(fallback.max_context_tokens);
        live.max_output_tokens = live.max_output_tokens.or(fallback.max_output_tokens);
        live
    })
}

pub(super) fn protocol_capabilities(capabilities: &Capabilities) -> ModelCapabilities {
    ModelCapabilities {
        tool_calling: capabilities.tool_calling,
        vision: capabilities.vision,
        thinking: capabilities.thinking,
        cache_behavior: match capabilities.cache_breakpoints {
            CacheBreakpointSupport::None => ModelCacheBehavior::None,
            CacheBreakpointSupport::Explicit => ModelCacheBehavior::Explicit,
            CacheBreakpointSupport::Automatic => ModelCacheBehavior::ProviderManaged,
        },
        max_context_tokens: capabilities.max_context_tokens,
        max_output_tokens: capabilities.max_output_tokens,
    }
}

pub(super) const fn provider_discovery_status(error: &ProviderError) -> &'static str {
    match error.kind {
        ProviderErrorKind::EffectsUnsettled => "provider effects remain unsettled",
        ProviderErrorKind::Authentication => "provider authentication failed",
        ProviderErrorKind::RateLimited => "provider model discovery was rate limited",
        ProviderErrorKind::Timeout => "provider model discovery timed out",
        ProviderErrorKind::Server => "provider model discovery returned a server error",
        ProviderErrorKind::InvalidRequest => "provider model discovery request was rejected",
        ProviderErrorKind::ContextOverflow => "provider model discovery failed",
        ProviderErrorKind::Protocol => "provider model catalog response was invalid",
        ProviderErrorKind::Network => "provider model discovery network request failed",
        ProviderErrorKind::Cancelled => "provider model discovery was cancelled",
        ProviderErrorKind::ReplayMiss => "provider model discovery is absent from replay",
        ProviderErrorKind::NetworkDisabled => "provider model discovery is disabled by policy",
        ProviderErrorKind::Unsupported => "provider does not support model discovery",
    }
}

pub(super) fn subscription_model_capabilities(pricing: Option<&ModelPricing>) -> Capabilities {
    Capabilities {
        // The subscription transport is intentionally isolated from ordinary
        // OpenAI model discovery. A refreshable catalog may enrich a known id,
        // but never makes that id selectable or proves subscription access.
        // Tool compatibility is a property of the isolated subscription
        // transport, not something models.dev pricing metadata may revoke.
        tool_calling: true,
        vision: pricing.is_some_and(|value| value.supports_vision),
        thinking: true,
        cache_breakpoints: CacheBreakpointSupport::Automatic,
        max_context_tokens: pricing.and_then(|value| value.max_context_tokens),
        max_output_tokens: pricing.and_then(|value| value.max_output_tokens),
        wire_mode: WireMode::OpenAiResponses,
    }
}

pub(super) fn github_copilot_capabilities(pricing: Option<&ModelPricing>) -> Capabilities {
    Capabilities {
        // Copilot is a coding-agent route, so tools must reach lazy discovery;
        // the discovered model record remains the authoritative fail-closed gate.
        tool_calling: true,
        vision: false,
        thinking: pricing.is_some_and(|value| !value.reasoning_efforts.is_empty()),
        cache_breakpoints: CacheBreakpointSupport::None,
        max_context_tokens: pricing.and_then(|value| value.max_context_tokens),
        max_output_tokens: pricing.and_then(|value| value.max_output_tokens),
        wire_mode: WireMode::GitHubCopilot,
    }
}

pub(super) fn random_subscription_session_id(
    provider: &str,
) -> Result<String, ProviderFactoryError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| {
        ProviderFactoryError::new(
            provider,
            "operating-system randomness is unavailable for subscription session id",
        )
    })?;
    let mut value = String::with_capacity(35);
    value.push_str("rw-");
    for byte in bytes {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(value)
}

pub(super) fn model_capabilities(
    kind: AdapterKind,
    pricing: Option<&ModelPricing>,
) -> Capabilities {
    let wire_mode = match kind {
        AdapterKind::Anthropic => WireMode::AnthropicMessages,
        AdapterKind::GitHubCopilot => WireMode::GitHubCopilot,
        AdapterKind::OpenAiResponses
        | AdapterKind::OpenAiSubscription
        | AdapterKind::OpenAiCompatibleResponses => WireMode::OpenAiResponses,
        AdapterKind::OpenAiChat | AdapterKind::OpenAiCompatibleChat => {
            WireMode::OpenAiChatCompletions
        }
    };
    Capabilities {
        tool_calling: pricing.is_some_and(|value| value.supports_tools),
        vision: pricing.is_some_and(|value| value.supports_vision),
        thinking: pricing.is_some_and(|value| {
            value
                .reasoning_efforts
                .iter()
                .any(|effort| *effort != ThinkingLevel::Off)
        }),
        cache_breakpoints: match kind {
            AdapterKind::Anthropic => CacheBreakpointSupport::Explicit,
            AdapterKind::OpenAiResponses
            | AdapterKind::OpenAiChat
            | AdapterKind::OpenAiSubscription => CacheBreakpointSupport::Automatic,
            AdapterKind::GitHubCopilot
            | AdapterKind::OpenAiCompatibleResponses
            | AdapterKind::OpenAiCompatibleChat => CacheBreakpointSupport::None,
        },
        max_context_tokens: pricing.and_then(|value| value.max_context_tokens),
        max_output_tokens: pricing.and_then(|value| value.max_output_tokens),
        wire_mode,
    }
}

pub(super) fn ai_credit_cost(
    pricing: &ModelPricing,
    usage: rw_providers::TokenUsage,
    micros_usd_per_credit: u64,
) -> Cost {
    let nominal = [
        (usage.input_tokens, pricing.input_per_million_micros_usd),
        (usage.output_tokens, pricing.output_per_million_micros_usd),
        (
            usage.cache_read_tokens,
            pricing
                .cache_read_per_million_micros_usd
                .unwrap_or(pricing.input_per_million_micros_usd),
        ),
        (
            usage.cache_write_tokens,
            pricing
                .cache_write_per_million_micros_usd
                .unwrap_or(pricing.input_per_million_micros_usd),
        ),
        (
            usage.reasoning_tokens,
            pricing
                .reasoning_per_million_micros_usd
                .unwrap_or(pricing.output_per_million_micros_usd),
        ),
    ]
    .into_iter()
    .try_fold(0_u64, |total, (tokens, rate)| {
        let component = u128::from(tokens)
            .checked_mul(u128::from(rate))?
            .checked_add(500_000)?
            / 1_000_000;
        total.checked_add(u64::try_from(component).ok()?)
    });
    let Some(nominal) = nominal else {
        return Cost::Unavailable {
            reason: "AI-credit cost exceeds the supported range".to_owned(),
        };
    };
    let credits = (micros_usd_per_credit > 0)
        .then(|| {
            u128::from(nominal)
                .checked_mul(1_000_000)?
                .checked_add(u128::from(micros_usd_per_credit / 2))?
                .checked_div(u128::from(micros_usd_per_credit))
        })
        .flatten()
        .and_then(|credits| u64::try_from(credits).ok());
    credits.map_or_else(
        || Cost::Unavailable {
            reason: "AI-credit conversion exceeds the supported range".to_owned(),
        },
        |credits_micros| Cost::AiCredits {
            credits_micros,
            nominal_amount_micros: Some(nominal.to_string()),
            currency: Some("USD".to_owned()),
        },
    )
}

pub(super) fn nominal_cost_micros(
    pricing: &ModelPricing,
    usage: rw_providers::TokenUsage,
) -> Option<u64> {
    [
        (usage.input_tokens, pricing.input_per_million_micros_usd),
        (usage.output_tokens, pricing.output_per_million_micros_usd),
        (
            usage.cache_read_tokens,
            pricing
                .cache_read_per_million_micros_usd
                .unwrap_or(pricing.input_per_million_micros_usd),
        ),
        (
            usage.cache_write_tokens,
            pricing
                .cache_write_per_million_micros_usd
                .unwrap_or(pricing.input_per_million_micros_usd),
        ),
        (
            usage.reasoning_tokens,
            pricing
                .reasoning_per_million_micros_usd
                .unwrap_or(pricing.output_per_million_micros_usd),
        ),
    ]
    .into_iter()
    .try_fold(0_u64, |total, (tokens, rate)| {
        let component = u128::from(tokens)
            .checked_mul(u128::from(rate))?
            .checked_add(500_000)?
            / 1_000_000;
        total.checked_add(u64::try_from(component).ok()?)
    })
}

pub(super) fn effective_model_metadata(
    model: &ResolvedModel,
    discovered: Option<ProviderModelMetadata>,
) -> ProviderModelMetadata {
    let capabilities = discovered.as_ref().map_or_else(
        || model.capabilities.clone(),
        |value| value.capabilities.clone(),
    );
    if model.accounting == UsageAccounting::SubscriptionQuota {
        return ProviderModelMetadata {
            capabilities,
            pricing: None,
            accounting: model.accounting,
        };
    }
    if matches!(model.accounting, UsageAccounting::AiCredits { .. }) {
        return ProviderModelMetadata {
            capabilities,
            pricing: discovered
                .and_then(|value| value.pricing)
                .or_else(|| model.pricing.clone()),
            accounting: model.accounting,
        };
    }
    let discovered_pricing = discovered.and_then(|value| value.pricing);
    let pricing = if model.pricing_source == Some(ModelPricingSource::UserConfig) {
        model.pricing.clone()
    } else {
        discovered_pricing.or_else(|| model.pricing.clone())
    };
    ProviderModelMetadata {
        capabilities,
        accounting: if pricing.is_some() {
            UsageAccounting::ApiDollars
        } else {
            UsageAccounting::UnpricedApi
        },
        pricing,
    }
}

/// Converts provider-neutral model metadata and normalized usage into a typed cost.
#[must_use]
pub fn cost_from_model_metadata(
    metadata: &ProviderModelMetadata,
    usage: rw_providers::TokenUsage,
) -> Cost {
    match metadata.accounting {
        UsageAccounting::ApiDollars => metadata.pricing.as_ref().map_or_else(
            || Cost::Unavailable {
                reason: "authoritative API pricing is unavailable".to_owned(),
            },
            |pricing| {
                nominal_cost_micros(pricing, usage).map_or_else(
                    || Cost::Unavailable {
                        reason: "API cost exceeds the supported range".to_owned(),
                    },
                    |amount_micros| Cost::Monetary {
                        amount_micros,
                        currency: "USD".to_owned(),
                    },
                )
            },
        ),
        UsageAccounting::AiCredits {
            micros_usd_per_credit,
        } => metadata.pricing.as_ref().map_or_else(
            || Cost::Unavailable {
                reason: "authoritative AI-credit pricing is unavailable".to_owned(),
            },
            |pricing| ai_credit_cost(pricing, usage, micros_usd_per_credit),
        ),
        UsageAccounting::SubscriptionQuota => Cost::SubscriptionQuota {
            used: Some(total_usage_tokens(usage).to_string()),
            unit: Some("tokens".to_owned()),
        },
        UsageAccounting::UnpricedApi => Cost::Unavailable {
            reason: "authoritative API pricing is unavailable".to_owned(),
        },
    }
}

pub(super) fn total_usage_tokens(usage: rw_providers::TokenUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_write_tokens)
        .saturating_add(usage.reasoning_tokens)
}
