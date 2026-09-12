//! Bounded reads of the current effective live tail; no journal replay on the read path.
use super::{
    CITATION_DATA_FIRST, CITATION_INDEX_FIRST, MAX_AUXILIARY_CELL_BYTES,
    MAX_PENDING_TOOL_INVOCATIONS, TEXT_FIRST, THINKING_FIRST, TOOL_CELLS, TOOL_DATA_FIRST,
    TOOL_INDEX, TailState, TranscriptProjectionError, index_cell, read_u32, read_u64, slot,
};
use rw_store::session::transcript_index::TranscriptIndex;
use rw_types::transcript::{
    TranscriptContent, TranscriptGeneration, TranscriptToolStatus, TranscriptView,
};
use rw_types::transcript_tail::{
    TRANSCRIPT_TAIL_MIN_PAGE_BYTES, TRANSCRIPT_TAIL_PAGE_BYTES, TRANSCRIPT_TAIL_PAGE_ITEMS,
    TRANSCRIPT_TAIL_TOOL_BYTES, TranscriptTailCitation, TranscriptTailContent,
    TranscriptTailIdentity, TranscriptTailPage, TranscriptTailPart, TranscriptTailRead,
    TranscriptTailResult, TranscriptTailText, TranscriptTailTool,
};
use rw_types::{SequenceId, SessionId, TurnId};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Citation {
    uri: String,
    #[serde(deserialize_with = "Option::deserialize")]
    title: Option<String>,
}

/// Read one bounded component page at the index's exact applied prefix.
///
/// # Errors
/// Rejects invalid ranges, missing/corrupt cells, unpublished rewind state or wrong row identity.
pub fn read_transcript_tail(
    index: &TranscriptIndex,
    session: &SessionId,
    request: &TranscriptTailRead,
) -> Result<TranscriptTailResult, TranscriptProjectionError> {
    validate_tail_read(request)?;
    let state = super::super::TranscriptProjector::tail_for(index)?;
    let head = index.head()?;
    let view = TranscriptView {
        session_id: session.clone(),
        projection_version: head.version,
        generation: TranscriptGeneration(head.generation),
        through: head.prefix.next_sequence.checked_sub(1).map(SequenceId),
        digest: head.prefix.digest,
    };
    let identity = TranscriptTailIdentity {
        generation: view.generation,
        turn_started: state.turn_started.map(SequenceId),
        response_epoch: view.through.map(|_| SequenceId(state.epoch)),
        tools_epoch: view.through.map(|_| SequenceId(state.tools_epoch)),
    };
    if request
        .expected
        .as_ref()
        .is_some_and(|expected| expected != &identity)
    {
        return Ok(TranscriptTailResult::Changed { view, identity });
    }
    let content = match request.part {
        TranscriptTailPart::Text {} => TranscriptTailContent::Text {
            preview: read_text(index, TEXT_FIRST, state.text_bytes, state.text_truncated)?,
        },
        TranscriptTailPart::Thinking {} => TranscriptTailContent::Thinking {
            preview: read_text(
                index,
                THINKING_FIRST,
                state.thinking_bytes,
                state.thinking_truncated,
            )?,
        },
        TranscriptTailPart::Citations { offset } => TranscriptTailContent::Citations {
            offset,
            items: Vec::with_capacity(usize::from(request.max_items)),
            next_offset: Some(u16::MAX),
        },
        TranscriptTailPart::Tools { offset } => TranscriptTailContent::Tools {
            offset,
            items: Vec::with_capacity(usize::from(request.max_items)),
            next_offset: Some(u16::MAX),
        },
    };
    let mut page = TranscriptTailPage {
        view,
        identity,
        content,
    };
    let mut bytes = encoded_size(&page)?;
    fill_page(index, &state, request, &mut page.content, &mut bytes)?;
    if bytes > request.max_bytes as usize || encoded_size(&page)? > request.max_bytes as usize {
        return Err(invalid("tail encoded page bound"));
    }
    Ok(TranscriptTailResult::Ready { page })
}

fn fill_page(
    index: &TranscriptIndex,
    state: &TailState,
    request: &TranscriptTailRead,
    content: &mut TranscriptTailContent,
    bytes: &mut usize,
) -> Result<(), TranscriptProjectionError> {
    match content {
        TranscriptTailContent::Citations {
            offset,
            items,
            next_offset,
        } => {
            let mut position = usize::from(*offset);
            if position > state.citation_count {
                return Err(invalid("tail citation offset"));
            }
            while position < state.citation_count && items.len() < usize::from(request.max_items) {
                let item = read_citation(index, state, position)?;
                if !admit(&item, bytes, request.max_bytes)? {
                    break;
                }
                items.push(item);
                position += 1;
            }
            *next_offset = (position < state.citation_count).then_some(slot(position)?);
        }
        TranscriptTailContent::Tools {
            offset,
            items,
            next_offset,
        } => {
            let mut position = usize::from(*offset);
            if position > MAX_PENDING_TOOL_INVOCATIONS {
                return Err(invalid("tail tool offset"));
            }
            let metadata = index_cell(
                index,
                TOOL_INDEX,
                state.tools_epoch,
                MAX_PENDING_TOOL_INVOCATIONS,
            )?;
            let count = (0..MAX_PENDING_TOOL_INVOCATIONS)
                .filter(|slot| metadata[8 + slot * 16 + 12] & 1 != 0)
                .count();
            if count != state.tools_count {
                return Err(invalid("tail pending tool count"));
            }
            while position < MAX_PENDING_TOOL_INVOCATIONS
                && items.len() < usize::from(request.max_items)
            {
                let entry = &metadata[8 + position * 16..8 + (position + 1) * 16];
                if entry[12] & 1 != 0 {
                    let item = read_tool(index, position, entry)?;
                    if !admit(&item, bytes, request.max_bytes)? {
                        break;
                    }
                    items.push(item);
                }
                position += 1;
            }
            *next_offset = (position < MAX_PENDING_TOOL_INVOCATIONS).then_some(slot(position)?);
        }
        TranscriptTailContent::Text { .. } | TranscriptTailContent::Thinking { .. } => {}
    }
    Ok(())
}

