//! Retirement tasks remain owned when a navigation waiter disappears.
use super::{
    ExclusiveInvocationGuard, ExtensionInvocations, Generation, Phase, error, unavailable,
};
use crate::PluginRpcError;
use futures_util::FutureExt as _;
use std::{panic::AssertUnwindSafe, sync::Arc, time::Duration};
use tokio::sync::watch;

const PROOF_DEADLINE: Duration = Duration::from_secs(5);

impl ExtensionInvocations {
    /// Closes admission, retires every old endpoint and drains admitted calls.
    /// The owned retirement task survives caller cancellation. No raw endpoint
    /// is released on a failed proof, timeout or panic.
    /// # Errors
    /// Returns a sticky failed-proof error or rejects another exclusive holder.
    pub async fn pause_and_settle(
        self: &Arc<Self>,
    ) -> Result<ExclusiveInvocationGuard, PluginRpcError> {
        let (mut completion, deadline, start) = {
            let mut state = self.lock();
            if state.phase == Phase::Failed {
                return Err(unavailable(&state));
            }
            if state.phase == Phase::Exclusive {
                if state.claimed {
                    return Err(error(
                        "busy",
                        "extension generation is already held exclusively",
                    ));
                }
                state.claimed = true;
                return Ok(ExclusiveInvocationGuard {
                    gate: Arc::clone(self),
                    generation: state.generation.id,
                    resumed: false,
                });
            }
            let start = state.phase == Phase::Open;
            if start {
                state.phase = Phase::Retiring;
                state.retirement = Some(watch::channel(None).0);
                state.retirement_deadline = Some(tokio::time::Instant::now() + PROOF_DEADLINE);
                for cancellation in state.active.values() {
                    cancellation.cancel();
                }
            }
            let completion = state
                .retirement
                .as_ref()
                .ok_or_else(|| error("effects_unsettled", "retirement proof owner is missing"))?
                .subscribe();
            let deadline = state
                .retirement_deadline
                .ok_or_else(|| error("effects_unsettled", "retirement deadline is missing"))?;
            (completion, deadline, start)
        };
        if start {
            self.start_retirement(deadline);
        }
        let proof = async {
            loop {
                if let Some(result) = completion.borrow_and_update().clone() {
                    return result;
                }
                completion.changed().await.map_err(|_| {
                    error(
                        "effects_unsettled",
                        "extension retirement proof channel closed",
                    )
                })?;
            }
        };
        if let Ok(result) = tokio::time::timeout_at(deadline, proof).await {
            result?;
        } else {
            let failure = error(
                "effects_unsettled",
                "extension retirement proof deadline expired",
            );
            self.fail(failure.clone());
            return Err(failure);
        }
        let mut state = self.lock();
        if state.phase != Phase::Exclusive {
            return Err(unavailable(&state));
        }
        if state.claimed {
            return Err(error(
                "busy",
                "extension generation is already held exclusively",
            ));
        }
        state.claimed = true;
        Ok(ExclusiveInvocationGuard {
            gate: Arc::clone(self),
            generation: state.generation.id,
            resumed: false,
        })
    }

    fn start_retirement(self: &Arc<Self>, deadline: tokio::time::Instant) {
        let generation = self.lock().generation.id;
        let deadline_gate = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            if let Some(gate) = deadline_gate.upgrade() {
                let expired = {
                    let state = gate.lock();
                    state.phase == Phase::Retiring && state.generation.id == generation
                };
                if expired {
                    gate.fail(error(
                        "effects_unsettled",
                        "extension retirement proof deadline expired",
                    ));
                }
            }
        });
        let gate = Arc::clone(self);
        tokio::spawn(async move {
            match AssertUnwindSafe(retire(&gate)).catch_unwind().await {
                Ok(Ok(())) => {}
                Ok(Err(failure)) => gate.fail(failure),
                Err(_) => gate.fail(error(
                    "effects_unsettled",
                    "extension retirement owner panicked",
                )),
            }
        });
    }
}

async fn retire(gate: &Arc<ExtensionInvocations>) -> Result<(), PluginRpcError> {
    let generation = Arc::clone(&gate.lock().generation);
    close_generation(&generation).await?;
    loop {
        let changed = gate.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        {
            let mut state = gate.lock();
            if state.phase == Phase::Failed {
                return Err(unavailable(&state));
            }
            if state.active.is_empty() {
                state.phase = Phase::Exclusive;
                if let Some(retirement) = &state.retirement {
                    retirement.send_replace(Some(Ok(())));
                }
                return Ok(());
            }
        }
        changed.await;
    }
}

pub(super) async fn close_generation(generation: &Generation) -> Result<(), PluginRpcError> {
    // The source cardinality limit bounds futures and endpoint references.
    // Each close is polled even if a sibling panics or reports failed proof.
    let outcomes =
        futures_util::future::join_all(generation.endpoints.iter().map(|endpoint| async move {
            AssertUnwindSafe(endpoint.close()).catch_unwind().await
        }))
        .await;
    if outcomes
        .iter()
        .any(|outcome| !matches!(outcome, Ok(Ok(()))))
    {
        Err(error(
            "effects_unsettled",
            "an extension endpoint could not prove retirement",
        ))
    } else {
        Ok(())
    }
}
