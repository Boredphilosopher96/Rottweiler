//! Indexed admission and completed-turn boundaries without scanning historical bodies.

use super::{
    CanonicalHistory, ConversationCut, RecoveryControl, RecoveryError,
    projector::key,
    state::{BOUNDARIES, Boundary},
};
use std::ops::Range;

/// Exact rewind state retained in the derived boundary index for one completed turn.
pub struct RecoveryBoundary {
    pub conversation: ConversationCut,
    pub control: RecoveryControl,
    pub budget: rw_context::BudgetSnapshot,
}

impl CanonicalHistory {
    /// Read a completed turn's bounded checkpoint by one indexed seek.
    ///
    /// # Errors
    /// Rejects malformed rows or storage errors. `None` means no currently valid boundary.
    pub fn completed_boundary(&self, turn: u64) -> Result<Option<RecoveryBoundary>, RecoveryError> {
        let Some(row) = self.read.get(key(BOUNDARIES, 0, turn))? else {
            return Ok(None);
        };
        let boundary: Boundary = serde_json::from_slice(&row.payload)?;
        if boundary.control.next_turn <= turn || boundary.control.completed_turns == 0 {
            return Err(RecoveryError::Invalid("completed turn boundary counters"));
        }
        Ok(Some(RecoveryBoundary {
            conversation: boundary.conversation,
            control: boundary.control,
            budget: boundary.budget,
        }))
    }

    /// Estimated token count of an exact canonical interval, using cumulative metadata.
    /// This is a planning estimate, never a provider tokenizer or billing bound.
    ///
    /// # Errors
    /// Rejects invalid intervals and inconsistent cumulative metadata.
    pub fn window_estimated_tokens(&self, range: Range<u64>) -> Result<u64, RecoveryError> {
        if range.start > range.end || range.end > self.head.conversation.turns {
            return Err(RecoveryError::Invalid("conversation interval"));
        }
        if range.is_empty() {
            return Ok(0);
        }
        let before = if range.start == 0 {
            0
        } else {
            self.turn_source(range.start - 1)?.cumulative_tokens
        };
        self.turn_source(range.end - 1)?
            .cumulative_tokens
            .checked_sub(before)
            .ok_or(RecoveryError::Invalid("cumulative conversation tokens"))
    }
}
