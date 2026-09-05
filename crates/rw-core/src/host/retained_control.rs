//! Byte ownership shared by completion waiters and the bounded retry cache.
use super::{Arc, CachedDispatch, ClientId, DedupeRegistry, DedupeState, RequestId, rejected};
use rw_types::{
    ClientCommand,
    allocation::{AllocationPlan, PrepareAllocation, PreparedAllocation},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const UNIT: usize = 1024;
const NORMAL_REPLY_UNITS: u32 = 4 * 1024;
const URGENT_REPLY_UNITS: u32 = 64;

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
impl CompletionBudget {
    pub(super) fn acquire(
        &self,
        command: &ClientCommand,
        ledger: &mut DedupeRegistry,
        key: &(ClientId, RequestId),
    ) -> Option<OwnedSemaphorePermit> {
        let (pool, units) = if super::control_admission::is_urgent(command) {
            (&self.urgent, URGENT_REPLY_UNITS)
        } else {
            (&self.normal, NORMAL_REPLY_UNITS)
        };
        loop {
            if let Ok(lease) = pool.clone().try_acquire_many_owned(units) {
                return Some(lease);
            }
            // Eviction releases only the cache's reference. A slow waiter retains
            // its own lease, so this cannot manufacture capacity it still owns.
            let victim = ledger
                .order
                .iter()
                .find(|candidate| {
                    *candidate != key
                        && matches!(
                            ledger.entries.get(*candidate),
                            Some(DedupeState::Complete { dispatch, .. }) if Arc::ptr_eq(dispatch._bytes.semaphore(), pool)
                        )
                })
                .cloned()?;
            ledger.entries.remove(&victim);
            ledger.order.retain(|candidate| candidate != &victim);
        }
    }
}

#[derive(Debug)]
pub(super) struct RetainedDispatch {
    value: PreparedAllocation<CachedDispatch>,
    _bytes: OwnedSemaphorePermit,
}
impl std::ops::Deref for RetainedDispatch {
    type Target = CachedDispatch;
    fn deref(&self) -> &CachedDispatch {
        self.value.value()
    }
}
impl RetainedDispatch {
    pub(super) fn prepare(dispatch: CachedDispatch, mut bytes: OwnedSemaphorePermit) -> Arc<Self> {
        let capacity = bytes.num_permits() * UNIT;
        let plan = match AllocationPlan::new(dispatch) {
            Ok(plan) if plan.bytes() <= capacity => plan,
            _ => AllocationPlan::new(CachedDispatch {
                outcome: rejected("control_result_limit", "control effects settled but its reply exceeds the retained byte limit; inspect session state before further actions"),
                events: Vec::new(), cacheable: true,
            }).expect("bounded completion error"),
        };
        let unused = bytes
            .num_permits()
            .saturating_sub(plan.bytes().div_ceil(UNIT));
        drop(bytes.split(unused));
        Arc::new(Self {
            value: plan.prepare(),
            _bytes: bytes,
        })
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
mod tests {
    use super::*;
    use rw_types::CommandOutcome;

    #[test]
    fn shared_waiter_keeps_bytes_after_cache_eviction() {
        let pool = Arc::new(Semaphore::new(4096));
        let lease = pool.clone().try_acquire_many_owned(4096).unwrap();
        let result = RetainedDispatch::prepare(
            CachedDispatch {
                outcome: CommandOutcome::Accepted {},
                events: Vec::new(),
                cacheable: true,
            },
            lease,
        );
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
        if let rw_types::CommandOutcome::Rejected { error } = &mut outcome {
            error.code = code;
        }
        let result = RetainedDispatch::prepare(
            CachedDispatch {
                outcome,
                events: Vec::new(),
                cacheable: true,
            },
            pool.clone().try_acquire_many_owned(4).unwrap(),
        );
        assert!(
            matches!(&result.outcome, CommandOutcome::Rejected { error } if error.code == "control_result_limit")
        );
        assert!(result.value.bytes() < 4 * UNIT);
        drop(result);
        assert_eq!(pool.available_permits(), 4);
    }
}
