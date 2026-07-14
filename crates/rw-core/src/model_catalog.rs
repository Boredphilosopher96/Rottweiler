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

    /// Discovers one exact provider without requiring callers to compose or
    /// authenticate unrelated providers. Sources with a provider-aware
    /// boundary should override this method; the default preserves backwards
    /// compatibility for simple and test sources.
    async fn discover_provider(
        &self,
        provider: &str,
    ) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        let mut snapshot = self.discover().await?;
        retain_provider(&mut snapshot, provider);
        Ok(snapshot)
    }
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

    /// Refreshes one provider and merges its live rows into the cached
    /// provider-neutral snapshot. This is used after an authentication flow so
    /// validating one newly connected provider never requires credential
    /// access for every configured provider.
    ///
    /// # Errors
    ///
    /// Returns a sanitized discovery error when the selected provider cannot
    /// be refreshed.
    pub async fn refresh_provider(
        &self,
        provider: &str,
    ) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
        let mut cached = self.snapshot.lock().await;
        let now = self.clock.now();
        let mut update = self.source.discover_provider(provider).await?;
        retain_provider(&mut update, provider);
        let mut snapshot = match cached.as_ref() {
            Some(existing) => merge_provider(existing.snapshot.clone(), update, provider),
            None => update,
        };
        snapshot.cached = false;
        *cached = Some(CachedSnapshot {
            discovered_at: now,
            snapshot: snapshot.clone(),
        });
        Ok(snapshot)
    }
}

fn candidate_provider(candidate: &str) -> Option<&str> {
    candidate.split_once('/').map(|(provider, _)| provider)
}

fn retain_provider(snapshot: &mut ModelCatalogSnapshot, provider: &str) {
    snapshot.providers.retain(|row| row.name == provider);
    snapshot.models.retain(|model| model.provider == provider);
    snapshot.aliases.retain_mut(|alias| {
        alias
            .candidates
            .retain(|candidate| candidate_provider(candidate) == Some(provider));
        !alias.candidates.is_empty()
    });
}

fn merge_provider(
    mut base: ModelCatalogSnapshot,
    mut update: ModelCatalogSnapshot,
    provider: &str,
) -> ModelCatalogSnapshot {
    base.providers.retain(|row| row.name != provider);
    base.providers.append(&mut update.providers);
    base.providers
        .sort_by(|left, right| left.name.cmp(&right.name));

    base.models.retain(|model| model.provider != provider);
    base.models.append(&mut update.models);
    base.models.sort_by(|left, right| left.id.cmp(&right.id));

    for alias in &mut base.aliases {
        alias
            .candidates
            .retain(|candidate| candidate_provider(candidate) != Some(provider));
        if let Some(position) = update
            .aliases
            .iter()
            .position(|updated| updated.alias == alias.alias)
        {
            let updated = update.aliases.remove(position);
            alias.candidates.extend(updated.candidates);
            alias.current |= updated.current;
        }
    }
    base.aliases.retain(|alias| !alias.candidates.is_empty());
    base.aliases.append(&mut update.aliases);
    base.aliases
        .sort_by(|left, right| left.alias.0.cmp(&right.alias.0));
    base.truncated |= update.truncated;
    base
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };

    use rw_providers::Clock;
    use rw_types::{ProviderAuthKind, ProviderDescriptor, ProviderNextAction};

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

    struct ProviderSource {
        full_discoveries: AtomicUsize,
        provider_discoveries: StdMutex<Vec<String>>,
    }

    #[async_trait]
    impl ModelCatalogSource for ProviderSource {
        async fn discover(&self) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
            self.full_discoveries.fetch_add(1, Ordering::SeqCst);
            Ok(snapshot(vec![provider("unrelated", true, 99)]))
        }

        async fn discover_provider(
            &self,
            provider_name: &str,
        ) -> Result<ModelCatalogSnapshot, ModelCatalogError> {
            self.provider_discoveries
                .lock()
                .expect("provider discovery log")
                .push(provider_name.to_owned());
            Ok(snapshot(vec![provider(provider_name, true, 7)]))
        }
    }

    fn provider(name: &str, reachable: bool, model_count: u32) -> ProviderDescriptor {
        ProviderDescriptor {
            name: name.to_owned(),
            auth_kind: ProviderAuthKind::ApiKey,
            next_action: ProviderNextAction::SelectModels,
            configured: true,
            authenticated: reachable,
            reachable,
            model_count,
            status: None,
        }
    }

    fn snapshot(providers: Vec<ProviderDescriptor>) -> ModelCatalogSnapshot {
        ModelCatalogSnapshot {
            aliases: Vec::new(),
            models: Vec::new(),
            providers,
            cached: false,
            truncated: false,
        }
    }

    #[tokio::test]
    async fn provider_refresh_is_scoped_and_preserves_unrelated_cached_rows() {
        let source = Arc::new(ProviderSource {
            full_discoveries: AtomicUsize::new(0),
            provider_discoveries: StdMutex::new(Vec::new()),
        });
        let seed = snapshot(vec![
            provider("anthropic", true, 2),
            provider("github_copilot", false, 0),
        ]);
        let catalog = CachedModelCatalog::with_initial(source.clone(), Some(seed));

        let refreshed = catalog
            .refresh_provider("github_copilot")
            .await
            .expect("provider-specific refresh");

        assert_eq!(source.full_discoveries.load(Ordering::SeqCst), 0);
        assert_eq!(
            *source
                .provider_discoveries
                .lock()
                .expect("provider discovery log"),
            vec!["github_copilot"]
        );
        assert_eq!(refreshed.providers.len(), 2);
        assert_eq!(refreshed.providers[0].name, "anthropic");
        assert_eq!(refreshed.providers[0].model_count, 2);
        assert_eq!(refreshed.providers[1].name, "github_copilot");
        assert_eq!(refreshed.providers[1].model_count, 7);
        assert!(!refreshed.cached);
    }
}