/// Validate source-owned page admission before entering the blocking read owner.
///
/// # Errors
/// Rejects requests outside the item or encoded-byte limits.
pub fn validate_tail_read(request: &TranscriptTailRead) -> Result<(), TranscriptProjectionError> {
    if !(1..=TRANSCRIPT_TAIL_PAGE_ITEMS).contains(&usize::from(request.max_items))
        || !(TRANSCRIPT_TAIL_MIN_PAGE_BYTES..=TRANSCRIPT_TAIL_PAGE_BYTES)
            .contains(&(request.max_bytes as usize))
    {
        return Err(invalid("tail page admission"));
    }
    Ok(())
}
fn read_text(
    index: &TranscriptIndex,
    first: u16,
    bytes: usize,
    truncated: bool,
) -> Result<TranscriptTailText, TranscriptProjectionError> {
    let cells = u16::try_from(bytes.div_ceil(MAX_AUXILIARY_CELL_BYTES))
        .map_err(|_| invalid("tail text extent"))?;
    let value = index.auxiliary_range(first, cells, bytes)?;
    if value.len() != bytes {
        return Err(invalid("tail text cell extent"));
    }
    Ok(TranscriptTailText {
        text: String::from_utf8(value).map_err(|_| invalid("tail utf8"))?,
        truncated,
    })
}
fn read_citation(
    index: &TranscriptIndex,
    state: &TailState,
    position: usize,
) -> Result<TranscriptTailCitation, TranscriptProjectionError> {
    let metadata = index_cell(
        index,
        CITATION_INDEX_FIRST + slot(position / 128)?,
        state.epoch,
        128,
    )?;
    let entry = &metadata[8 + position % 128 * 16..8 + (position % 128 + 1) * 16];
    let source = SequenceId(read_u64(&entry[..8]));
    let start = read_u32(&entry[8..12]) as usize;
    let len = read_u32(&entry[12..16]) as usize;
    let max = rw_types::citation_admission::MAX_CITATION_TEXT_BYTES;
    if len == 0 || len > max * 6 + 128 || start.saturating_add(len) > state.citation_encoded_bytes {
        return Err(invalid("tail citation extent"));
    }
    let leading = start % MAX_AUXILIARY_CELL_BYTES;
    let first = CITATION_DATA_FIRST + slot(start / MAX_AUXILIARY_CELL_BYTES)?;
    let cells = slot((leading + len).div_ceil(MAX_AUXILIARY_CELL_BYTES))?;
    let encoded = index.auxiliary_range(
        first,
        cells,
        (leading + len).div_ceil(MAX_AUXILIARY_CELL_BYTES) * MAX_AUXILIARY_CELL_BYTES,
    )?;
    let encoded = encoded
        .get(leading..leading + len)
        .ok_or_else(|| invalid("tail citation cell range"))?;
    let citation: Citation = serde_json::from_slice(encoded)?;
    if citation
        .uri
        .len()
        .saturating_add(citation.title.as_ref().map_or(0, String::len))
        > max
    {
        return Err(invalid("tail citation decoded bytes"));
    }
    Ok(TranscriptTailCitation {
        source,
        uri: citation.uri,
        title: citation.title,
    })
}
fn read_tool(
    index: &TranscriptIndex,
    slot: usize,
    entry: &[u8],
) -> Result<TranscriptTailTool, TranscriptProjectionError> {
    let source = SequenceId(read_u64(&entry[..8]));
    let row = index
        .row(&format!("item:{}", source.0))?
        .ok_or_else(|| invalid("tail invocation source"))?;
    let turn = row
        .agent_turn
        .ok_or_else(|| invalid("tail invocation turn"))?;
    if row.source != source {
        return Err(invalid("tail invocation row source"));
    }
    let TranscriptContent::Tool {
        invocation_id,
        name,
        call_index,
        arguments,
        diff,
        status: TranscriptToolStatus::Running {},
    } = super::super::decode(&row)?
    else {
        return Err(invalid("tail invocation row kind"));
    };
    let len =
        usize::try_from(read_u32(&entry[8..12])).map_err(|_| invalid("tail preview extent"))?;
    if len > TRANSCRIPT_TAIL_TOOL_BYTES {
        return Err(invalid("tail invocation preview extent"));
    }
    let output = read_text(
        index,
        TOOL_DATA_FIRST + super::slot(slot)? * TOOL_CELLS,
        len,
        entry[12] & 2 != 0,
    )?;
    Ok(TranscriptTailTool {
        source,
        turn_id: TurnId(turn.to_string()),
        invocation_id,
        name,
        call_index,
        arguments,
        diff,
        output,
    })
}
fn admit(
    value: &impl serde::Serialize,
    bytes: &mut usize,
    maximum: u32,
) -> Result<bool, TranscriptProjectionError> {
    let next = bytes.saturating_add(encoded_size(value)?).saturating_add(1);
    if next > maximum as usize {
        return Ok(false);
    }
    *bytes = next;
    Ok(true)
}
fn encoded_size(value: &impl serde::Serialize) -> Result<usize, TranscriptProjectionError> {
    let mut counter = rw_types::json_encoding::JsonWriter::count(usize::MAX);
    counter.serialize(value)?;
    Ok(counter.written())
}

fn invalid(message: &'static str) -> TranscriptProjectionError {
    TranscriptProjectionError::Invalid(message)
}
