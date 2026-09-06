//! Shared resident bytes with protected capacity for the next canonical query.
use rw_core::{
    AgentLoopError,
    recovery::{HistoryWorkingAllowance, MAX_HISTORY_RESULT_BYTES},
};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, Semaphore};

const UNIT_BYTES: usize = 64 * 1024;
const TOTAL_BYTES: usize = 512 * 1024 * 1024;
const RESIDENT_BYTES: usize = TOTAL_BYTES - MAX_HISTORY_RESULT_BYTES;
const MAX_WAITERS: usize = 64;
const ADMISSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Default)]
struct Usage {
    resident: usize,
    query: usize,
}
struct Pool {
    usage: Mutex<Usage>,
    changed: Notify,
    order: Semaphore,
    waiters: Semaphore,
}
pub(super) struct HistoryRetentions(Arc<Pool>);
pub(crate) struct HistoryRetention {
    pool: Arc<Pool>,
    bytes: usize,
    query: bool,
}
fn exhausted(message: &str) -> AgentLoopError {
    AgentLoopError::Persistence(message.into())
}
fn rounded(bytes: usize) -> Result<usize, AgentLoopError> {
    if bytes > MAX_HISTORY_RESULT_BYTES {
        return Err(exhausted(
            "canonical result exceeded its retained allowance",
        ));
    }
    Ok(bytes.div_ceil(UNIT_BYTES) * UNIT_BYTES)
}
impl HistoryRetentions {
    pub(super) fn new() -> Self {
        Self(Arc::new(Pool {
            usage: Mutex::default(),
            changed: Notify::new(),
            order: Semaphore::new(1),
            waiters: Semaphore::new(MAX_WAITERS),
        }))
    }
    /// The caller grows this owner before allocating its checked work plan.
    pub(super) fn working(&self) -> HistoryRetention {
        HistoryRetention {
            pool: Arc::clone(&self.0),
            bytes: 0,
            query: false,
        }
    }
    /// FIFO admission owns no result bytes or worker while capacity is unavailable.
    pub(super) async fn query(&self) -> Result<HistoryRetention, AgentLoopError> {
        let _waiting = self
            .0
            .waiters
            .try_acquire()
            .map_err(|_| exhausted("canonical query admission queue is full"))?;
        tokio::time::timeout(ADMISSION_TIMEOUT, async {
            let _order = self
                .0
                .order
                .acquire()
                .await
                .map_err(|_| exhausted("canonical query admission is closed"))?;
            loop {
                let changed = self.0.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let mut usage = self
                        .0
                        .usage
                        .lock()
                        .map_err(|_| exhausted("canonical allocation owner poisoned"))?;
                    if usage.resident + usage.query <= TOTAL_BYTES - MAX_HISTORY_RESULT_BYTES {
                        usage.query += MAX_HISTORY_RESULT_BYTES;
                        return Ok(HistoryRetention {
                            pool: Arc::clone(&self.0),
                            bytes: MAX_HISTORY_RESULT_BYTES,
                            query: true,
                        });
                    }
                }
                changed.await;
            }
        })
        .await
        .map_err(|_| exhausted("canonical query admission deadline exceeded"))?
    }
}
impl HistoryRetention {
    /// Transfer a query result into resident ownership, or grow a checked working plan.
    pub(crate) fn resize(&mut self, bytes: usize) -> Result<(), AgentLoopError> {
        let bytes = rounded(bytes)?;
        let mut usage = self
            .pool
            .usage
            .lock()
            .map_err(|_| exhausted("canonical allocation owner poisoned"))?;
        let resident = usage.resident - if self.query { 0 } else { self.bytes };
        let query = usage.query - if self.query { self.bytes } else { 0 };
        if resident + bytes > RESIDENT_BYTES || resident + bytes + query > TOTAL_BYTES {
            return Err(exhausted("retained canonical working allocation exhausted"));
        }
        usage.resident = resident + bytes;
        usage.query = query;
        self.bytes = bytes;
        self.query = false;
        drop(usage);
        self.pool.changed.notify_waiters();
        Ok(())
    }
}
impl HistoryWorkingAllowance for HistoryRetention {
    fn resize(&mut self, bytes: usize) -> Result<(), AgentLoopError> {
        Self::resize(self, bytes)
    }
}
impl Drop for HistoryRetention {
    fn drop(&mut self) {
        let mut usage = self
            .pool
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.query {
            usage.query -= self.bytes;
        } else {
            usage.resident -= self.bytes;
        }
        drop(usage);
        self.pool.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests;
