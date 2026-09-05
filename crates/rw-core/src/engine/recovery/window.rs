//! Indexed admission and completed-turn boundaries without scanning historical bodies.

use super::{
    CanonicalHistory, ConversationCut, RecoveryControl, RecoveryError,
    projector::key,
    state::{BOUNDARIES, Boundary, SOURCE_ORDINAL},
};
use std::ops::Range;

/// Exact rewind state retained in the derived boundary index for one completed turn.
pub struct RecoveryBoundary {
    pub source_sequence: rw_types::SequenceId,
    pub agent_turn: u64,
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
        let Some(boundary) = self.boundary(turn)? else {
            return Ok(None);
        };
        Ok(Some(RecoveryBoundary {
            source_sequence: boundary.source_sequence,
            agent_turn: turn,
            conversation: boundary.conversation,
            control: boundary.control,
            budget: boundary.budget,
        }))
    }

    /// Resolve the controls that a rewind will make effective, without reading
    /// historical conversation bodies. Accounting and physical source identity
    /// remain attached to the captured present; only rewindable state changes.
    ///
    /// # Errors
    /// Rejects absent completed boundaries, invalid selectors or live-payload overflow.
    pub fn recovery_at_completed_turn(
        &self,
        turn: u64,
    ) -> Result<super::RecoveryBootstrap, RecoveryError> {
        let boundary = self
            .boundary(turn)?
            .ok_or(RecoveryError::Invalid("unknown rewind boundary"))?;
        let mut head = self.head.clone();
        head.apply_rewind_boundary(&boundary, turn);
        let controls = self.control_payloads_at(&head, super::MAX_CONTROL_SOURCE_BYTES)?;
        Ok(super::RecoveryBootstrap {
            head,
            controls,
            interrupted: None,
        })
    }

    fn boundary(&self, turn: u64) -> Result<Option<Boundary>, RecoveryError> {
        let Some(row) = self.read.get(key(BOUNDARIES, 0, turn))? else {
            return Ok(None);
        };
        let boundary: Boundary = serde_json::from_slice(&row.payload)?;
        if boundary.control.next_turn <= turn || boundary.control.completed_turns == 0 {
            return Err(RecoveryError::Invalid("completed turn boundary counters"));
        }
        if boundary.source_sequence.0 >= self.head.next_sequence {
            return Err(RecoveryError::Invalid("boundary source sequence"));
        }
        Ok(Some(boundary))
    }

    /// Resolve a source-qualified rewind against this exact effective prefix.
    ///
    /// # Errors
    /// Rejects stale views, removed or non-user sources, mismatched turn identities,
    /// active turns, and absent completed workspace boundaries.
    pub fn resolve_source_rewind(
        &self,
        expected_through: rw_types::SequenceId,
        source: rw_types::SequenceId,
        turn: u64,
        position: rw_types::RewindSourcePosition,
    ) -> Result<u64, RecoveryError> {
        if self.head.next_sequence.checked_sub(1) != Some(expected_through.0) {
            return Err(RecoveryError::Invalid("rewind view has changed"));
        }
        if self.head.control.active.is_some() || self.head.control.active_shell.is_some() {
            return Err(RecoveryError::Invalid("rewind requires an idle session"));
        }
        let (_, item) = self
            .source_turn(source)?
            .ok_or(RecoveryError::Invalid("rewind source is not effective"))?;
        if item.role != rw_types::Role::User
            || item.kind != super::TurnSourceKind::Committed
            || item.agent_turn != turn
        {
            return Err(RecoveryError::Invalid(
                "rewind source is not the selected user turn",
            ));
        }
        let boundary = match position {
            rw_types::RewindSourcePosition::Before => self.completed_before(turn)?,
            rw_types::RewindSourcePosition::Through => self.completed_boundary(turn)?,
        }
        .ok_or(RecoveryError::Invalid(
            "rewind has no completed workspace boundary",
        ))?;
        Ok(boundary.agent_turn)
    }

    /// Resolve a source identity only while it is part of the effective conversation.
    /// Overwritten or rewound ordinals cannot authorize a mutation.
    ///
    /// # Errors
    /// Rejects malformed reverse-index rows or storage errors.
    pub fn source_turn(
        &self,
        sequence: rw_types::SequenceId,
    ) -> Result<Option<(u64, super::ConversationSource)>, RecoveryError> {
        let Some(row) = self.read.get(key(
            SOURCE_ORDINAL,
            self.head.conversation.generation,
            sequence.0,
        ))?
        else {
            return Ok(None);
        };
        let ordinal: u64 = serde_json::from_slice(&row.payload)?;
        if ordinal >= self.head.conversation.turns {
            return Ok(None);
        }
        let source = self.turn_source(ordinal)?;
        Ok((source.sequence == sequence).then_some((ordinal, source)))
    }

    /// Resolve the latest completed turn strictly before the requested turn.
    ///
    /// # Errors
    /// Rejects malformed boundary metadata or storage errors.
    pub fn completed_before(&self, turn: u64) -> Result<Option<RecoveryBoundary>, RecoveryError> {
        let Some(row) = self.read.last_before(BOUNDARIES, 0, turn)? else {
            return Ok(None);
        };
        self.completed_boundary(row.key.ordinal)
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

impl super::RecoveryHead {
    pub(super) fn apply_rewind_boundary(&mut self, boundary: &Boundary, turn: u64) {
        self.conversation = boundary.conversation;
        self.control.completed_turns = boundary.control.completed_turns;
        self.control.todos = boundary.control.todos;
        self.control.mode = boundary.control.mode;
        self.control.mode_id.clone_from(&boundary.control.mode_id);
        self.control.pending_plan = boundary.control.pending_plan;
        self.control.approved_plan = boundary.control.approved_plan;
        self.control.plan_gate_active = boundary.control.plan_gate_active;
        self.control.queued.clear();
        self.control
            .accepted
            .retain(|accepted| accepted.agent_turn <= turn);
        self.control
            .questions
            .retain(|question| question.agent_turn <= turn);
        self.control.active = None;
        self.context_cut = boundary.context_cut;
        self.budget = boundary.budget;
        self.extension_root = boundary.extension_root;
        self.compacting = None;
    }
}
