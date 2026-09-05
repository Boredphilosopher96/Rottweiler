#![cfg(test)]
#![allow(clippy::expect_used)]
use super::SessionAccountingState;
use rw_types::{Cost, Usage};

fn usage() -> Usage {
    Usage {
        input_tokens: 2,
        output_tokens: 3,
        cache_read_tokens: 5,
        cache_write_tokens: 7,
        reasoning_tokens: 11,
    }
}
#[test]
fn cumulative_accounting_retains_fixed_metadata_as_history_grows() {
    let mut state = SessionAccountingState::default();
    for _ in 0..100_000 {
        state.record_actuals(
            &usage(),
            &Cost::Monetary {
                amount_micros: 13,
                currency: "USD".into(),
            },
        );
    }
    assert_eq!(state.entries, 100_000);
    assert_eq!(state.usage.output_tokens, 300_000);
    assert_eq!(state.usage.cache_read_tokens, 500_000);
    assert_eq!(state.cost_micros_usd, 1_300_000);
    assert!(serde_json::to_vec(&state).expect("bounded state").len() < 1024);
}
#[test]
fn incompatible_dispositions_keep_exact_separate_counters() {
    let mut state = SessionAccountingState::default();
    for cost in [
        Cost::Monetary {
            amount_micros: 10,
            currency: "EUR".into(),
        },
        Cost::SubscriptionQuota {
            used: Some("123".into()),
            unit: Some("tokens".into()),
        },
        Cost::SubscriptionQuota {
            used: None,
            unit: Some("tokens".into()),
        },
        Cost::Unavailable {
            reason: "unknown".into(),
        },
    ] {
        state.record_actuals(&usage(), &cost);
    }
    assert_eq!(state.non_usd_monetary_entries, 1);
    assert_eq!(state.subscription_quota_entries, 2);
    assert_eq!(state.subscription_tokens, 123);
    assert_eq!(state.unmetered_subscription_quota_entries, 1);
    assert_eq!(state.cost_unavailable_entries, 1);
    assert_eq!(state.subscription_quota().expect("quota").used, "123");
}
