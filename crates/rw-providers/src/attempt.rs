//! Provider-neutral admission for each actual retry or failover attempt.

use async_trait::async_trait;

use crate::{ModelCandidate, ProviderError, ProviderRequest, TokenUsage};

/// Accounting observations retained by the provider operation owner until settlement.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProviderAttemptOutcome {
    /// Latest normalized usage seen during this attempt.
    pub usage: Option<TokenUsage>,
    /// A provider terminal was observed without a subsequent protocol/stream error.
    /// Missing terminal usage cannot be treated as zero cost.
    pub terminal: bool,
}

/// Session-supplied gate executed before each concrete provider invocation.
#[async_trait]
pub trait ProviderAttemptGate: Send + Sync {
    /// Reserves this exact candidate/attempt and records its started state.
    /// Attempt numbers increase across the whole logical call, including failover.
    async fn enter(
        &self,
        candidate: &ModelCandidate,
        request: &ProviderRequest,
        attempt: u32,
    ) -> Result<Box<dyn ProviderAttempt>, ProviderError>;
}

/// Accounting ownership retained alongside the provider's local effect owner.
#[async_trait]
pub trait ProviderAttempt: Send {
    /// Invoked after local provider effects have settled, even when the consumer
    /// disappears. Actual usage must be durably recorded before releasing a charge.
    async fn settle(self: Box<Self>, outcome: ProviderAttemptOutcome) -> Result<(), ProviderError>;
}
