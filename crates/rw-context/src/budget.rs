//! Local token budgets reconciled against provider-reported usage.

use rw_providers::{TokenUsage, ToolDefinition};
use rw_types::Turn;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::LocalTokenEstimator;

const FACTOR_SCALE: u64 = 1_000_000;
const MIN_FACTOR: u64 = 250_000;
const MAX_FACTOR: u64 = 4_000_000;

/// A local estimate before and after provider reconciliation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BudgetEstimate {
    pub local_tokens: u64,
    pub reconciled_tokens: u64,
    /// Rolling correction factor in millionths; 1,000,000 means 1.0.
    pub correction_millionths: u64,
}

/// Result of incorporating one authoritative provider usage report.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Reconciliation {
    pub estimated_input_tokens: u64,
    pub provider_input_tokens: u64,
    pub correction_millionths: u64,
    pub sample_count: u64,
}

/// Snapshot persisted or exposed by a context inspector.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BudgetSnapshot {
    pub estimated_input_total: u64,
    pub provider_input_total: u64,
    pub correction_millionths: u64,
    pub sample_count: u64,
}

/// Stateful rolling reconciliation over a deterministic local estimator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Budgeter {
    estimated_input_total: u64,
    provider_input_total: u64,
    correction_millionths: u64,
    sample_count: u64,
}

impl Default for Budgeter {
    fn default() -> Self {
        Self {
            estimated_input_total: 0,
            provider_input_total: 0,
            correction_millionths: FACTOR_SCALE,
            sample_count: 0,
        }
    }
}

impl Budgeter {
    /// Estimates the complete provider input using the current correction.
    #[must_use]
    pub fn estimate(&self, turns: &[Turn], tools: &[ToolDefinition]) -> BudgetEstimate {
        let local_tokens = turns
            .iter()
            .fold(LocalTokenEstimator::tools(tools), |total, turn| {
                total.saturating_add(LocalTokenEstimator::turn(turn))
            });
        BudgetEstimate {
            local_tokens,
            reconciled_tokens: scale_ceil(local_tokens, self.correction_millionths),
            correction_millionths: self.correction_millionths,
        }
    }

    /// Reconciles an estimate with normalized provider input partitions.
    ///
    /// Cache reads and writes are included because [`TokenUsage`] defines
    /// `input_tokens` as the non-cached partition, avoiding double counting.
    pub fn reconcile(&mut self, estimated_input_tokens: u64, usage: TokenUsage) -> Reconciliation {
        let provider_input_tokens = usage
            .input_tokens
            .saturating_add(usage.cache_read_tokens)
            .saturating_add(usage.cache_write_tokens);
        if estimated_input_tokens > 0 && provider_input_tokens > 0 {
            self.estimated_input_total = self
                .estimated_input_total
                .saturating_add(estimated_input_tokens);
            self.provider_input_total = self
                .provider_input_total
                .saturating_add(provider_input_tokens);
            self.sample_count = self.sample_count.saturating_add(1);
            self.correction_millionths =
                ratio_millionths(self.provider_input_total, self.estimated_input_total)
                    .clamp(MIN_FACTOR, MAX_FACTOR);
        }
        Reconciliation {
            estimated_input_tokens,
            provider_input_tokens,
            correction_millionths: self.correction_millionths,
            sample_count: self.sample_count,
        }
    }

    /// Returns the rolling estimator state.
    #[must_use]
    pub const fn snapshot(&self) -> BudgetSnapshot {
        BudgetSnapshot {
            estimated_input_total: self.estimated_input_total,
            provider_input_total: self.provider_input_total,
            correction_millionths: self.correction_millionths,
            sample_count: self.sample_count,
        }
    }
}

/// Overflow/compaction policy derived from provider model metadata.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OverflowPolicy {
    pub context_window_tokens: u64,
    pub max_output_tokens: u64,
    pub reserved_tokens_override: Option<u64>,
    pub automatic_compaction: bool,
}

impl OverflowPolicy {
    /// Validates the context window and explicit user reserve.
    ///
    /// # Errors
    ///
    /// Rejects a zero context window or an explicit reserve that consumes the
    /// entire window.
    pub const fn validate(self) -> Result<Self, OverflowPolicyError> {
        if self.context_window_tokens == 0 {
            return Err(OverflowPolicyError::ZeroContextWindow);
        }
        if let Some(reserved) = self.reserved_tokens_override
            && reserved >= self.context_window_tokens
        {
            return Err(OverflowPolicyError::ExplicitReserveExhaustsWindow {
                reserved_tokens: reserved,
                context_window_tokens: self.context_window_tokens,
            });
        }
        Ok(self)
    }

