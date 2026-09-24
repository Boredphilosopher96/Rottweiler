//! Parent-side delivery state shared by one child invocation and the calls waiting on it.
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use tokio::sync::watch;

/// How a child's completion reaches its parent.
///
/// Every finished child result is committed to the parent conversation once, at
/// the parent's next provider call. A foreground invocation additionally has a
/// tool call waiting on it; a background invocation wakes an idle parent when it
/// finishes. Moving a child to the background releases its waiting tool calls.
#[derive(Debug)]
pub struct ChildDelivery {
    background: AtomicBool,
    waiters: AtomicUsize,
    detached: watch::Sender<u64>,
}

impl ChildDelivery {
    #[must_use]
    pub fn new(background: bool) -> Arc<Self> {
        Arc::new(Self {
            background: AtomicBool::new(background),
            waiters: AtomicUsize::new(0),
            detached: watch::channel(0).0,
        })
    }

    /// Whether completion should wake an idle parent.
    #[must_use]
    pub fn is_background(&self) -> bool {
        self.background.load(Ordering::Acquire)
    }

    /// Registers a tool call waiting in the foreground until the guard drops.
    #[must_use]
    pub fn waiter(self: &Arc<Self>) -> DeliveryWaiter {
        self.waiters.fetch_add(1, Ordering::AcqRel);
        DeliveryWaiter {
            detached: self.detached.subscribe(),
            delivery: Arc::clone(self),
        }
    }

    /// Releases every waiting tool call and delivers completion in the background.
    /// Returns false when no tool call is waiting.
    pub(super) fn detach(&self) -> bool {
        if self.waiters.load(Ordering::Acquire) == 0 {
            return false;
        }
        self.background.store(true, Ordering::Release);
        self.detached
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        true
    }
}

/// One tool call waiting in the foreground for a child.
#[derive(Debug)]
pub struct DeliveryWaiter {
    delivery: Arc<ChildDelivery>,
    detached: watch::Receiver<u64>,
}

impl DeliveryWaiter {
    /// Resolves when the child is moved to the background.
    pub async fn detached(&mut self) {
        // The sender lives in the shared delivery owned by this guard.
        let _ = self.detached.changed().await;
    }
}

impl Drop for DeliveryWaiter {
    fn drop(&mut self) {
        self.delivery.waiters.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detach_releases_waiters_and_marks_background() {
        let delivery = ChildDelivery::new(false);
        assert!(!delivery.detach(), "nothing waits yet");
        let mut first = delivery.waiter();
        let mut second = delivery.waiter();
        assert!(delivery.detach());
        assert!(delivery.is_background());
        tokio::time::timeout(std::time::Duration::from_secs(1), first.detached())
            .await
            .expect("first waiter released");
        tokio::time::timeout(std::time::Duration::from_secs(1), second.detached())
            .await
            .expect("second waiter released");
        drop((first, second));
        assert!(!delivery.detach(), "released waiters no longer count");
    }
}
