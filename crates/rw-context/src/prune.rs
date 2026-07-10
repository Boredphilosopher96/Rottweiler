//! Deterministic ADR-010 tool-output pruning.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Replacement persisted in place of a pruned tool result.
pub const PRUNED_TOOL_OUTPUT_REPLACEMENT: &str = "[Old tool result content cleared]";

/// Default number of newest user turns left completely untouched.
pub const DEFAULT_RECENT_USER_TURNS: usize = 2;
/// Default completed tool-output protection window.
pub const DEFAULT_PROTECTED_TOOL_TOKENS: u64 = 40_000;
/// Pruning runs only when reclaimable tokens are strictly greater than this.
pub const DEFAULT_MINIMUM_RECLAIM_TOKENS: u64 = 20_000;

/// Transcript record kind needed by the pure pruning planner.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PruneRecordKind {
    User,
    ToolOutput {
        tool_call_id: String,
        tool_name: String,
        completed: bool,
    },
    /// A previous compaction boundary; nothing before it is reconsidered.
    SummaryMarker,
    /// A previous prune/eviction boundary; nothing before it is reconsidered.
    PrunedMarker,
    Other,
}

/// Minimal normalized transcript record consumed by [`Pruner`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PruneRecord {
    pub item_id: String,
    pub transcript_index: usize,
    pub kind: PruneRecordKind,
    pub tokens: u64,
    /// Pins are additive to the protected-tool contract.
    pub pinned: bool,
}

/// Configurable constants and always-protected tool names.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PruneConfig {
    pub recent_user_turns: usize,
    pub protected_tool_tokens: u64,
    pub minimum_reclaim_tokens: u64,
    pub protected_tools: BTreeSet<String>,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            recent_user_turns: DEFAULT_RECENT_USER_TURNS,
            protected_tool_tokens: DEFAULT_PROTECTED_TOOL_TOKENS,
            minimum_reclaim_tokens: DEFAULT_MINIMUM_RECLAIM_TOKENS,
            protected_tools: BTreeSet::from(["skill".to_owned()]),
        }
    }
}

/// One tool output the caller should replace and persist as an event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PruneDecision {
    pub item_id: String,
    pub transcript_index: usize,
    pub tool_call_id: String,
    pub tool_name: String,
    pub original_tokens: u64,
    pub replacement: String,
}

/// Why a backward walk terminated.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PruneStopReason {
    StartOfTranscript,
    SummaryMarker,
    PrunedMarker,
}

/// Complete, auditable output from a pruning pass.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PrunePlan {
    pub decisions: Vec<PruneDecision>,
    /// Candidate total even when it does not cross the strict threshold.
    pub eligible_reclaim_tokens: u64,
    /// Zero when the strict threshold was not crossed.
    pub reclaimed_tokens: u64,
    pub protected_window_tokens: u64,
    pub stop_reason: PruneStopReason,
}

/// Pure ADR-010 backward pruning planner.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Pruner;

