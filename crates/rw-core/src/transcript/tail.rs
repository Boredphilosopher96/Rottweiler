//! Bounded live previews folded into the same canonical prefix as semantic rows.
use super::{TranscriptProjectionError, TranscriptRowLookup, turn_number};
use rw_store::session::transcript_index::{
    MAX_AUXILIARY_CELL_BYTES, MAX_AUXILIARY_CELLS, TranscriptIndexMutation,
};
use rw_types::citation_admission::{MAX_TURN_CITATION_TEXT_BYTES, MAX_TURN_CITATIONS};
use rw_types::tool_admission::MAX_PENDING_TOOL_INVOCATIONS;
use rw_types::transcript_tail::{TRANSCRIPT_TAIL_TEXT_BYTES, TRANSCRIPT_TAIL_TOOL_BYTES};
use rw_types::{EngineEvent, Role};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
mod chunks;
mod citations;
mod read;
mod tools;
pub use read::{read_transcript_tail, validate_tail_read};

const TEXT_FIRST: u16 = 0;
const THINKING_FIRST: u16 = const_slot(TRANSCRIPT_TAIL_TEXT_BYTES / MAX_AUXILIARY_CELL_BYTES);
const CITATION_INDEX_FIRST: u16 = 2 * THINKING_FIRST;
const CITATION_DATA_FIRST: u16 = CITATION_INDEX_FIRST + 2;
const CITATION_ENCODED_LIMIT: usize = MAX_TURN_CITATION_TEXT_BYTES * 6 + MAX_TURN_CITATIONS * 128;
const TOOL_INDEX: u16 =
    CITATION_DATA_FIRST + const_slot(CITATION_ENCODED_LIMIT.div_ceil(MAX_AUXILIARY_CELL_BYTES));
const TOOL_DATA_FIRST: u16 = TOOL_INDEX + 1;
const TOOL_CELLS: u16 = const_slot(TRANSCRIPT_TAIL_TOOL_BYTES / MAX_AUXILIARY_CELL_BYTES);
const TOOL_PROVIDER_FIRST: u16 =
    TOOL_DATA_FIRST + TOOL_CELLS * const_slot(MAX_PENDING_TOOL_INVOCATIONS);
const _: () = assert!(
    TOOL_PROVIDER_FIRST as usize + MAX_PENDING_TOOL_INVOCATIONS <= MAX_AUXILIARY_CELLS as usize
);

/// Scalar progress only; byte chunks and fixed-size entity indexes live in auxiliary cells.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TailState {
    pub active_turn: Option<u64>,
    pub turn_started: Option<u64>,
    pub epoch: u64,
    pub text_bytes: usize,
    pub thinking_bytes: usize,
    pub text_truncated: bool,
    pub thinking_truncated: bool,
    pub citation_count: usize,
    pub citation_utf8_bytes: usize,
    pub citation_encoded_bytes: usize,
    pub tools_epoch: u64,
    pub tools_count: usize,
}
impl TailState {
    pub(super) fn validate(&self, next_sequence: u64) -> Result<(), TranscriptProjectionError> {
        if self.text_bytes > TRANSCRIPT_TAIL_TEXT_BYTES
            || self.thinking_bytes > TRANSCRIPT_TAIL_TEXT_BYTES
            || self.citation_count > MAX_TURN_CITATIONS
            || self.citation_utf8_bytes > MAX_TURN_CITATION_TEXT_BYTES
            || self.citation_encoded_bytes > CITATION_ENCODED_LIMIT
            || self.tools_count > MAX_PENDING_TOOL_INVOCATIONS
            || self.epoch > next_sequence.saturating_sub(1)
            || self.tools_epoch > next_sequence.saturating_sub(1)
            || self.active_turn.is_some() != self.turn_started.is_some()
            || self
                .turn_started
                .is_some_and(|source| source >= next_sequence)
            || (next_sequence == 0 && *self != Self::default())
        {
            return Err(TranscriptProjectionError::Invalid("tail checkpoint bounds"));
        }
        Ok(())
    }
    pub(super) fn reset(&mut self, source: u64) {
        *self = Self {
            epoch: source,
            tools_epoch: source,
            ..Self::default()
        };
    }
    fn clear_response(&mut self, source: u64) {
        self.epoch = source;
        self.text_bytes = 0;
        self.thinking_bytes = 0;
        self.text_truncated = false;
        self.thinking_truncated = false;
        self.citation_count = 0;
        self.citation_utf8_bytes = 0;
        self.citation_encoded_bytes = 0;
    }
}

