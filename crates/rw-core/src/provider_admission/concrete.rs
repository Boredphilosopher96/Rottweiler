use super::{ProviderInvocation, gate::InvocationGate};
use rw_providers::{ModelCandidate, ProviderAttemptGate, ProviderModelMetadata};
use std::{collections::BTreeMap, sync::Arc};

/// Binds an exact provider route to the same admission and receipt policy used by aliases.
#[must_use]
pub fn concrete_attempt_gate(
    invocation: ProviderInvocation,
    candidate: ModelCandidate,
    metadata: Option<ProviderModelMetadata>,
) -> Arc<dyn ProviderAttemptGate> {
    Arc::new(InvocationGate {
        invocation,
        metadata: metadata.map_or_else(BTreeMap::new, |metadata| {
            BTreeMap::from([(candidate, metadata)])
        }),
    })
}
