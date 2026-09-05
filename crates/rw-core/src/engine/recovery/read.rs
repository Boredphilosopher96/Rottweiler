use super::{
    ConversationSource, RecoveryError, RecoveryHead, TurnSourceKind,
    encoding::serialized_size,
    projector::{CanonicalRecovery, key},
    state::CONVERSATION,
};
use rw_store::session::{
    SessionEventPageLimits, journal::JournalReadView, recovery_index::RecoveryReadView,
};
use rw_types::{EngineEvent, SequenceId, Turn};
use std::{collections::VecDeque, ops::Range};

/// Hard maximum canonical JSON bytes retained by one provider/context materialization.
pub const MAX_MATERIALIZED_HISTORY_BYTES: u64 = 32 * 1024 * 1024;
/// Retained typed turns, independent of the raw reader's page and decode scratch.
pub const MAX_MATERIALIZED_HISTORY_DECODE_BYTES: u64 = 64 * 1024 * 1024;
/// Hard maximum IR turns retained by one provider/context materialization.
pub const MAX_MATERIALIZED_HISTORY_TURNS: usize = 16_384;

/// Independent bounds for an explicitly materialized provider/context window.
/// Callers may reduce the hard maxima, never expand them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryMaterializationLimits {
    pub max_turns: usize,
    pub max_serialized_bytes: u64,
    pub max_decoded_bytes: u64,
}
impl Default for HistoryMaterializationLimits {
    fn default() -> Self {
        Self {
            max_turns: MAX_MATERIALIZED_HISTORY_TURNS,
            max_serialized_bytes: MAX_MATERIALIZED_HISTORY_BYTES,
            max_decoded_bytes: MAX_MATERIALIZED_HISTORY_DECODE_BYTES,
        }
    }
}

/// Index snapshot captured before the corresponding raw committed view.
pub struct RecoverySnapshot {
    read: RecoveryReadView,
    head: RecoveryHead,
}
impl RecoverySnapshot {
    /// Bounded control state at the exact captured index prefix.
    #[must_use]
    pub const fn head(&self) -> &RecoveryHead {
        &self.head
    }

    /// Bind a subsequently captured journal view to this exact index snapshot.
    ///
    /// # Errors
    /// Rejects foreign/older source views and incomplete projection maintenance.
    pub fn bind_source(self, source: &JournalReadView) -> Result<CanonicalHistory, RecoveryError> {
        let source = self.read.bind_source(source)?;
        Ok(CanonicalHistory {
            read: self.read,
            head: self.head,
            source,
        })
    }
}
impl CanonicalRecovery {
    /// Capture consistent recovery state before capturing its source journal.
    ///
    /// # Errors
    /// Fails closed while rewind/clear publication is incomplete or storage is invalid.
    pub fn snapshot(&self) -> Result<RecoverySnapshot, RecoveryError> {
        let read = self.index.read()?;
        let head = self.decode_head(&read)?;
        if head.maintenance.is_some() {
            return Err(RecoveryError::Maintenance);
        }
        Ok(RecoverySnapshot { read, head })
    }
}

/// Exact canonical history snapshot; all pages share one metadata transaction and raw prefix.
pub struct CanonicalHistory {
    pub(super) read: RecoveryReadView,
    pub(super) head: RecoveryHead,
    pub(super) source: JournalReadView,
}
impl CanonicalHistory {
    /// Bounded state describing the visible conversation and current controls.
    #[must_use]
    pub const fn head(&self) -> &RecoveryHead {
        &self.head
    }

    /// Read admission metadata by stable ordinal, without decoding a historical body.
    ///
    /// # Errors
    /// Rejects out-of-range ordinals, missing rows or corrupt metadata.
    pub fn turn_source(&self, ordinal: u64) -> Result<ConversationSource, RecoveryError> {
        if ordinal >= self.head.conversation.turns {
            return Err(RecoveryError::Invalid("conversation ordinal"));
        }
        let row = self
            .read
            .get(key(
                CONVERSATION,
                self.head.conversation.generation,
                ordinal,
            ))?
            .ok_or(RecoveryError::Invalid("missing canonical conversation row"))?;
        Ok(serde_json::from_slice(&row.payload)?)
    }

    /// Compute exact serialized admission for a canonical interval using two indexed seeks.
    ///
    /// # Errors
    /// Rejects invalid intervals or inconsistent cumulative counters.
    pub fn window_bytes(&self, range: Range<u64>) -> Result<u64, RecoveryError> {
        if range.start > range.end || range.end > self.head.conversation.turns {
            return Err(RecoveryError::Invalid("conversation interval"));
        }
        if range.is_empty() {
            return Ok(0);
        }
        let start = if range.start == 0 {
            0
        } else {
            self.turn_source(range.start - 1)?.cumulative_bytes
        };
        self.turn_source(range.end - 1)?
            .cumulative_bytes
            .checked_sub(start)
            .ok_or(RecoveryError::Invalid("cumulative conversation bytes"))
    }

