//! Byte ownership shared by completion waiters and the bounded retry cache.
use super::{Arc, CachedDispatch, ClientId, DedupeRegistry, DedupeState, RequestId, rejected};
use rw_types::{
    ClientCommand,
    allocation::{AllocationPlan, PrepareAllocation, PreparedAllocation},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const UNIT: usize = 1024;
const _: () =
    assert!(rw_types::MAX_URGENT_CONTROL_REPLY_RETAINED_BYTES / UNIT <= u32::MAX as usize);
const NORMAL_REPLY_UNITS: u32 = 4 * 1024;

#[derive(Debug)]
pub(super) struct CompletionBudget {
    normal: Arc<Semaphore>,
    urgent: Arc<Semaphore>,
}
impl Default for CompletionBudget {
    fn default() -> Self {
        Self {
            normal: Arc::new(Semaphore::new(64 * 1024)),
            urgent: Arc::new(Semaphore::new(1024)),
        }
    }
}
pub(super) struct CompletionReservation {
    pub(super) bytes: OwnedSemaphorePermit,
    pub(super) failure: Arc<RetainedDispatch>,
    pub(super) closed: Arc<RetainedDispatch>,
    pub(super) oversized: Arc<RetainedDispatch>,
}
impl CompletionBudget {
    pub(super) fn acquire(
        &self,
        command: &ClientCommand,
        ledger: &mut DedupeRegistry,
        key: &(ClientId, RequestId),
    ) -> Option<CompletionReservation> {
        let (pool, units) = if command.is_urgent() {
            (
                &self.urgent,
                u32::try_from(rw_types::MAX_URGENT_CONTROL_REPLY_RETAINED_BYTES / UNIT).ok()?,
            )
        } else {
            (&self.normal, NORMAL_REPLY_UNITS)
        };
        loop {
            if let Ok(mut bytes) = pool.clone().try_acquire_many_owned(units) {
                let failure = error_reply(
                    "control_completion_failed",
                    "control completion failed; effects require host recovery",
                    bytes.split(2)?,
                )?;
                let closed = error_reply(
                    "host_shutting_down",
                    "host control admission is closed",
                    bytes.split(2)?,
                )?;
                let oversized = error_reply(
                    "control_result_limit",
                    "control effects settled but its reply exceeds the retained byte limit; inspect session state before further actions",
                    bytes.split(2)?,
                )?;
                return Some(CompletionReservation {
                    bytes,
                    failure,
                    closed,
                    oversized,
                });
            }
            // A slow waiter retains its own lease after cache eviction.
            let victim = ledger.order.iter().find(|candidate| *candidate != key && matches!(ledger.entries.get(*candidate), Some(DedupeState::Complete { dispatch, .. }) if Arc::ptr_eq(dispatch.bytes.semaphore(), pool))).cloned()?;
            ledger.entries.remove(&victim);
            ledger.order.retain(|candidate| candidate != &victim);
        }
    }
}

#[derive(Debug)]
pub(super) struct RetainedDispatch {
    value: PreparedAllocation<CachedDispatch>,
    bytes: OwnedSemaphorePermit,
}
impl std::ops::Deref for RetainedDispatch {
    type Target = CachedDispatch;
    fn deref(&self) -> &CachedDispatch {
        self.value.value()
    }
}
impl RetainedDispatch {
    pub(super) fn prepare(
        dispatch: CachedDispatch,
        mut bytes: OwnedSemaphorePermit,
    ) -> Option<Arc<Self>> {
        let plan = AllocationPlan::new(dispatch).ok()?;
        if plan.bytes() > bytes.num_permits() * UNIT {
            return None;
        }
        let unused = bytes
            .num_permits()
            .saturating_sub(plan.bytes().div_ceil(UNIT));
        drop(bytes.split(unused));
        Some(Arc::new(Self {
            value: plan.prepare(),
            bytes,
        }))
    }
}
fn error_reply(
    code: &str,
    message: &str,
    bytes: OwnedSemaphorePermit,
) -> Option<Arc<RetainedDispatch>> {
    RetainedDispatch::prepare(
        CachedDispatch {
            outcome: rejected(code, message),
            events: Vec::new(),
            cacheable: true,
        },
        bytes,
    )
}
// Completions are constructed by the host and have no wire deserializer.
impl rw_types::allocation::DecodeAllocation for CachedDispatch {
    fn decode_node_bytes() -> Option<usize> {
        None
    }
}
impl PrepareAllocation for CachedDispatch {
    fn prepared_heap_bytes(&self) -> Option<usize> {
        self.outcome
            .prepared_heap_bytes()?
            .checked_add(self.events.prepared_heap_bytes()?)
    }
    fn prepare_allocations(&mut self) {
        self.outcome.prepare_allocations();
        self.events.prepare_allocations();
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use rw_types::CommandOutcome;

    #[test]
    fn shared_waiter_keeps_bytes_after_cache_eviction() {
        let pool = Arc::new(Semaphore::new(4096));
        let lease = pool
            .clone()
            .try_acquire_many_owned(4096)
            .expect("admission");
        let result = RetainedDispatch::prepare(
            CachedDispatch {
                outcome: CommandOutcome::Accepted {},
                events: Vec::new(),
                cacheable: true,
            },
            lease,
        )
        .expect("bounded reply");
        let waiter = result.clone();
        drop(result);
        assert!(pool.available_permits() < 4096);
        drop(waiter);
        assert_eq!(pool.available_permits(), 4096);
    }

    #[test]
    fn oversized_typed_capacity_is_rejected_without_retention() {
        let pool = Arc::new(Semaphore::new(4));
        let mut code = String::with_capacity(64 * 1024);
        code.push_str("error");
        let mut outcome = rejected("error", "message");
        if let CommandOutcome::Rejected { error } = &mut outcome {
            error.code = code;
        }
        let result = RetainedDispatch::prepare(
            CachedDispatch {
                outcome,
                events: Vec::new(),
                cacheable: true,
            },
            pool.clone().try_acquire_many_owned(4).expect("admission"),
        );
        assert!(result.is_none());
        assert_eq!(pool.available_permits(), 4);
    }
}
