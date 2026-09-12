//! Explicit accounting-free gate for provider wire-contract fixtures.

use async_trait::async_trait;
use rw_providers::{
    ModelCandidate, ProviderAttempt, ProviderAttemptGate, ProviderAttemptOutcome, ProviderError,
    ProviderRequest,
};
use std::sync::Arc;

pub fn gate() -> Arc<dyn ProviderAttemptGate> {
    Arc::new(FixtureGate)
}
struct FixtureGate;
struct FixtureAttempt;

#[async_trait]
impl ProviderAttemptGate for FixtureGate {
    async fn enter(
        &self,
        _: &ModelCandidate,
        _: &ProviderRequest,
        _: u32,
    ) -> Result<Box<dyn ProviderAttempt>, ProviderError> {
        Ok(Box::new(FixtureAttempt))
    }
}

#[async_trait]
impl ProviderAttempt for FixtureAttempt {
    async fn settle(self: Box<Self>, _: ProviderAttemptOutcome) -> Result<(), ProviderError> {
        Ok(())
    }
}