pub(super) fn project(
    event: &EngineEvent,
    state: &mut TailState,
    rows: &impl TranscriptRowLookup,
) -> Result<Vec<TranscriptIndexMutation>, TranscriptProjectionError> {
    let source = event
        .meta()
        .ok_or(TranscriptProjectionError::Invalid("tail durable source"))?
        .sequence_id
        .0;
    match event {
        EngineEvent::TurnStarted { turn_id, .. } => {
            state.reset(source);
            state.active_turn = Some(turn_number(turn_id)?);
            state.turn_started = Some(source);
        }
        EngineEvent::TurnFinished { .. } => state.reset(source),
        EngineEvent::ConversationTurnCommitted { turn, .. }
            if matches!(turn.role, Role::Assistant | Role::Tool) =>
        {
            state.clear_response(source);
        }
        EngineEvent::CompactionStarted { .. } | EngineEvent::CompactionFinished { .. } => {
            state.clear_response(source);
        }
        EngineEvent::TextDelta { turn_id, text, .. } => {
            require_turn(state, turn_id)?;
            return append_text(
                TEXT_FIRST,
                &mut state.text_bytes,
                &mut state.text_truncated,
                text,
                rows,
            );
        }
        EngineEvent::ThinkingDelta { turn_id, text, .. } => {
            require_turn(state, turn_id)?;
            return append_text(
                THINKING_FIRST,
                &mut state.thinking_bytes,
                &mut state.thinking_truncated,
                text,
                rows,
            );
        }
        EngineEvent::CitationDelta {
            turn_id,
            uri,
            title,
            ..
        } => {
            require_turn(state, turn_id)?;
            return citations::append(state, source, uri, title.as_deref(), rows);
        }
        EngineEvent::ToolCallStarted { .. }
        | EngineEvent::ToolCallFinished { .. }
        | EngineEvent::ToolOutputDelta { .. } => return tools::project(event, state, rows),
        _ => {}
    }
    Ok(Vec::new())
}

fn require_turn(
    state: &TailState,
    turn: &rw_types::TurnId,
) -> Result<(), TranscriptProjectionError> {
    if state.active_turn != Some(turn_number(turn)?) {
        return Err(TranscriptProjectionError::Invalid("tail active turn"));
    }
    Ok(())
}
fn append_text(
    first: u16,
    bytes: &mut usize,
    truncated: &mut bool,
    text: &str,
    rows: &impl TranscriptRowLookup,
) -> Result<Vec<TranscriptIndexMutation>, TranscriptProjectionError> {
    if *truncated {
        return Ok(Vec::new());
    }
    let prefix = chunks::utf8_prefix(text, TRANSCRIPT_TAIL_TEXT_BYTES.saturating_sub(*bytes));
    if prefix.is_empty() {
        *truncated = !text.is_empty();
        return Ok(Vec::new());
    }
    let mut writer = chunks::CellAppender::new(first, *bytes, TRANSCRIPT_TAIL_TEXT_BYTES, rows)?;
    writer
        .write_all(prefix.as_bytes())
        .map_err(|_| TranscriptProjectionError::Invalid("tail text admission"))?;
    let (next, mutations) = writer.finish()?;
    *bytes = next;
    *truncated = prefix.len() < text.len();
    Ok(mutations)
}

fn index_cell(
    rows: &impl TranscriptRowLookup,
    key: u16,
    epoch: u64,
    entries: usize,
) -> Result<Vec<u8>, TranscriptProjectionError> {
    let extent = 8 + entries * 16;
    if let Some(cell) = rows.auxiliary_cell(key)? {
        if cell.len() != extent {
            return Err(TranscriptProjectionError::Invalid("tail index extent"));
        }
        let stored_epoch = read_u64(&cell[..8]);
        if stored_epoch > epoch {
            return Err(TranscriptProjectionError::Invalid("future tail cell epoch"));
        }
        if stored_epoch == epoch {
            return Ok(cell);
        }
    }
    let mut cell = vec![0; extent];
    cell[..8].copy_from_slice(&epoch.to_le_bytes());
    Ok(cell)
}
fn read_u64(bytes: &[u8]) -> u64 {
    let mut value = [0; 8];
    value.copy_from_slice(bytes);
    u64::from_le_bytes(value)
}
fn read_u32(bytes: &[u8]) -> u32 {
    let mut value = [0; 4];
    value.copy_from_slice(bytes);
    u32::from_le_bytes(value)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod read_tests;

#[cfg(test)]
mod chunk_tests;

const fn const_slot(value: usize) -> u16 {
    assert!(value <= 65535);
    let bytes = value.to_le_bytes();
    u16::from_le_bytes([bytes[0], bytes[1]])
}
fn slot(value: usize) -> Result<u16, TranscriptProjectionError> {
    u16::try_from(value).map_err(|_| TranscriptProjectionError::Invalid("tail slot range"))
}
