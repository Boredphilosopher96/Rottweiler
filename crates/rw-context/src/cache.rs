//! Deterministic provider-cache simulator for acceptance tests and metrics.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{AssembledContext, CacheBreakpoint, CacheBreakpointKind};

/// One actually assembled stable prefix and its emitted cache boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CacheObservation {
    /// Injected wall-clock time used for provider TTL simulation.
    pub observed_at_unix_ms: u64,
    /// Exact canonical provider-neutral bytes covered by the boundary.
    pub provider_neutral_prefix_bytes: Vec<u8>,
    pub expected_stable_prefix_turns: usize,
    pub prefix_tokens: u64,
    /// Descriptors produced by assembly. Invalid multiplicity is a cache miss.
    pub breakpoints: Vec<CacheBreakpoint>,
}

impl CacheObservation {
    /// Captures cache-relevant bytes and descriptors from an assembled request.
    #[must_use]
    pub fn from_assembled(assembled: &AssembledContext, observed_at_unix_ms: u64) -> Self {
        Self {
            observed_at_unix_ms,
            provider_neutral_prefix_bytes: assembled.stable_prefix_bytes.clone(),
            expected_stable_prefix_turns: assembled.stable_prefix_turn_count,
            prefix_tokens: assembled.token_totals.stable_prefix,
            breakpoints: assembled.cache_breakpoints.clone(),
        }
    }
}

/// Provider-neutral cache rule families modeled by CI.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CacheRuleProfile {
    /// The adapter must map an explicit stable-prefix breakpoint.
    Explicit {
        minimum_cacheable_tokens: u64,
        ttl_millis: u64,
    },
    /// The provider caches automatically, while assembly still describes the
    /// stable boundary used for byte-level simulation and metrics.
    Automatic {
        minimum_cacheable_tokens: u64,
        ttl_millis: u64,
    },
}

impl CacheRuleProfile {
    const fn minimum_cacheable_tokens(self) -> u64 {
        match self {
            Self::Explicit {
                minimum_cacheable_tokens,
                ..
            }
            | Self::Automatic {
                minimum_cacheable_tokens,
                ..
            } => minimum_cacheable_tokens,
        }
    }

    const fn ttl_millis(self) -> u64 {
        match self {
            Self::Explicit { ttl_millis, .. } | Self::Automatic { ttl_millis, .. } => ttl_millis,
        }
    }

    const fn breakpoint_kind(self) -> CacheBreakpointKind {
        match self {
            Self::Explicit { .. } => CacheBreakpointKind::Explicit,
            Self::Automatic { .. } => CacheBreakpointKind::ProviderManaged,
        }
    }
}

/// Aggregate cache simulation result.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CacheSimulation {
    pub requests: u64,
    pub eligible_requests: u64,
    pub hits: u64,
    pub misses: u64,
    pub invalid_breakpoints: u64,
    pub out_of_order_observations: u64,
    /// Hits divided by all requests in basis points (10,000 = 100%).
    pub hit_rate_basis_points: u16,
}

/// Pure simulator of stable-prefix reuse.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheSimulator;

impl CacheSimulator {
    /// Simulates byte-level cache reuse in the supplied request order.
    #[must_use]
    pub fn simulate(
        observations: &[CacheObservation],
        profile: CacheRuleProfile,
    ) -> CacheSimulation {
        let mut last_seen: HashMap<blake3::Hash, u64> = HashMap::new();
        let mut result = CacheSimulation {
            requests: usize_to_u64(observations.len()),
            ..CacheSimulation::default()
        };
        let mut previous_observation_time = None;

        for observation in observations {
            if previous_observation_time
                .is_some_and(|previous| observation.observed_at_unix_ms < previous)
            {
                result.misses = result.misses.saturating_add(1);
                result.out_of_order_observations =
                    result.out_of_order_observations.saturating_add(1);
                continue;
            }
            previous_observation_time = Some(observation.observed_at_unix_ms);

            if observation.prefix_tokens < profile.minimum_cacheable_tokens() {
                result.misses = result.misses.saturating_add(1);
                continue;
            }
            result.eligible_requests = result.eligible_requests.saturating_add(1);

            let actual_hash = stable_prefix_hash(&observation.provider_neutral_prefix_bytes);
            if !valid_breakpoint(observation, profile, &actual_hash) {
                result.misses = result.misses.saturating_add(1);
                result.invalid_breakpoints = result.invalid_breakpoints.saturating_add(1);
                continue;
            }

            let hit = profile.ttl_millis() > 0
                && last_seen.get(&actual_hash).is_some_and(|previous| {
                    observation.observed_at_unix_ms >= *previous
                        && observation.observed_at_unix_ms.saturating_sub(*previous)
                            <= profile.ttl_millis()
                });
            if hit {
                result.hits = result.hits.saturating_add(1);
            } else {
                result.misses = result.misses.saturating_add(1);
            }
            last_seen.insert(actual_hash, observation.observed_at_unix_ms);
        }
        result.hit_rate_basis_points = basis_points(result.hits, result.requests);
        result
    }
}

