//! Physical `SQLite` execution observes the caller's cancellation and deadline.
use super::SessionStoreError;
use rusqlite::Connection;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_QUERY_TIME: Duration = Duration::from_secs(2);

/// One read's physical execution fence, shared with its awaiting caller.
#[derive(Clone)]
pub struct SessionIndexReadControl {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
    #[cfg(test)]
    cancel_after_callbacks: Option<usize>,
}
impl Default for SessionIndexReadControl {
    fn default() -> Self {
        Self::new()
    }
}
impl SessionIndexReadControl {
    /// Start the fixed two-second index-read execution budget.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + MAX_QUERY_TIME,
            #[cfg(test)]
            cancel_after_callbacks: None,
        }
    }
    /// Cancellation remains observable even when no SQL statement is active yet.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || Instant::now() >= self.deadline
    }
    /// Check cancellation and remaining time before another bounded operation.
    /// # Errors
    /// Returns an interruption when cancelled or expired.
    pub fn check(&self) -> Result<(), SessionStoreError> {
        if self.stopped() {
            Err(SessionStoreError::IndexReadInterrupted)
        } else {
            Ok(())
        }
    }
    pub(super) fn install(&self, connection: &Connection) -> Result<(), SessionStoreError> {
        self.check()?;
        connection.busy_timeout(
            self.deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50)),
        )?;
        let control = self.clone();
        #[cfg(test)]
        let mut callbacks = 0;
        connection.progress_handler(
            1000,
            Some(move || {
                #[cfg(test)]
                {
                    callbacks += 1;
                    if control.cancel_after_callbacks == Some(callbacks) {
                        control.cancel();
                    }
                }
                control.stopped()
            }),
        )?;
        self.check()
    }
}

#[cfg(test)]
mod tests;
