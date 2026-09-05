//! Source-qualified context windows admitted by indexed cumulative metadata.
use super::{
    CanonicalHistory, ConversationSource, HistoryMaterializationLimits,
    MAX_MATERIALIZED_HISTORY_BYTES, MAX_MATERIALIZED_HISTORY_DECODE_BYTES,
    MAX_MATERIALIZED_HISTORY_TURNS, RecoveryError,
};
use rw_types::Turn;
use std::ops::Range;

/// One contiguous interval from a captured canonical history. `sources` is aligned
/// with `turns`; a tool block is identified by its source sequence and block index.
/// The next request starts at `range.end`, including when a byte ceiling cuts a page.
pub struct ConversationPage {
    pub range: Range<u64>,
    pub turns: Vec<Turn>,
    pub sources: Vec<ConversationSource>,
    /// Latest mutations only for returned rows; aligned with `turns` and `sources`.
    pub context_actions: Vec<Option<crate::engine::projection::ContextSurgeryAction>>,
    pub pruned_tool_outputs: std::collections::BTreeMap<String, u64>,
    pub serialized_bytes: u64,
    pub decoded_bytes: u64,
    pub has_more: bool,
}

impl CanonicalHistory {
    /// Decode allowance of an exact interval, using two metadata seeks.
    ///
    /// # Errors
    /// Rejects invalid intervals or inconsistent cumulative metadata.
    pub fn window_decoded_bytes(&self, range: Range<u64>) -> Result<u64, RecoveryError> {
        if range.start > range.end || range.end > self.head.conversation.turns {
            return Err(RecoveryError::Invalid("conversation interval"));
        }
        if range.is_empty() {
            return Ok(0);
        }
        let before = if range.start == 0 {
            0
        } else {
            self.turn_source(range.start - 1)?.cumulative_decoded_bytes
        };
        self.turn_source(range.end - 1)?
            .cumulative_decoded_bytes
            .checked_sub(before)
            .ok_or(RecoveryError::Invalid(
                "cumulative conversation decode bytes",
            ))
    }

    /// Select the largest prefix that fits all context allowances, then decode it.
    /// Selection uses logarithmic metadata seeks and never reads rejected bodies.
    /// Iterating pages covers the complete requested history without truncation.
    ///
    /// # Errors
    /// Rejects invalid intervals, limits exceeding hard bounds, and an initial turn
    /// that cannot fit. A nonempty request never returns an empty successful page.
    pub fn conversation_page(
        &self,
        range: Range<u64>,
        limits: HistoryMaterializationLimits,
    ) -> Result<ConversationPage, RecoveryError> {
        if range.start > range.end || range.end > self.head.conversation.turns {
            return Err(RecoveryError::Invalid("conversation interval"));
        }
        if limits.max_turns > MAX_MATERIALIZED_HISTORY_TURNS
            || limits.max_serialized_bytes > MAX_MATERIALIZED_HISTORY_BYTES
            || limits.max_decoded_bytes > MAX_MATERIALIZED_HISTORY_DECODE_BYTES
        {
            return Err(RecoveryError::Limit(
                "context page limits exceed hard bounds",
            ));
        }
        let mut low = range.start;
        let mut high = range
            .end
            .min(range.start.saturating_add(limits.max_turns as u64));
        while low < high {
            let candidate = low + (high - low).div_ceil(2);
            let selected = range.start..candidate;
            if self.window_bytes(selected.clone())? <= limits.max_serialized_bytes
                && self.window_decoded_bytes(selected)? <= limits.max_decoded_bytes
            {
                low = candidate;
            } else {
                high = candidate - 1;
            }
        }
        if low == range.start && !range.is_empty() {
            return Err(RecoveryError::Limit(
                "first context turn exceeds page admission",
            ));
        }
        let selected = range.start..low;
        let turns = self.materialize(selected.clone(), limits)?;
        let sources = selected
            .clone()
            .map(|ordinal| self.turn_source(ordinal))
            .collect::<Result<Vec<_>, _>>()?;
        let context_actions = selected
            .clone()
            .map(|ordinal| {
                self.context_action(&rw_types::ContextItemId(format!("conversation:{ordinal}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut pruned_tool_outputs = std::collections::BTreeMap::new();
        for turn in &turns {
            for block in &turn.blocks {
                if let rw_types::Block::ToolResult { id, .. } = block {
                    if let Some(tokens) = self.pruned_output(&id.0)? {
                        pruned_tool_outputs.insert(id.0.clone(), tokens);
                    }
                }
            }
        }
        Ok(ConversationPage {
            pruned_tool_outputs,
            serialized_bytes: self.window_bytes(selected.clone())?,
            decoded_bytes: self.window_decoded_bytes(selected.clone())?,
            has_more: selected.end < range.end,
            range: selected,
            turns,
            sources,
            context_actions,
        })
    }
}