    /// Materialize exactly the selected canonical interval after metadata admission.
    /// This never substitutes a recent tail when the requested history exceeds capacity.
    ///
    /// # Errors
    /// Rejects turn/byte overflow before reading payloads, or mismatched canonical sources.
    #[tracing::instrument(target = "rw_performance", level = "trace", name = "recovery.materialize", skip_all, fields(from = range.start, through = range.end))]
    pub fn materialize(
        &self,
        range: Range<u64>,
        limits: HistoryMaterializationLimits,
    ) -> Result<Vec<Turn>, RecoveryError> {
        if limits.max_turns > MAX_MATERIALIZED_HISTORY_TURNS
            || limits.max_serialized_bytes > MAX_MATERIALIZED_HISTORY_BYTES
            || limits.max_decoded_bytes > MAX_MATERIALIZED_HISTORY_DECODE_BYTES
        {
            return Err(RecoveryError::Limit(
                "materialization limits exceed hard bounds",
            ));
        }
        let bytes = self.window_bytes(range.clone())?;
        let decoded_bytes = self.window_decoded_bytes(range.clone())?;
        let count = range.end - range.start;
        if count > limits.max_turns as u64
            || bytes > limits.max_serialized_bytes
            || decoded_bytes > limits.max_decoded_bytes
        {
            return Err(RecoveryError::Limit("provider history materialization"));
        }
        let mut output = Vec::with_capacity(
            usize::try_from(count).map_err(|_| RecoveryError::Limit("provider turn count"))?,
        );
        let mut source = SourceReader {
            source: &self.source,
            events: VecDeque::new(),
        };
        let mut observed_bytes = 0_u64;
        let mut observed_decoded = 0_u64;
        for ordinal in range {
            let row = self.turn_source(ordinal)?;
            observed_decoded = observed_decoded
                .checked_add(row.decoded_bytes)
                .ok_or(RecoveryError::Limit("materialized decode counter"))?;
            if observed_decoded > limits.max_decoded_bytes {
                return Err(RecoveryError::Limit("provider decoded materialization"));
            }
            let turn = source.turn(&row)?;
            let actual_bytes = serialized_size(&turn)?;
            if turn.role != row.role
                || actual_bytes != row.serialized_bytes
                || turn
                    .meta
                    .model
                    .as_ref()
                    .is_some_and(|model| model.contains('/'))
                    != row.has_resolved_model
            {
                return Err(RecoveryError::Invalid("canonical source metadata"));
            }
            observed_bytes = observed_bytes
                .checked_add(actual_bytes)
                .ok_or(RecoveryError::Limit("materialized byte counter"))?;
            if observed_bytes > limits.max_serialized_bytes {
                return Err(RecoveryError::Limit("provider history materialization"));
            }
            output.push(turn);
        }
        if observed_bytes != bytes || observed_decoded != decoded_bytes {
            return Err(RecoveryError::Invalid("cumulative source size"));
        }
        Ok(output)
    }
}

pub(super) struct SourceReader<'a> {
    pub(super) source: &'a JournalReadView,
    pub(super) events: VecDeque<EngineEvent>,
}
impl SourceReader<'_> {
    fn turn(&mut self, row: &ConversationSource) -> Result<Turn, RecoveryError> {
        let event = self.event(row.sequence)?;
        let turn = match (row.kind, event) {
            (
                TurnSourceKind::Committed,
                EngineEvent::ConversationTurnCommitted {
                    agent_turn, turn, ..
                },
            ) if agent_turn == row.agent_turn => turn,
            (
                TurnSourceKind::Shell,
                EngineEvent::UserShellStateChanged {
                    command,
                    active: false,
                    status: Some(status),
                    captured_output,
                    ..
                },
            ) => crate::engine::projection::shell_context_turn(
                command.as_deref().unwrap_or_default(),
                status,
                captured_output.as_deref(),
            ),
            _ => return Err(RecoveryError::Invalid("canonical source selector")),
        };
        let plan = rw_types::allocation::AllocationPlan::new(turn)
            .map_err(|_| RecoveryError::Limit("materialized allocation overflow"))?;
        if plan.bytes() as u64 > row.decoded_bytes {
            return Err(RecoveryError::Invalid(
                "canonical decoded allocation metadata",
            ));
        }
        let turn = plan.prepare().into_inner();
        Ok(turn)
    }

    pub(super) fn event(&mut self, sequence: SequenceId) -> Result<EngineEvent, RecoveryError> {
        if self
            .events
            .front()
            .and_then(EngineEvent::meta)
            .is_none_or(|first| first.sequence_id > sequence)
            || self
                .events
                .back()
                .and_then(EngineEvent::meta)
                .is_none_or(|last| last.sequence_id < sequence)
        {
            self.events = self
                .source
                .page::<EngineEvent>(
                    sequence.0.checked_sub(1).map(SequenceId),
                    SessionEventPageLimits {
                        max_page_events: 64,
                        max_page_bytes: SessionEventPageLimits::default().max_line_bytes as u64 + 1,
                        ..SessionEventPageLimits::default()
                    },
                )?
                .events
                .into_iter()
                .map(|entry| entry.event)
                .collect();
        }
        while let Some(event) = self.events.pop_front() {
            let meta = event
                .meta()
                .ok_or(RecoveryError::Invalid("non-durable source"))?;
            if meta.sequence_id == sequence {
                return Ok(event);
            }
            if meta.sequence_id > sequence {
                break;
            }
        }
        Err(RecoveryError::Invalid("missing canonical source event"))
    }
}