impl Pruner {
    /// Builds a deterministic plan without mutating conversation state.
    #[must_use]
    pub fn plan(records: &[PruneRecord], config: &PruneConfig) -> PrunePlan {
        let mut newest_users_seen = 0_usize;
        let mut protected_window_tokens = 0_u64;
        let mut candidates = Vec::new();
        let mut eligible_reclaim_tokens = 0_u64;
        let mut stop_reason = PruneStopReason::StartOfTranscript;

        for record in records.iter().rev() {
            match &record.kind {
                PruneRecordKind::SummaryMarker => {
                    stop_reason = PruneStopReason::SummaryMarker;
                    break;
                }
                PruneRecordKind::PrunedMarker => {
                    stop_reason = PruneStopReason::PrunedMarker;
                    break;
                }
                PruneRecordKind::User => {
                    newest_users_seen = newest_users_seen.saturating_add(1);
                    continue;
                }
                PruneRecordKind::ToolOutput { .. } | PruneRecordKind::Other => {}
            }

            if newest_users_seen < config.recent_user_turns {
                continue;
            }

            let PruneRecordKind::ToolOutput {
                tool_call_id,
                tool_name,
                completed,
            } = &record.kind
            else {
                continue;
            };
            if !completed {
                continue;
            }

            if record.pinned || config.protected_tools.contains(tool_name) {
                continue;
            }
            if protected_window_tokens < config.protected_tool_tokens {
                protected_window_tokens = protected_window_tokens.saturating_add(record.tokens);
                continue;
            }

            eligible_reclaim_tokens = eligible_reclaim_tokens.saturating_add(record.tokens);
            candidates.push(PruneDecision {
                item_id: record.item_id.clone(),
                transcript_index: record.transcript_index,
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                original_tokens: record.tokens,
                replacement: PRUNED_TOOL_OUTPUT_REPLACEMENT.to_owned(),
            });
        }

        let crossed_threshold = eligible_reclaim_tokens > config.minimum_reclaim_tokens;
        if crossed_threshold {
            candidates.sort_unstable_by_key(|decision| decision.transcript_index);
        } else {
            candidates.clear();
        }
        PrunePlan {
            decisions: candidates,
            eligible_reclaim_tokens,
            reclaimed_tokens: if crossed_threshold {
                eligible_reclaim_tokens
            } else {
                0
            },
            protected_window_tokens,
            stop_reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PruneConfig, PruneRecord, PruneRecordKind, PruneStopReason, Pruner};

    fn user(index: usize) -> PruneRecord {
        PruneRecord {
            item_id: format!("user-{index}"),
            transcript_index: index,
            kind: PruneRecordKind::User,
            tokens: 10,
            pinned: false,
        }
    }

    fn tool(index: usize, name: &str, tokens: u64) -> PruneRecord {
        PruneRecord {
            item_id: format!("tool-{index}"),
            transcript_index: index,
            kind: PruneRecordKind::ToolOutput {
                tool_call_id: format!("call-{index}"),
                tool_name: name.into(),
                completed: true,
            },
            tokens,
            pinned: false,
        }
    }

    #[test]
    fn exact_recent_window_and_strict_reclaim_threshold() {
        let records = vec![
            tool(0, "shell", 10_001),
            tool(1, "shell", 10_000),
            user(2),
            tool(3, "shell", 40_000),
            user(4),
            tool(5, "shell", 99_000),
            user(6),
            tool(7, "shell", 99_000),
        ];
        let plan = Pruner::plan(&records, &PruneConfig::default());
        assert_eq!(plan.protected_window_tokens, 40_000);
        assert_eq!(plan.reclaimed_tokens, 20_001);
        assert_eq!(
            plan.decisions
                .iter()
                .map(|decision| decision.transcript_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(
            plan.decisions
                .iter()
                .all(|decision| decision.transcript_index < 2)
        );
    }

    #[test]
    fn exactly_twenty_thousand_is_not_pruned() {
        let records = vec![
            tool(0, "shell", 20_000),
            tool(1, "shell", 40_000),
            user(2),
            user(3),
        ];
        let plan = Pruner::plan(&records, &PruneConfig::default());
        assert_eq!(plan.eligible_reclaim_tokens, 20_000);
        assert!(plan.decisions.is_empty());
    }

    #[test]
    fn protected_tools_and_pins_never_prune() {
        let mut pinned = tool(1, "shell", 50_000);
        pinned.pinned = true;
        let records = vec![
            tool(0, "skill", 50_000),
            pinned,
            tool(2, "shell", 40_000),
            user(3),
            user(4),
        ];
        let plan = Pruner::plan(&records, &PruneConfig::default());
        assert!(plan.decisions.is_empty());
    }

    #[test]
    fn protected_tools_do_not_consume_ordinary_protection_window() {
        let records = vec![
            tool(0, "shell", 20_001),
            tool(1, "shell", 20_000),
            tool(2, "skill", 100_000),
            tool(3, "shell", 20_000),
            user(4),
            user(5),
        ];
        let plan = Pruner::plan(&records, &PruneConfig::default());
        assert_eq!(plan.protected_window_tokens, 40_000);
        assert_eq!(plan.reclaimed_tokens, 20_001);
        assert_eq!(plan.decisions[0].transcript_index, 0);
    }

    #[test]
    fn previous_marker_stops_the_walk() {
        let records = vec![
            tool(0, "shell", 100_000),
            PruneRecord {
                item_id: "summary".into(),
                transcript_index: 1,
                kind: PruneRecordKind::SummaryMarker,
                tokens: 0,
                pinned: false,
            },
            tool(2, "shell", 40_000),
            user(3),
            user(4),
        ];
        let plan = Pruner::plan(&records, &PruneConfig::default());
        assert_eq!(plan.stop_reason, PruneStopReason::SummaryMarker);
        assert!(plan.decisions.is_empty());
    }
}
