//! Catalog-published context limits and the one order that resolves them.
//!
//! A lazy hosted session knows its selected model before any provider runtime
//! exists. The provider-neutral catalog (live, or its private durable cache)
//! already carries the resolved window for that model, so context meters,
//! compaction, and the model picker use the same window before and after the
//! runtime is built.
use rw_core::ModelCatalogSnapshot;
use rw_core::ModelContextMetadata;
use rw_providers::CacheBreakpointSupport;
use rw_types::ModelCacheBehavior;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::RwLock;

/// Maximum concrete rows retained; a catalog larger than this keeps its first
/// rows in provider order, which always includes any configured selection.
const MAX_REMEMBERED_MODELS: usize = 4096;

#[derive(Default)]
struct Known {
    models: BTreeMap<String, ModelContextMetadata>,
    aliases: BTreeMap<String, String>,
}

/// Context metadata remembered from provider-neutral catalog snapshots.
pub(in crate::session_runtime) struct CatalogContextLimits {
    known: RwLock<Known>,
    durable_cache: Option<PathBuf>,
    seeded: OnceLock<()>,
}

impl CatalogContextLimits {
    /// `durable_cache` is the private catalog cache that seeds limits once,
    /// on first lookup, so session startup never reads it eagerly.
    pub(in crate::session_runtime) fn new(durable_cache: Option<PathBuf>) -> Self {
        Self {
            known: RwLock::new(Known::default()),
            durable_cache,
            seeded: OnceLock::new(),
        }
    }

    /// Remembers every concrete row with a known window. Newer snapshots
    /// replace older rows; rows absent from a provider-scoped update remain.
    pub(in crate::session_runtime) fn record(&self, snapshot: &ModelCatalogSnapshot) {
        let mut known = self
            .known
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for model in &snapshot.models {
            let Some(max_context_tokens) = model.capabilities.max_context_tokens else {
                continue;
            };
            if known.models.len() >= MAX_REMEMBERED_MODELS && !known.models.contains_key(&model.id)
            {
                continue;
            }
            known.models.insert(
                model.id.clone(),
                ModelContextMetadata {
                    max_context_tokens: Some(max_context_tokens),
                    max_output_tokens: model.capabilities.max_output_tokens,
                    cache_breakpoints: Some(match model.capabilities.cache_behavior {
                        ModelCacheBehavior::None => CacheBreakpointSupport::None,
                        ModelCacheBehavior::Explicit => CacheBreakpointSupport::Explicit,
                        ModelCacheBehavior::ProviderManaged => CacheBreakpointSupport::Automatic,
                    }),
                },
            );
        }
        for alias in &snapshot.aliases {
            if let Some(first) = alias.candidates.first() {
                known.aliases.insert(alias.alias.0.clone(), first.clone());
            }
        }
    }

    /// Resolves a concrete route or a configured alias's primary candidate.
    pub(in crate::session_runtime) fn resolve(&self, alias: &str) -> Option<ModelContextMetadata> {
        self.seed_from_durable_cache();
        let known = self
            .known
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        known.models.get(alias).copied().or_else(|| {
            known
                .aliases
                .get(alias)
                .and_then(|candidate| known.models.get(candidate))
                .copied()
        })
    }

    fn seed_from_durable_cache(&self) {
        self.seeded.get_or_init(|| {
            let Some(path) = &self.durable_cache else {
                return;
            };
            if let Ok(Some(snapshot)) = rw_store::catalog_cache::load_model_catalog_cache(path) {
                self.record(&snapshot);
            }
        });
    }
}

/// The one resolution order for a model's context limits:
///
/// 1. the provider-neutral catalog row, which already prefers the provider's
///    own reported window (live discovery or its private durable cache) over
///    bundled models.dev data;
/// 2. the built runtime's static metadata, used only when no catalog row is
///    known;
/// 3. otherwise the limit is unknown and reported as such.
///
/// The runtime's static composition may only know bundled data for a
/// different namespace (for example the public API window of a subscription
/// route), so it never overrides a catalog row. Cache breakpoint support is a
/// transport property the built runtime knows best.
pub(in crate::session_runtime) fn authoritative(
    catalog: Option<ModelContextMetadata>,
    runtime: ModelContextMetadata,
) -> ModelContextMetadata {
    let Some(catalog) = catalog.filter(|catalog| catalog.max_context_tokens.is_some()) else {
        return runtime;
    };
    ModelContextMetadata {
        max_context_tokens: catalog.max_context_tokens,
        max_output_tokens: catalog.max_output_tokens.or(runtime.max_output_tokens),
        cache_breakpoints: runtime.cache_breakpoints.or(catalog.cache_breakpoints),
    }
}