fn valid_breakpoint(
    observation: &CacheObservation,
    profile: CacheRuleProfile,
    actual_hash: &blake3::Hash,
) -> bool {
    let [breakpoint] = observation.breakpoints.as_slice() else {
        return false;
    };
    breakpoint.kind == profile.breakpoint_kind()
        && breakpoint.after_turn_count == observation.expected_stable_prefix_turns
        && breakpoint.prefix_tokens == observation.prefix_tokens
        && breakpoint.stable_prefix_hash == actual_hash.to_hex().as_str()
}

fn stable_prefix_hash(bytes: &[u8]) -> blake3::Hash {
    const DOMAIN: &[u8] = b"rottweiler.context.stable-prefix.v1\0";
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN);
    hasher.update(bytes);
    hasher.finalize()
}

fn basis_points(numerator: u64, denominator: u64) -> u16 {
    if denominator == 0 {
        return 0;
    }
    let scaled = u128::from(numerator).saturating_mul(10_000);
    let value = scaled / u128::from(denominator);
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use rw_providers::CacheBreakpointSupport;
    use rw_types::{Block, Role, Turn, TurnMeta};

    use super::{CacheObservation, CacheRuleProfile, CacheSimulator};
    use crate::{
        AssemblyInput, ContextAssembler, ContextItem, ContextItemId, ContextItemKind,
        ContextProvenance,
    };

    fn assembled(support: CacheBreakpointSupport, suffix: &str) -> crate::AssembledContext {
        let stable = ContextItem {
            id: ContextItemId("system".into()),
            kind: ContextItemKind::System,
            label: "system".into(),
            provenance: ContextProvenance::BuiltIn,
            turn: Turn {
                role: Role::System,
                blocks: vec![Block::Text {
                    text: format!("{} {suffix}", "stable cache instructions".repeat(400)),
                }],
                meta: TurnMeta::default(),
            },
            pinned: false,
            evicted: false,
            summarized: false,
            pruned: false,
        };
        ContextAssembler::assemble(AssemblyInput {
            stable_prefix: vec![stable],
            cache_support: support,
            ..AssemblyInput::default()
        })
        .unwrap_or_else(|error| panic!("cache fixture must assemble: {error}"))
    }

    fn explicit_profile() -> CacheRuleProfile {
        CacheRuleProfile::Explicit {
            minimum_cacheable_tokens: 1_024,
            ttl_millis: 300_000,
        }
    }

    #[test]
    fn assembled_steady_state_exceeds_eighty_percent() {
        let observations: Vec<_> = (0..20)
            .map(|turn| {
                CacheObservation::from_assembled(
                    &assembled(CacheBreakpointSupport::Explicit, "same"),
                    turn * 60_000,
                )
            })
            .collect();
        let result = CacheSimulator::simulate(&observations, explicit_profile());
        assert_eq!(result.hits, 19);
        assert_eq!(result.invalid_breakpoints, 0);
        assert!(result.hit_rate_basis_points >= 8_000);
    }

    #[test]
    fn changed_assembled_prefix_and_wall_clock_expiry_are_misses() {
        let observations = vec![
            CacheObservation::from_assembled(&assembled(CacheBreakpointSupport::Explicit, "a"), 1),
            CacheObservation::from_assembled(&assembled(CacheBreakpointSupport::Explicit, "b"), 2),
            CacheObservation::from_assembled(
                &assembled(CacheBreakpointSupport::Explicit, "a"),
                300_002,
            ),
        ];
        let result = CacheSimulator::simulate(&observations, explicit_profile());
        assert_eq!(result.hits, 0);
        assert_eq!(result.misses, 3);
    }

    #[test]
    fn missing_or_wrong_breakpoint_regresses_the_hit_gate() {
        let stable = assembled(CacheBreakpointSupport::Explicit, "same");
        let mut observations: Vec<_> = (0..20)
            .map(|turn| CacheObservation::from_assembled(&stable, turn * 1_000))
            .collect();
        observations[10].breakpoints.clear();
        observations[11].breakpoints[0].after_turn_count = usize::MAX;
        observations[12].breakpoints[0].stable_prefix_hash = "wrong".into();
        let duplicate = observations[13].breakpoints[0].clone();
        observations[13].breakpoints.push(duplicate);

        let result = CacheSimulator::simulate(&observations, explicit_profile());
        assert_eq!(result.invalid_breakpoints, 4);
        assert!(result.hit_rate_basis_points < 8_000);
    }

    #[test]
    fn automatic_profile_requires_provider_managed_descriptor() {
        let automatic = assembled(CacheBreakpointSupport::Automatic, "same");
        let observations = vec![
            CacheObservation::from_assembled(&automatic, 1),
            CacheObservation::from_assembled(&automatic, 2),
        ];
        let result = CacheSimulator::simulate(
            &observations,
            CacheRuleProfile::Automatic {
                minimum_cacheable_tokens: 1_024,
                ttl_millis: 60_000,
            },
        );
        assert_eq!(result.hits, 1);

        let wrong = CacheSimulator::simulate(&observations, explicit_profile());
        assert_eq!(wrong.invalid_breakpoints, 2);
        assert_eq!(wrong.hits, 0);
    }
}
