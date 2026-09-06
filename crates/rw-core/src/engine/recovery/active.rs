use super::{
    CanonicalHistory, HistoryMaterializationLimits, RecoveryError, ToolStartIdentity,
    read::SourceReader,
    state::{
        ACTIVE_ASSISTANT, ACTIVE_TOOL_LIFECYCLE, ACTIVE_TOOL_RESULTS, ActiveSource, SourceTotals,
        ToolLifecycleSource,
    },
};
use rw_types::{EngineEvent, SequenceId, ToolInvocationId, Turn};
use std::collections::{BTreeMap, VecDeque};

/// Bounded inputs for deterministic repair of the active interrupted turn only.
/// Completed historical turns and old streaming output are not retained here.
pub struct InterruptedTurnInputs {
    pub turn: u64,
    pub conversation: Vec<Turn>,
    pub pending_starts: Vec<ToolStartIdentity>,
    pub fragments: Vec<EngineEvent>,
}

impl CanonicalHistory {
    /// Read current-turn canonical IR, unresolved starts and uncommitted output fragments.
    /// Metadata admission happens before any historical body is decoded.
    ///
    /// # Errors
    /// Rejects resource overflow, missing/mismatched source records or corrupt counters.
    pub fn interrupted_inputs(&self) -> Result<Option<InterruptedTurnInputs>, RecoveryError> {
        self.interrupted_inputs_with_allowance(super::MAX_MATERIALIZED_HISTORY_DECODE_BYTES)
    }

    pub(super) fn interrupted_inputs_with_allowance(
        &self,
        max_decoded_bytes: u64,
    ) -> Result<Option<InterruptedTurnInputs>, RecoveryError> {
        let Some(active) = &self.head.control.active else {
            return Ok(None);
        };
        let limits = HistoryMaterializationLimits::default();
        let range = active.first_conversation_ordinal..self.head.conversation.turns;
        let conversation_bytes = self.window_bytes(range.clone())?;
        let totals = [
            active.assistant_parts,
            active.tool_results,
            active.tool_lifecycle,
        ];
        let records = totals
            .iter()
            .try_fold(range.end - range.start, |count, totals| {
                count.checked_add(totals.records)
            })
            .ok_or(RecoveryError::Limit("interrupted source count"))?;
        let bytes = totals
            .iter()
            .try_fold(conversation_bytes, |count, totals| {
                count.checked_add(totals.serialized_bytes)
            })
            .ok_or(RecoveryError::Limit("interrupted source bytes"))?;
        let decoded_bytes = totals
            .iter()
            .try_fold(self.window_decoded_bytes(range.clone())?, |bytes, total| {
                bytes.checked_add(total.decoded_bytes)
            })
            .ok_or(RecoveryError::Limit("interrupted decoded counter"))?;
        if records > limits.max_turns as u64
            || bytes > limits.max_serialized_bytes
            || decoded_bytes > max_decoded_bytes
        {
            return Err(RecoveryError::Limit("interrupted turn materialization"));
        }
        let mut sources = self.active_sources(
            ACTIVE_ASSISTANT,
            active.turn,
            active.last_assistant_commit,
            active.assistant_parts,
        )?;
        sources.extend(self.active_sources(
            ACTIVE_TOOL_RESULTS,
            active.turn,
            active.last_tool_commit,
            active.tool_results,
        )?);
        sources.sort_by_key(|source| source.sequence);
        let mut reader = SourceReader {
            source: &self.source,
            events: VecDeque::new(),
        };
        let mut fragments = Vec::with_capacity(sources.len());
        for source in sources {
            let event = reader.event(source.sequence)?;
            if super::encoding::serialized_size(&event)? != source.serialized_bytes {
                return Err(RecoveryError::Invalid("active source byte count"));
            }
            if !matches!(
                event,
                EngineEvent::TextDelta { .. }
                    | EngineEvent::ThinkingDelta { .. }
                    | EngineEvent::CitationDelta { .. }
                    | EngineEvent::ToolCallFinished { .. }
            ) {
                return Err(RecoveryError::Invalid("active source event kind"));
            }
            let plan = rw_types::allocation::AllocationPlan::new(event)
                .map_err(|_| RecoveryError::Limit("active fragment allocation"))?;
            if plan.bytes() as u64 > source.decoded_bytes {
                return Err(RecoveryError::Invalid("active source decoded allowance"));
            }
            fragments.push(plan.prepare().into_inner());
        }
        Ok(Some(InterruptedTurnInputs {
            turn: active.turn,
            conversation: self.materialize(range, limits)?,
            pending_starts: self.pending_tool_starts(active.turn, active.tool_lifecycle)?,
            fragments,
        }))
    }