    /// Default ADR-010 reserve, capped at half of the context window.
    #[must_use]
    pub fn reserved_tokens(self) -> u64 {
        self.reserved_tokens_override.unwrap_or_else(|| {
            20_000_u64
                .min(self.max_output_tokens)
                .min(self.context_window_tokens / 2)
        })
    }

    /// Calculates both physical overflow and whether auto-compaction should run.
    #[must_use]
    pub fn calculate(self, total_tokens: u64) -> OverflowDecision {
        let reserved_tokens = self.reserved_tokens();
        let threshold_tokens = self.context_window_tokens.saturating_sub(reserved_tokens);
        let would_overflow = total_tokens >= threshold_tokens;
        OverflowDecision {
            total_tokens,
            reserved_tokens,
            threshold_tokens,
            remaining_before_threshold: threshold_tokens.saturating_sub(total_tokens),
            would_overflow,
            should_compact: self.automatic_compaction && would_overflow,
        }
    }
}

/// Invalid user-supplied overflow policy.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum OverflowPolicyError {
    #[error("context window must be greater than zero")]
    ZeroContextWindow,
    #[error(
        "explicit reserve {reserved_tokens} must be smaller than context window {context_window_tokens}"
    )]
    ExplicitReserveExhaustsWindow {
        reserved_tokens: u64,
        context_window_tokens: u64,
    },
}

/// Exact result of an overflow threshold calculation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OverflowDecision {
    pub total_tokens: u64,
    pub reserved_tokens: u64,
    pub threshold_tokens: u64,
    pub remaining_before_threshold: u64,
    pub would_overflow: bool,
    pub should_compact: bool,
}

fn ratio_millionths(numerator: u64, denominator: u64) -> u64 {
    let scaled = u128::from(numerator).saturating_mul(u128::from(FACTOR_SCALE));
    let ratio = scaled / u128::from(denominator);
    u64::try_from(ratio).unwrap_or(u64::MAX)
}

fn scale_ceil(value: u64, factor: u64) -> u64 {
    let scaled = u128::from(value).saturating_mul(u128::from(factor));
    let rounded = scaled.saturating_add(u128::from(FACTOR_SCALE - 1)) / u128::from(FACTOR_SCALE);
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use rw_providers::TokenUsage;

    use super::{Budgeter, OverflowPolicy};

    #[test]
    fn reconciliation_includes_all_input_partitions() {
        let mut budgeter = Budgeter::default();
        let result = budgeter.reconcile(
            100,
            TokenUsage {
                input_tokens: 50,
                cache_read_tokens: 60,
                cache_write_tokens: 40,
                ..TokenUsage::default()
            },
        );
        assert_eq!(result.provider_input_tokens, 150);
        assert_eq!(result.correction_millionths, 1_500_000);
    }

    #[test]
    fn overflow_uses_exact_boundary_and_default_reserve() {
        let policy = OverflowPolicy {
            context_window_tokens: 100_000,
            max_output_tokens: 32_000,
            reserved_tokens_override: None,
            automatic_compaction: true,
        };
        assert!(!policy.calculate(79_999).should_compact);
        let at_boundary = policy.calculate(80_000);
        assert!(at_boundary.should_compact);
        assert_eq!(at_boundary.reserved_tokens, 20_000);
    }

    #[test]
    fn disabling_auto_preserves_overflow_diagnostic() {
        let decision = OverflowPolicy {
            context_window_tokens: 10,
            max_output_tokens: 5,
            reserved_tokens_override: None,
            automatic_compaction: false,
        }
        .calculate(5);
        assert!(decision.would_overflow);
        assert!(!decision.should_compact);
    }

    #[test]
    fn default_reserve_cannot_exhaust_the_context_window() {
        let policy = OverflowPolicy {
            context_window_tokens: 10_000,
            max_output_tokens: 20_000,
            reserved_tokens_override: None,
            automatic_compaction: true,
        };
        let decision = policy.calculate(0);
        assert_eq!(decision.reserved_tokens, 5_000);
        assert_eq!(decision.threshold_tokens, 5_000);
        assert!(!decision.should_compact);
    }

    #[test]
    fn explicit_reserve_must_leave_input_capacity() {
        let policy = OverflowPolicy {
            context_window_tokens: 10_000,
            max_output_tokens: 2_000,
            reserved_tokens_override: Some(10_000),
            automatic_compaction: true,
        };
        assert!(policy.validate().is_err());
        assert!(
            OverflowPolicy {
                reserved_tokens_override: Some(9_999),
                ..policy
            }
            .validate()
            .is_ok()
        );
    }
}
