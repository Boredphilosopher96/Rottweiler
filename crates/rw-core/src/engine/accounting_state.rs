//! Cumulative session accounting with no retained per-turn payloads.
mod quota;
use quota::Quota;
use rw_types::{
    Cost, SubscriptionTokenAccounting, TurnAccounting, Usage, billing::SubscriptionQuotaSummary,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionAccountingState {
    pub usage: Usage,
    pub entries: u64,
    pub cost_micros_usd: u64,
    pub ai_credit_micros: u64,
    pub subscription_tokens: u64,
    pub subscription_quota_entries: u64,
    pub unmetered_subscription_quota_entries: u64,
    pub cost_unavailable_entries: u64,
    pub non_usd_monetary_entries: u64,
    quota: Quota,
}
impl Default for SessionAccountingState {
    fn default() -> Self {
        Self {
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            entries: 0,
            cost_micros_usd: 0,
            ai_credit_micros: 0,
            subscription_tokens: 0,
            subscription_quota_entries: 0,
            unmetered_subscription_quota_entries: 0,
            cost_unavailable_entries: 0,
            non_usd_monetary_entries: 0,
            quota: Quota::default(),
        }
    }
}
impl SessionAccountingState {
    pub(crate) fn record(&mut self, entry: &TurnAccounting) {
        self.record_actuals(&entry.usage, &entry.cost);
    }
    pub(crate) fn record_actuals(&mut self, usage: &Usage, cost: &Cost) {
        self.entries = self.entries.saturating_add(1);
        super::turn::accounting::add_usage(&mut self.usage, usage);
        match cost {
            Cost::Monetary {
                amount_micros,
                currency,
            } if currency.eq_ignore_ascii_case("USD") => {
                self.cost_micros_usd = self.cost_micros_usd.saturating_add(*amount_micros);
            }
            Cost::Monetary { .. } => {
                self.non_usd_monetary_entries = self.non_usd_monetary_entries.saturating_add(1);
            }
            Cost::AiCredits { credits_micros, .. } => {
                self.ai_credit_micros = self.ai_credit_micros.saturating_add(*credits_micros);
            }
            Cost::Unavailable { .. } => {
                self.cost_unavailable_entries = self.cost_unavailable_entries.saturating_add(1);
            }
            Cost::SubscriptionQuota { used, unit } => {
                self.subscription_quota_entries = self.subscription_quota_entries.saturating_add(1);
                if let Some(used) = used {
                    self.quota.add(used, unit.as_deref());
                }
                match cost.subscription_token_accounting() {
                    SubscriptionTokenAccounting::Metered(tokens) => {
                        self.subscription_tokens = self.subscription_tokens.saturating_add(tokens);
                    }
                    SubscriptionTokenAccounting::Unavailable => {
                        self.unmetered_subscription_quota_entries =
                            self.unmetered_subscription_quota_entries.saturating_add(1);
                    }
                    SubscriptionTokenAccounting::NotApplicable => {}
                }
            }
        }
    }
    #[must_use]
    pub fn subscription_quota(&self) -> Option<SubscriptionQuotaSummary> {
        self.quota.summary()
    }
}

#[cfg(test)]
mod tests;
