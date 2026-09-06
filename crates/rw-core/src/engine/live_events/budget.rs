use super::AgentLoopError;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

// Source JSON is released before normalization starts: the maximum of
// decoded+source and original+normalized storage, not their sum.
pub(super) const MAX_REPLAY_BYTES: usize = {
    let read = rw_store::session::journal::MAX_JOURNAL_DECODE_BYTES
        + rw_store::session::journal::MAX_JOURNAL_APPEND_BYTES;
    let prepare = 2 * rw_store::session::journal::MAX_JOURNAL_DECODE_BYTES;
    (if read > prepare { read } else { prepare }) + 16 * 1024
};
static LIVE: OnceLock<Arc<Budget>> = OnceLock::new();
static REPLAY: OnceLock<ReplayPool> = OnceLock::new();
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
/// Construction owns the FIFO permit through physical read and normalization.
/// Prepared payloads keep only byte credit after construction finishes.
pub(in crate::engine) struct ReplayAdmission {
    pub(super) credit: Credit,
    pub(super) construction: tokio::sync::OwnedSemaphorePermit,
}
struct ReplayPool {
    bytes: Arc<Budget>,
    construction: Arc<tokio::sync::Semaphore>,
    waiters: Arc<Budget>,
}
impl ReplayPool {
    fn new(bytes: usize) -> Self {
        Self {
            bytes: Budget::new(bytes),
            construction: Arc::new(tokio::sync::Semaphore::new(1)),
            waiters: Budget::new(64),
        }
    }
    async fn admit(
        &self,
        bytes: usize,
        deadline: std::time::Duration,
    ) -> Result<ReplayAdmission, AgentLoopError> {
        let _waiting = self.waiters.reserve(1)?;
        tokio::time::timeout(deadline, async {
            // FIFO construction admission prevents fresh readers racing ahead
            // of queued readers whenever a delivered page releases its bytes.
            let construction = self
                .construction
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| AgentLoopError::EventDeliverySaturated)?;
            let credit = self.bytes.reserve_wait(bytes, deadline).await?;
            Ok(ReplayAdmission {
                credit,
                construction,
            })
        })
        .await
        .map_err(|_| AgentLoopError::EventDeliverySaturated)?
    }
}
pub(super) async fn replay() -> Result<ReplayAdmission, AgentLoopError> {
    REPLAY
        .get_or_init(|| ReplayPool::new(2 * MAX_REPLAY_BYTES))
        .admit(MAX_REPLAY_BYTES, std::time::Duration::from_secs(30))
        .await
}
impl Budget {
    async fn reserve_wait(
        self: &Arc<Self>,
        bytes: usize,
        deadline: std::time::Duration,
    ) -> Result<Credit, AgentLoopError> {
        if let Ok(credit) = self.reserve(bytes) {
            return Ok(credit);
        }
        self.waiters
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 64).then_some(count + 1)
            })
            .map_err(|_| AgentLoopError::EventDeliverySaturated)?;
        let _waiter = Waiter(self);
        tokio::time::timeout(deadline, async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if let Ok(credit) = self.reserve(bytes) {
                    return credit;
                }
                changed.await;
            }
        })
        .await
        .map_err(|_| AgentLoopError::EventDeliverySaturated)
    }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_or_expired_admission_refunds_waiter_without_allocating_a_result() {
        let budget = Budget::new(1024);
        let held = budget.reserve(1024).expect("retain full result");
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                budget.reserve_wait(1024, Duration::from_secs(30))
            )
            .await
            .is_err()
        );
        assert_eq!(budget.waiters.load(Ordering::Acquire), 0);
        assert_eq!(budget.used(), 1024);
        assert!(matches!(
            budget.reserve_wait(1024, Duration::from_millis(10)).await,
            Err(AgentLoopError::EventDeliverySaturated)
        ));
        assert_eq!(budget.waiters.load(Ordering::Acquire), 0);
        drop(held);
        assert!(
            budget
                .reserve_wait(1024, Duration::from_millis(10))
                .await
                .is_ok()
        );
        assert_eq!(budget.used(), 0);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod replay_tests {
    use super::{MAX_REPLAY_BYTES, ReplayPool};
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn maximum_page_results_keep_credit_while_next_reader_uses_fifo_construction() {
        let pool = ReplayPool::new(2 * MAX_REPLAY_BYTES);
        let mut first = pool
            .admit(MAX_REPLAY_BYTES, Duration::from_secs(30))
            .await
            .expect("maximum read");
        first
            .credit
            .shrink(rw_store::session::journal::MAX_JOURNAL_DECODE_BYTES)
            .expect("maximum prepared result");
        let next = pool.admit(MAX_REPLAY_BYTES, Duration::from_secs(30));
        tokio::pin!(next);
        assert!(
            tokio::time::timeout(Duration::from_millis(1), &mut next)
                .await
                .is_err()
        );
        assert_eq!(pool.waiters.used(), 1);
        let later = pool.admit(MAX_REPLAY_BYTES, Duration::from_secs(30));
        tokio::pin!(later);
        assert!(
            tokio::time::timeout(Duration::from_millis(1), &mut later)
                .await
                .is_err()
        );
        let super::ReplayAdmission {
            credit: retained,
            construction,
        } = first;
        drop(construction);
        let second = next
            .await
            .expect("FIFO next read can admit maximum alongside retained result");
        assert_eq!(
            pool.bytes.used(),
            MAX_REPLAY_BYTES + rw_store::session::journal::MAX_JOURNAL_DECODE_BYTES
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(1), &mut later)
                .await
                .is_err()
        );
        drop(second);
        let third = later.await.expect("next queued construction");
        drop(third);
        assert_eq!(
            pool.bytes.used(),
            rw_store::session::journal::MAX_JOURNAL_DECODE_BYTES
        );
        drop(retained);
        assert_eq!(pool.bytes.used(), 0);
        assert_eq!(pool.waiters.used(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn held_results_expire_admission_and_cancelled_waits_refund_all_queue_owners() {
        let pool = ReplayPool::new(128);
        let held = pool
            .bytes
            .reserve(100)
            .expect("caller retains delivered guards");
        let waiting = pool.admit(64, Duration::from_secs(30));
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(1), &mut waiting)
                .await
                .is_err()
        );
        assert_eq!(pool.construction.available_permits(), 0);
        // A canceled FIFO waiter neither enters a source reader nor holds bytes.
        assert!(
            tokio::time::timeout(
                Duration::from_millis(1),
                pool.admit(64, Duration::from_secs(30))
            )
            .await
            .is_err()
        );
        assert_eq!(pool.waiters.used(), 1);
        assert!(waiting.await.is_err());
        assert_eq!(pool.construction.available_permits(), 1);
        assert_eq!(pool.waiters.used(), 0);
        assert_eq!(pool.bytes.used(), 100);
        drop(held);
        drop(
            pool.admit(64, Duration::from_secs(30))
                .await
                .expect("read after caller releases guards"),
        );
        assert_eq!(pool.bytes.used(), 0);
    }
}