    fn active_sources(
        &self,
        namespace: u8,
        turn: u64,
        after: Option<SequenceId>,
        expected: SourceTotals,
    ) -> Result<Vec<ActiveSource>, RecoveryError> {
        let mut cursor = after.map(|sequence| sequence.0);
        let mut sources = Vec::new();
        let mut observed = SourceTotals::default();
        loop {
            let page = self.read.page(namespace, turn, cursor, 128, 1024 * 1024)?;
            for row in page.rows {
                let source: ActiveSource = serde_json::from_slice(&row.payload)?;
                if row.key.ordinal != source.sequence.0
                    || source.sequence.0 >= self.head.next_sequence
                {
                    return Err(RecoveryError::Invalid("active source sequence"));
                }
                observed.records += 1;
                observed.decoded_bytes =
                    observed
                        .decoded_bytes
                        .checked_add(source.decoded_bytes)
                        .ok_or(RecoveryError::Limit("active decoded source bytes"))?;
                observed.serialized_bytes = observed
                    .serialized_bytes
                    .checked_add(source.serialized_bytes)
                    .ok_or(RecoveryError::Limit("active source bytes"))?;
                if observed.records > expected.records
                    || observed.serialized_bytes > expected.serialized_bytes
                    || observed.decoded_bytes > expected.decoded_bytes
                {
                    return Err(RecoveryError::Invalid("active source admission metadata"));
                }
                sources.push(source);
            }
            cursor = page.next_cursor;
            if !page.has_more {
                break;
            }
        }
        if observed != expected {
            return Err(RecoveryError::Invalid("active source totals"));
        }
        Ok(sources)
    }

    fn pending_tool_starts(
        &self,
        turn: u64,
        expected: SourceTotals,
    ) -> Result<Vec<ToolStartIdentity>, RecoveryError> {
        let mut cursor = None;
        let mut pending = BTreeMap::<ToolInvocationId, ToolStartIdentity>::new();
        let mut observed = SourceTotals::default();
        loop {
            let page = self
                .read
                .page(ACTIVE_TOOL_LIFECYCLE, turn, cursor, 128, 1024 * 1024)?;
            for row in page.rows {
                observed.records += 1;
                observed.serialized_bytes = observed
                    .serialized_bytes
                    .checked_add(row.payload.len() as u64)
                    .ok_or(RecoveryError::Limit("tool identity bytes"))?;
                if observed.records > expected.records
                    || observed.serialized_bytes > expected.serialized_bytes
                {
                    return Err(RecoveryError::Invalid("tool lifecycle admission metadata"));
                }
                let lifecycle: ToolLifecycleSource = serde_json::from_slice(&row.payload)?;
                observed.decoded_bytes = observed
                    .decoded_bytes
                    .checked_add(super::encoding::decode_bytes(&lifecycle)?)
                    .ok_or(RecoveryError::Limit("tool lifecycle decoded counter"))?;
                if observed.decoded_bytes > expected.decoded_bytes {
                    return Err(RecoveryError::Invalid("tool lifecycle decoded allowance"));
                }
                match lifecycle {
                    ToolLifecycleSource::Started(start) => {
                        if pending.insert(start.invocation_id.clone(), start).is_some() {
                            return Err(RecoveryError::Invalid("duplicate active tool invocation"));
                        }
                    }
                    ToolLifecycleSource::Finished(invocation) => {
                        pending.remove(&invocation);
                    }
                }
            }
            cursor = page.next_cursor;
            if !page.has_more {
                break;
            }
        }
        if observed != expected {
            return Err(RecoveryError::Invalid("tool lifecycle totals"));
        }
        Ok(pending.into_values().collect())
    }
}
