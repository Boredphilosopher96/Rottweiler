//! Binds one logical invocation to concrete route pricing and durable accounting.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use rw_providers::{
    ModelCandidate, ProviderAttempt, ProviderAttemptGate, ProviderAttemptOutcome, ProviderError,
    ProviderErrorKind, ProviderModelMetadata, ProviderRequest, TokenUsage, UsageAccounting,
};
use rw_store::session::UtcTimestamp;
use rw_types::{Cost, Usage};

use super::{
    ActiveProviderCall, BudgetCharge, BudgetChargeBound, BudgetReservationError,
    BudgetReservationPlan, ProviderAccountingSink, ProviderCallActuals, ProviderCallIdentity,
    ProviderInputBudget, ProviderInvocation,
};

pub(crate) struct InvocationGate {
    pub(crate) invocation: ProviderInvocation,
    pub(crate) metadata: BTreeMap<ModelCandidate, ProviderModelMetadata>,
}

struct AccountedAttempt {
    active: Box<dyn ActiveProviderCall>,
    identity: ProviderCallIdentity,
    metadata: Option<ProviderModelMetadata>,
    accounting: Arc<dyn ProviderAccountingSink>,
}

#[async_trait]
impl ProviderAttemptGate for InvocationGate {
    async fn enter(
        &self,
        candidate: &ModelCandidate,
        request: &ProviderRequest,
        attempt: u32,
    ) -> Result<Box<dyn ProviderAttempt>, ProviderError> {
        let invocation = &self.invocation;
        let identity = ProviderCallIdentity {
            budget_session_id: invocation.budget_session_id.clone(),
            session_id: invocation.session_id.clone(),
            turn_id: invocation.turn_id.clone(),
            attribution: invocation.attribution.clone(),
            call_id: invocation.call_id.clone(),
            attempt,
        };
        let metadata = self.metadata.get(candidate).cloned();
        let output = u64::from(request.max_output_tokens);
        let input = match invocation.input {
            ProviderInputBudget::Bounded(value) | ProviderInputBudget::Estimated(value) => value,
        };
        let charge = charge_bound(metadata.as_ref(), invocation.input, output)
            .map_err(|error| admission_error(&error))?;
        let plan = BudgetReservationPlan {
            identity: identity.clone(),
            admitted_at: UtcTimestamp::from_unix_millis(invocation.clock.unix_time_millis())
                .map_err(BudgetReservationError::from)
                .map_err(|error| admission_error(&error))?,
            input_token_bound: input,
            output_token_limit: output,
            charge,
            budget: invocation.budget.clone(),
        };
        let active = invocation
            .admission
            .reserve(plan)
            .await
            .map_err(|error| admission_error(&error))?
            .start()
            .await
            .map_err(|error| admission_error(&error))?;
        Ok(Box::new(AccountedAttempt {
            active,
            identity,
            metadata,
            accounting: invocation.accounting.clone(),
        }))
    }
}

#[async_trait]
impl ProviderAttempt for AccountedAttempt {
    async fn settle(
        mut self: Box<Self>,
        outcome: ProviderAttemptOutcome,
    ) -> Result<(), ProviderError> {
        let cost = if outcome.terminal {
            match (self.metadata.as_ref(), outcome.usage) {
                (Some(metadata), Some(usage)) => {
                    crate::provider_factory::cost_from_model_metadata(metadata, usage)
                }
                _ => Cost::Unavailable {
                    reason: "provider terminal has no authoritative usage or pricing".to_owned(),
                },
            }
        } else {
            Cost::Unavailable {
                reason: "provider attempt ended without an authoritative terminal".to_owned(),
            }
        };
        let usage = outcome.usage.unwrap_or_default();
        let actuals = ProviderCallActuals {
            usage: Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
            },
            cost,
        };
        let receipt = self
            .accounting
            .append_accounted(self.identity, actuals)
            .await
            .map_err(|error| admission_error(&error))?;
        self.active
            .settle_accounted(receipt)
            .await
            .map_err(|error| admission_error(&error))
    }
}

fn charge_bound(
    metadata: Option<&ProviderModelMetadata>,
    input: ProviderInputBudget,
    output: u64,
) -> Result<BudgetChargeBound, BudgetReservationError> {
    let tokens = match input {
        ProviderInputBudget::Bounded(tokens) | ProviderInputBudget::Estimated(tokens) => tokens,
    };
    // Each billing component has its own bound. Do not assume provider cache or
    // reasoning counters are disjoint when deriving the maximum charged amount.
    tokens
        .checked_mul(3)
        .and_then(|value| {
            output
                .checked_mul(2)
                .and_then(|other| value.checked_add(other))
        })
        .ok_or(BudgetReservationError::Arithmetic)?;
    let maximum = TokenUsage {
        input_tokens: tokens,
        cache_read_tokens: tokens,
        cache_write_tokens: tokens,
        output_tokens: output,
        reasoning_tokens: output,
    };
    let charge = metadata.and_then(|metadata| {
        match crate::provider_factory::cost_from_model_metadata(metadata, maximum) {
            Cost::Monetary {
                amount_micros,
                currency,
            } if currency == "USD" => Some(BudgetCharge::UsdMicros(amount_micros)),
            Cost::AiCredits { credits_micros, .. } => {
                Some(BudgetCharge::AiCreditMicros(credits_micros))
            }
            cost @ Cost::SubscriptionQuota { .. } => match cost.subscription_token_accounting() {
                rw_types::SubscriptionTokenAccounting::Metered(tokens) => {
                    Some(BudgetCharge::SubscriptionTokens(tokens))
                }
                _ => None,
            },
            _ => None,
        }
    });
    let priced = metadata.is_some_and(|metadata| match metadata.accounting {
        UsageAccounting::ApiDollars | UsageAccounting::AiCredits { .. } => {
            metadata.pricing.is_some()
        }
        UsageAccounting::SubscriptionQuota => true,
        UsageAccounting::UnpricedApi => false,
    });
    if priced && charge.is_none() {
        return Err(BudgetReservationError::Arithmetic);
    }
    Ok(match (input, charge) {
        (ProviderInputBudget::Bounded(_), Some(charge)) => BudgetChargeBound::Bounded(charge),
        (_, charge) => BudgetChargeBound::BestEffort(charge),
    })
}

fn admission_error(error: &BudgetReservationError) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, error.to_string())
}

#[cfg(test)]
mod tests;
