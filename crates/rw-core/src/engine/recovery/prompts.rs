//! Source cuts for recorded context assembly, independent of lifetime audit replay.
use super::{CanonicalHistory, RecoveryError, RecoveryHead, projector::key, state::PROMPTS};

impl CanonicalHistory {
    /// Resolve the latest effective prompt without visiting discarded source rows.
    /// # Errors
    /// Rejects index corruption or storage failure.
    pub fn latest_prompt_turn(&self) -> Result<Option<u64>, RecoveryError> {
        Ok(self
            .read
            .last_before(PROMPTS, 0, self.head.control.next_turn)?
            .map(|row| row.key.ordinal))
    }

    /// Select the first recorded context assembly for an effective agent turn.
    /// The index transaction and raw source remain pinned to this captured reader.
    /// # Errors
    /// Rejects absent or invalid prompt boundaries and source-prefix mismatches.
    pub fn prompt_at_turn(&self, turn: u64) -> Result<Self, RecoveryError> {
        let row = self
            .read
            .get(key(PROMPTS, 0, turn))?
            .ok_or(RecoveryError::Invalid(
                "no assembled prompt was recorded for the requested turn",
            ))?;
        let head: RecoveryHead = serde_json::from_slice(&row.payload)?;
        head.validate()?;
        if head.session_id != self.head.session_id
            || head.next_sequence > self.head.next_sequence
            || head
                .control
                .active
                .as_ref()
                .is_none_or(|active| active.turn != turn)
        {
            return Err(RecoveryError::Invalid("historical prompt source identity"));
        }
        let mut selected = self.clone();
        selected.source = self
            .source
            .prefix_through(head.next_sequence.checked_sub(1).map(rw_types::SequenceId))?;
        selected.head = head;
        selected.prompt_cut = true;
        Ok(selected)
    }
}
