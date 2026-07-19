use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::time::Instant;

use crate::ProviderError;

/// Injectable delay for deterministic retry tests.
pub trait Delay: Send + Sync {
    /// Sleeps without tying retry policy to wall-clock time.
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Injectable monotonic clock used for provider health cooldowns.
pub trait Clock: Send + Sync {
    /// Returns the current monotonic instant.
    fn now(&self) -> Instant;
}

/// Injectable source for a uniformly distributed jitter sample in `[0, 1)`.
///
/// Keeping randomness outside [`RetryPolicy`] makes retry timing reproducible
/// in tests and replay while production routers still avoid synchronized retry
/// storms.
pub trait JitterSource: Send + Sync {
    /// Returns the next unit-interval sample.
    fn sample_unit(&self) -> f64;
}

/// Deterministic seeded jitter source for tests and reproducible runtimes.
#[derive(Debug)]
pub struct SeededJitter {
    state: AtomicU64,
}

impl SeededJitter {
    /// Creates a deterministic xorshift sequence. A zero seed is mapped to a
    /// fixed non-zero state rather than consulting ambient randomness.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        let seed = if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        };
        Self {
            state: AtomicU64::new(seed),
        }
    }
}

impl JitterSource for SeededJitter {
    fn sample_unit(&self) -> f64 {
        unit_sample(next_xorshift(&self.state))
    }
}

/// Process-local nondeterministic jitter used by production routers.
#[derive(Debug)]
pub struct ProductionJitter(SeededJitter);

impl Default for ProductionJitter {
    fn default() -> Self {
        static SEED_NONCE: AtomicU64 = AtomicU64::new(1);
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let seed = elapsed.as_secs()
            ^ u64::from(elapsed.subsec_nanos()).rotate_left(32)
            ^ u64::from(std::process::id()).rotate_left(17)
            ^ SEED_NONCE.fetch_add(1, Ordering::Relaxed);
        Self(SeededJitter::new(seed))
    }
}

impl JitterSource for ProductionJitter {
    fn sample_unit(&self) -> f64 {
        self.0.sample_unit()
    }
}

fn next_xorshift(state: &AtomicU64) -> u64 {
    let mut current = state.load(Ordering::Relaxed);
    loop {
        let mut next = current;
        next ^= next << 13;
        next ^= next >> 7;
        next ^= next << 17;
        if next == 0 {
            next = 0x9e37_79b9_7f4a_7c15;
        }
        match state.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(observed) => current = observed,
        }
    }
}

fn unit_sample(value: u64) -> f64 {
    let upper = u32::try_from(value >> 32).unwrap_or_default();
    f64::from(upper) / 4_294_967_296.0
}

/// Production monotonic clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioClock;

impl Clock for TokioClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Tokio-backed production delay.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioDelay;

impl Delay for TokioDelay {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// Bounded exponential retry policy. The policy bounds jitter but receives the
/// actual sample from an injected [`JitterSource`].
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Total attempts including the initial call.
    pub max_attempts: usize,
    /// First backoff delay.
    pub base_delay: Duration,
    /// Backoff ceiling.
    pub max_delay: Duration,
    /// Additive jitter fraction in `[0, 1]`.
    pub jitter_fraction: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(5),
            jitter_fraction: 0.2,
        }
    }
}

impl RetryPolicy {
    /// Computes the next delay, honoring `Retry-After` when present.
    #[must_use]
    pub fn delay_for(
        &self,
        retry_index: usize,
        error: &ProviderError,
        jitter_unit: f64,
    ) -> Duration {
        if let Some(milliseconds) = error.retry_after_ms {
            return Duration::from_millis(milliseconds).min(Duration::from_secs(120));
        }
        let exponent = u32::try_from(retry_index.min(31)).unwrap_or(31);
        let factor = 2_u32.saturating_pow(exponent);
        let base = self.base_delay.saturating_mul(factor).min(self.max_delay);
        let clamped_jitter = self.jitter_fraction.clamp(0.0, 1.0);
        let sample = if jitter_unit.is_finite() {
            jitter_unit.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let jitter = base.mul_f64(clamped_jitter * sample);
        base.saturating_add(jitter).min(self.max_delay)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, time::Duration};

    use crate::{ProviderError, ProviderErrorKind};

    use super::{JitterSource, RetryPolicy, SeededJitter};

    #[test]
    fn retry_after_is_honored_separately_from_backoff_cap() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(3),
            jitter_fraction: 0.0,
        };
        let throttled = ProviderError::new(ProviderErrorKind::RateLimited, "slow down")
            .with_retry_after(60_000);
        assert_eq!(
            policy.delay_for(0, &throttled, 0.75),
            Duration::from_secs(60)
        );
        let excessive = ProviderError::new(ProviderErrorKind::RateLimited, "slow down")
            .with_retry_after(121_000);
        assert_eq!(
            policy.delay_for(0, &excessive, 0.75),
            Duration::from_secs(120)
        );
        let server = ProviderError::new(ProviderErrorKind::Server, "unavailable");
        assert_eq!(policy.delay_for(8, &server, 0.75), Duration::from_secs(3));
    }

    #[test]
    fn seeded_jitter_is_reproducible_distinct_and_bounded() {
        let first = SeededJitter::new(42);
        let second = SeededJitter::new(42);
        let policy = RetryPolicy {
            max_attempts: 6,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(2),
            jitter_fraction: 0.5,
        };
        let error = ProviderError::new(ProviderErrorKind::Server, "unavailable");
        let first_delays = (0..6)
            .map(|_| policy.delay_for(0, &error, first.sample_unit()))
            .collect::<Vec<_>>();
        let second_delays = (0..6)
            .map(|_| policy.delay_for(0, &error, second.sample_unit()))
            .collect::<Vec<_>>();
        assert_eq!(first_delays, second_delays);
        assert!(first_delays.iter().all(
            |delay| *delay >= Duration::from_secs(1) && *delay <= Duration::from_millis(1_500)
        ));
        assert!(first_delays.into_iter().collect::<BTreeSet<_>>().len() > 1);
    }

    #[test]
    fn only_transient_error_classes_are_retryable() {
        for kind in [
            ProviderErrorKind::RateLimited,
            ProviderErrorKind::Timeout,
            ProviderErrorKind::Server,
            ProviderErrorKind::Network,
        ] {
            assert!(ProviderError::new(kind, "transient fixture").is_retryable());
        }
        for kind in [
            ProviderErrorKind::Authentication,
            ProviderErrorKind::InvalidRequest,
            ProviderErrorKind::Protocol,
            ProviderErrorKind::Unsupported,
        ] {
            assert!(!ProviderError::new(kind, "permanent fixture").is_retryable());
        }
    }
}
