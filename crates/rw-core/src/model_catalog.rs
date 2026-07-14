//! Briefly cached provider-neutral live model catalog.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use rw_providers::{Clock, TokioClock};
use rw_types::ModelCatalogSnapshot;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// Sanitized failure of the catalog composition boundary.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("model catalog discovery failed: {0}")]
pub struct ModelCatalogError(pub String);

/// Injectable live discovery source. Implementations keep provider auth,
/// endpoints, and wire formats behind `rw-providers`.
#[async_trait]
pub trait ModelCatalogSource: Send + Sync + 'static {
    async fn discover(&self) -> Result<ModelCatalogSnapshot, ModelCatalogError>;
}

struct CachedSnapshot {
    discovered_at: Instant,
    snapshot: ModelCatalogSnapshot,
}

/// Short process-local cache with deterministic injected monotonic time.
pub struct CachedModelCatalog {
    source: Arc<dyn ModelCatalogSource>,
    clock: Arc<dyn Clock>,
    ttl: Duration,
    snapshot: Mutex<Option<CachedSnapshot>>,
}

impl CachedModelCatalog {
    pub const DEFAULT_TTL: Duration = Duration::from_secs(30);

    #[must_use]
    pub fn new(source: Arc<dyn ModelCatalogSource>) -> Self {
        Self::with_clock(source, Arc::new(TokioClock), Self::DEFAULT_TTL)
    }

    /// Seeds the short process cache from a private durable cache. The seed is
    /// never authoritative: explicit refresh still contacts the live source.
    #[must_use]
    pub fn with_initial(
        source: Arc<dyn ModelCatalogSource>,
        snapshot: Option<ModelCatalogSnapshot>,
    ) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(TokioClock);
        Self::with_clock_and_initial(source, clock, Self::DEFAULT_TTL, snapshot)
    }

    #[must_use]
    pub fn with_clock(
        source: Arc<dyn ModelCatalogSource>,
        clock: Arc<dyn Clock>,
        ttl: Duration,
    ) -> Self {
        Self::with_clock_and_initial(source, clock, ttl, None)
    }

    fn with_clock_and_initial(
        source: Arc<dyn ModelCatalogSource>,
        clock: Arc<dyn Clock>,
        ttl: Duration,
        snapshot: Option<ModelCatalogSnapshot>,
    ) -> Self {
        let discovered_at = clock.now();
        Self {
            source,
            clock,
            ttl,
            snapshot: Mutex::new(snapshot.map(|snapshot| CachedSnapshot {
                discovered_at,
                snapshot,
            })),
        }
    }

    /// Returns a cached snapshot or performs one serialized refresh.
    ///
    /// # Errors
    ///
    /// Returns only composition-wide failures. Individual provider failures
    /// remain visible as provider/model status rows in the snapshot.
    pub async fn get(&self, refresh: bool) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        let mut cached = self.snapshot.lock().await;
        let now = self.clock.now();
        if !refresh
            && let Some(existing) = cached.as_ref()
            && now.duration_since(existing.discovered_at) < self.ttl
        {
            let mut snapshot = existing.snapshot.clone();
            snapshot.cached = true;
            return Ok(snapshot);
        }
        let mut snapshot = self.source.discover().await?;
        snapshot.cached = false;
        *cached = Some(CachedSnapshot {
            discovered_at: now,
            snapshot: snapshot.clone(),
        });
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::sync::atomic::{AtomicUsize, Ordering};

    use rw_providers::Clock;

    use super::*;

    struct FixedClock(Instant);

    impl Clock for FixedClock {
        fn now(&self) -> Instant {
            self.0
        }
    }

    struct Source(AtomicUsize);

    #[async_trait]
    impl ModelCatalogSource for Source {
        async fn discover(&self) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ModelCatalogSnapshot {
                aliases: Vec::new(),
                models: Vec::new(),
                providers: Vec::new(),
                cached: false,
                truncated: false,
            })
        }
    }

    #[tokio::test]
    async fn cache_is_short_refreshable_and_source_injectable() {
        let source = Arc::new(Source(AtomicUsize::new(0)));
        let clock = Arc::new(FixedClock(Instant::now()));
        let catalog =
            CachedModelCatalog::with_clock(source.clone(), clock, Duration::from_secs(30));
        assert!(!catalog.get(false).await.expect("first").cached);
        assert!(catalog.get(false).await.expect("cached").cached);
        assert!(!catalog.get(true).await.expect("refresh").cached);
        assert_eq!(source.0.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn durable_seed_avoids_launch_discovery_but_refresh_remains_live() {
        let source = Arc::new(Source(AtomicUsize::new(0)));
        let seed = ModelCatalogSnapshot {
            aliases: Vec::new(),
            models: Vec::new(),
            providers: Vec::new(),
            cached: false,
            truncated: true,
        };
        let catalog = CachedModelCatalog::with_initial(source.clone(), Some(seed));

        let initial = catalog.get(false).await.expect("seeded cache");
        assert!(initial.cached);
        assert!(initial.truncated);
        assert_eq!(source.0.load(Ordering::SeqCst), 0);

        assert!(!catalog.get(true).await.expect("explicit refresh").cached);
        assert_eq!(source.0.load(Ordering::SeqCst), 1);
    }
}
