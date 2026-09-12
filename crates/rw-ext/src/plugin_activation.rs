//! One bounded control context for cold native readiness, including OS execution.
use rw_tools::CancellationToken;
use std::time::Duration;
use tokio::time::Instant;

/// Shared upper bound for admitted native generation activation.
pub const PLUGIN_ACTIVATION_TIMEOUT: Duration = Duration::from_secs(30);
/// Existing physical retirement allowance after native activation is revoked.
pub const PLUGIN_ACTIVATION_SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(5);

/// The caller's absolute readiness deadline and cancellation authority.
/// Cloning this context never restarts its clock or transfers physical ownership.
#[derive(Clone)]
pub struct PluginActivation {
    cancellation: CancellationToken,
    deadline: Instant,
}

impl PluginActivation {
    #[must_use]
    pub fn new(cancellation: CancellationToken) -> Self {
        Self::until(cancellation, Instant::now() + PLUGIN_ACTIVATION_TIMEOUT)
    }

    #[must_use]
    pub fn until(cancellation: CancellationToken, deadline: Instant) -> Self {
        Self {
            cancellation,
            deadline: deadline.min(Instant::now() + PLUGIN_ACTIVATION_TIMEOUT),
        }
    }

    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    #[must_use]
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled() || Instant::now() >= self.deadline
    }
}
