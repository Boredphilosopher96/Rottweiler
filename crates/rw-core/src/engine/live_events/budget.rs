use super::AgentLoopError;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

pub(super) const MAX_REPLAY_BYTES: usize = (2 * 64 + 16) * 1024 * 1024 + 16 * 1024;
static LIVE: OnceLock<Arc<Budget>> = OnceLock::new();
static REPLAY: OnceLock<Arc<Budget>> = OnceLock::new();
static SUBSCRIPTIONS: OnceLock<Arc<Budget>> = OnceLock::new();

#[derive(Debug)]
pub(super) struct Budget {
    used: AtomicUsize,
    limit: usize,
    waiters: AtomicUsize,
    changed: tokio::sync::Notify,
}
impl Budget {
    pub(super) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicUsize::new(0),
            limit,
            waiters: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
        })
    }
    pub(super) fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Credit, AgentLoopError> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= self.limit)
            })
            .map_err(|_| AgentLoopError::EventDeliverySaturated)?;
        Ok(Credit {
            budget: Arc::clone(self),
            bytes,
        })
    }
    #[cfg(test)]
    pub(super) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}
#[derive(Debug)]
pub(in crate::engine) struct Credit {
    budget: Arc<Budget>,
    bytes: usize,
}
impl Credit {
    pub(super) fn shrink(&mut self, bytes: usize) -> Result<(), AgentLoopError> {
        if bytes > self.bytes {
            return Err(AgentLoopError::EventDeliverySaturated);
        }
        self.budget
            .used
            .fetch_sub(self.bytes - bytes, Ordering::AcqRel);
        self.bytes = bytes;
        self.budget.changed.notify_waiters();
        Ok(())
    }
}
impl Drop for Credit {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
        self.budget.changed.notify_waiters();
    }
}
pub(super) fn live() -> Arc<Budget> {
    Arc::clone(LIVE.get_or_init(|| Budget::new(128 * 1024 * 1024)))
}
// Four worst-case decoded + original JSON + normalized-copy windows, plus
// page descriptor storage. Completed reads shrink to their measured prepared
// page bytes. Admission waits outside actors; held results can never wait on
// the journal writer's commit credits.
pub(super) async fn replay() -> Result<Credit, AgentLoopError> {
    let budget = REPLAY.get_or_init(|| Budget::new(4 * MAX_REPLAY_BYTES));
    if let Ok(credit) = budget.reserve(MAX_REPLAY_BYTES) {
        return Ok(credit);
    }
    budget
        .waiters
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < 64).then_some(count + 1)
        })
        .map_err(|_| AgentLoopError::EventDeliverySaturated)?;
    let _waiter = Waiter(budget);
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let changed = budget.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Ok(credit) = budget.reserve(MAX_REPLAY_BYTES) {
                return credit;
            }
            changed.await;
        }
    })
    .await
    .map_err(|_| AgentLoopError::EventDeliverySaturated)
}
struct Waiter<'a>(&'a Budget);
impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        self.0.waiters.fetch_sub(1, Ordering::AcqRel);
    }
}
pub(super) fn subscription() -> Result<Credit, AgentLoopError> {
    SUBSCRIPTIONS.get_or_init(|| Budget::new(512)).reserve(1)
}
