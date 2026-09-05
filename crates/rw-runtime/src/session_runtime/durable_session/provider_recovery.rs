//! Resume settles exact pending receipts under the source journal's exclusive writer lease.
use super::DurableEventSink;
use crate::provider_admission::DurableProviderAdmission;
use miette::{Result, miette};
use rw_store::session::reservations::{MAX_ACTIVE_PROVIDER_CALLS, ProviderCallPhase};
use std::sync::Arc;

impl DurableEventSink {
    pub(in crate::session_runtime) async fn reconcile_provider_attempts(
        &self,
        admission: &Arc<DurableProviderAdmission>,
    ) -> Result<()> {
        // This owner retains SessionEventLog and its exclusive source writer lock.
        // Calls run before actor/provider admission; workspace effects have already
        // been settled by the execution lease held by composition.
        let mut after = None;
        let mut observed = 0u128;
        loop {
            let pending = admission
                .pending_for_session(self.session_id.clone(), after.clone(), 128)
                .await
                .map_err(|error| miette!("pending provider accounting could not load: {error}"))?;
            if pending.is_empty() {
                return Ok(());
            }
            for call in pending {
                observed += 1;
                if observed > MAX_ACTIVE_PROVIDER_CALLS {
                    return Err(miette!(
                        "pending provider accounting exceeds its authority bound"
                    ));
                }
                after = Some(call.identity.clone());
                let identity = call.identity.clone();
                let receipt = self
                    .read_canonical(move |history| {
                        if history.head().next_sequence == 0 {
                            Ok(None)
                        } else {
                            history.provider_receipt(&identity)
                        }
                    })
                    .await
                    .map_err(|error| miette!("provider receipt could not resolve: {error}"))?;
                if let Some(receipt) = receipt {
                    admission
                        .reconcile_accounted(receipt)
                        .await
                        .map_err(|error| {
                            miette!("provider receipt could not reconcile: {error}")
                        })?;
                } else if call.phase == ProviderCallPhase::Reserved {
                    admission
                        .recover_unstarted(call.identity)
                        .await
                        .map_err(|error| {
                            miette!("unstarted provider reservation could not release: {error}")
                        })?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
