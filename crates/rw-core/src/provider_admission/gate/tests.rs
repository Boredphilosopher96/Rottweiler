#![allow(clippy::unwrap_used)]

use super::*;
use rw_providers::{CacheBreakpointSupport, Capabilities, ModelPricing, WireMode};

fn metadata(accounting: UsageAccounting) -> ProviderModelMetadata {
    ProviderModelMetadata {
        capabilities: Capabilities {
            tool_calling: true,
            vision: false,
            thinking: true,
            cache_breakpoints: CacheBreakpointSupport::Automatic,
            max_context_tokens: Some(100_000),
            max_output_tokens: Some(10_000),
            wire_mode: WireMode::NormalizedReplay,
        },
        pricing: Some(ModelPricing {
            input_per_million_micros_usd: 3_000_000,
            output_per_million_micros_usd: 15_000_000,
            cache_read_per_million_micros_usd: Some(300_000),
            cache_write_per_million_micros_usd: Some(3_750_000),
            reasoning_per_million_micros_usd: Some(20_000_000),
            ..ModelPricing::default()
        }),
        accounting,
    }
}

#[test]
fn priced_component_bounds_cover_normalized_usage_in_each_billing_unit() {
    for accounting in [
        UsageAccounting::ApiDollars,
        UsageAccounting::AiCredits {
            micros_usd_per_credit: 10_000,
        },
        UsageAccounting::SubscriptionQuota,
    ] {
        let metadata = metadata(accounting);
        let bound = charge_bound(Some(&metadata), ProviderInputBudget::Bounded(1000), 100)
            .unwrap()
            .charge()
            .unwrap()
            .amount();
        for input in [0, 1, 997, 1000] {
            for output in [0, 1, 97, 100] {
                let usage = TokenUsage {
                    input_tokens: input,
                    cache_read_tokens: input,
                    cache_write_tokens: input,
                    output_tokens: output,
                    reasoning_tokens: output,
                };
                let actual = crate::provider_factory::cost_from_model_metadata(&metadata, usage);
                let amount = match actual {
                    Cost::Monetary { amount_micros, .. } => amount_micros,
                    Cost::AiCredits { credits_micros, .. } => credits_micros,
                    Cost::SubscriptionQuota {
                        used: Some(value), ..
                    } => value.parse().unwrap(),
                    other => panic!("unexpected accounting: {other:?}"),
                };
                assert!(amount <= bound);
            }
        }
    }
}

#[test]
fn estimates_and_missing_pricing_never_become_strict_bounds() {
    assert!(matches!(
        charge_bound(
            Some(&metadata(UsageAccounting::ApiDollars)),
            ProviderInputBudget::Estimated(1000),
            100
        )
        .unwrap(),
        BudgetChargeBound::BestEffort(Some(_))
    ));
    assert_eq!(
        charge_bound(None, ProviderInputBudget::Bounded(1000), 100).unwrap(),
        BudgetChargeBound::BestEffort(None)
    );
    let mut unknown = metadata(UsageAccounting::UnpricedApi);
    unknown.pricing = None;
    assert_eq!(
        charge_bound(Some(&unknown), ProviderInputBudget::Bounded(1000), 100).unwrap(),
        BudgetChargeBound::BestEffort(None)
    );
}

#[test]
fn pricing_overflow_and_invalid_credit_conversion_cannot_become_free_calls() {
    let mut priced = metadata(UsageAccounting::ApiDollars);
    priced
        .pricing
        .as_mut()
        .unwrap()
        .input_per_million_micros_usd = u64::MAX;
    assert!(matches!(
        charge_bound(Some(&priced), ProviderInputBudget::Bounded(1_000_001), 100),
        Err(BudgetReservationError::Arithmetic)
    ));
    let invalid = metadata(UsageAccounting::AiCredits {
        micros_usd_per_credit: 0,
    });
    assert!(matches!(
        charge_bound(Some(&invalid), ProviderInputBudget::Bounded(1000), 100),
        Err(BudgetReservationError::Arithmetic)
    ));
}
