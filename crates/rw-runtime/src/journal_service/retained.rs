//! Application-wide byte ownership for delivered canonical read results.
use rw_core::{AgentLoopError, recovery::MAX_HISTORY_RESULT_BYTES};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const UNIT_BYTES: usize = 64 * 1024;
const TOTAL_BYTES: usize = 512 * 1024 * 1024;

pub(super) struct HistoryRetentions(Arc<Semaphore>);
pub(crate) struct HistoryRetention(OwnedSemaphorePermit);
impl HistoryRetentions {
    pub(super) fn new() -> Self {
        Self(Arc::new(Semaphore::new(TOTAL_BYTES / UNIT_BYTES)))
    }
    pub(super) fn admit(&self) -> Result<HistoryRetention, AgentLoopError> {
        Arc::clone(&self.0)
            .try_acquire_many_owned((MAX_HISTORY_RESULT_BYTES / UNIT_BYTES) as u32)
            .map(HistoryRetention)
            .map_err(|_| {
                AgentLoopError::Persistence("retained canonical read admission exhausted".into())
            })
    }
}
impl HistoryRetention {
    pub(crate) fn resize(&mut self, bytes: usize) -> Result<(), AgentLoopError> {
        let required = bytes.div_ceil(UNIT_BYTES).max(1);
        let released = self.0.num_permits().checked_sub(required).ok_or_else(|| {
            AgentLoopError::Persistence("canonical result exceeded its retained allowance".into())
        })?;
        drop(self.0.split(released));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::{HistoryRetentions, MAX_HISTORY_RESULT_BYTES, TOTAL_BYTES};

    #[test]
    fn delivered_small_results_release_worker_sized_reservations_but_keep_bytes() {
        let budget = HistoryRetentions::new();
        let mut delivered = Vec::new();
        for _ in 0..32 {
            let mut owner = budget.admit().expect("new read can start");
            owner.resize(1024).expect("small retained result");
            delivered.push(owner);
        }
        let mut active = Vec::new();
        for _ in 0..(TOTAL_BYTES / MAX_HISTORY_RESULT_BYTES - 1) {
            active.push(budget.admit().expect("remaining worker reservation"));
        }
        assert!(budget.admit().is_err());
        drop(delivered);
        assert!(budget.admit().is_ok());
        drop(active);
        assert!(budget.admit().is_ok());
    }
}
