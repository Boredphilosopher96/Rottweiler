use super::{
    TOOL_CELLS, TOOL_DATA_FIRST, TOOL_INDEX, TOOL_PROVIDER_FIRST, TailState,
    TranscriptIndexMutation, TranscriptProjectionError, TranscriptRowLookup, chunks, index_cell,
    read_u32, read_u64,
};
use rw_types::tool_admission::MAX_PENDING_TOOL_INVOCATIONS;
use rw_types::transcript_tail::TRANSCRIPT_TAIL_TOOL_BYTES;
use rw_types::{EngineEvent, ToolOutputStream};
use std::io::Write as _;

pub(super) fn project(
    event: &EngineEvent,
    state: &mut TailState,
    rows: &impl TranscriptRowLookup,
) -> Result<Vec<TranscriptIndexMutation>, TranscriptProjectionError> {
    let mut cell = index_cell(
        rows,
        TOOL_INDEX,
        state.tools_epoch,
        MAX_PENDING_TOOL_INVOCATIONS,
    )?;
    if let EngineEvent::ToolCallStarted {
        meta, tool_call_id, ..
    } = event
    {
        return start(state, cell, meta.sequence_id.0, tool_call_id);
    }
    let (EngineEvent::ToolCallFinished {
        invocation_id: invocation,
        ..
    }
    | EngineEvent::ToolOutputDelta {
        invocation_id: invocation,
        ..
    }) = event
    else {
        return Err(TranscriptProjectionError::Invalid("tail tool event"));
    };
    let row = rows
        .bound_row(&super::super::entity_binding("tool", &[&invocation.0]))?
        .ok_or(TranscriptProjectionError::Invalid("tail tool source"))?;
    let slot = (0..MAX_PENDING_TOOL_INVOCATIONS)
        .find(|slot| {
            let entry = 8 + slot * 16;
            cell[entry + 12] & 1 != 0 && read_u64(&cell[entry..entry + 8]) == row.source.0
        })
        .ok_or(TranscriptProjectionError::Invalid("tail active invocation"))?;
    let entry = 8 + slot * 16;
    if matches!(event, EngineEvent::ToolCallFinished { .. }) {
        cell[entry..entry + 16].fill(0);
        state.tools_count = state
            .tools_count
            .checked_sub(1)
            .ok_or(TranscriptProjectionError::Invalid("tail tool count"))?;
        return Ok(vec![TranscriptIndexMutation::PutAuxiliary {
            key: TOOL_INDEX,
            payload: cell,
        }]);
    }
    let EngineEvent::ToolOutputDelta { stream, chunk, .. } = event else {
        return Err(TranscriptProjectionError::Invalid("tail output"));
    };
    if chunk.is_empty() || cell[entry + 12] & 2 != 0 {
        return Ok(Vec::new());
    }
    let bytes = read_u32(&cell[entry + 8..entry + 12]) as usize;
    let mut writer = chunks::CellAppender::new(
        TOOL_DATA_FIRST + super::slot(slot)? * TOOL_CELLS,
        bytes,
        TRANSCRIPT_TAIL_TOOL_BYTES,
        rows,
    )?;
    let stream_id = match stream {
        ToolOutputStream::Stdout => 1,
        ToolOutputStream::Stderr => 2,
    };
    let label = if stream_id == cell[entry + 13] || (cell[entry + 13] == 0 && stream_id == 1) {
        ""
    } else if stream_id == 1 {
        "\n[stdout]\n"
    } else {
        "\n[stderr]\n"
    };
    let label = chunks::utf8_prefix(label, TRANSCRIPT_TAIL_TOOL_BYTES.saturating_sub(bytes));
    writer
        .write_all(label.as_bytes())
        .map_err(|_| TranscriptProjectionError::Invalid("tool preview label"))?;
    let prefix = chunks::utf8_prefix(
        chunk,
        TRANSCRIPT_TAIL_TOOL_BYTES.saturating_sub(bytes + label.len()),
    );
    writer
        .write_all(prefix.as_bytes())
        .map_err(|_| TranscriptProjectionError::Invalid("tool preview bytes"))?;
    let (next, mut mutations) = writer.finish()?;
    cell[entry + 8..entry + 12].copy_from_slice(
        &u32::try_from(next)
            .map_err(|_| TranscriptProjectionError::Invalid("tool preview extent"))?
            .to_le_bytes(),
    );
    cell[entry + 12] = 1 | if prefix.len() < chunk.len() { 2 } else { 0 };
    cell[entry + 13] = stream_id;
    mutations.push(TranscriptIndexMutation::PutAuxiliary {
        key: TOOL_INDEX,
        payload: cell,
    });
    Ok(mutations)
}

fn start(
    state: &mut TailState,
    mut cell: Vec<u8>,
    source: u64,
    tool_call_id: &rw_types::ToolCallId,
) -> Result<Vec<TranscriptIndexMutation>, TranscriptProjectionError> {
    if tool_call_id.0.len() > rw_types::tool_admission::MAX_TOOL_CALL_ID_BYTES {
        return Err(TranscriptProjectionError::Invalid(
            "tail provider identifier bound",
        ));
    }
    let slot = (0..MAX_PENDING_TOOL_INVOCATIONS)
        .find(|slot| cell[8 + slot * 16 + 12] & 1 == 0)
        .ok_or(TranscriptProjectionError::Invalid(
            "tail pending tool admission",
        ))?;
    let entry = 8 + slot * 16;
    cell[entry..entry + 16].fill(0);
    cell[entry..entry + 8].copy_from_slice(&source.to_le_bytes());
    cell[entry + 12] = 1;
    state.tools_count += 1;
    Ok(vec![
        TranscriptIndexMutation::PutAuxiliary {
            key: TOOL_INDEX,
            payload: cell,
        },
        TranscriptIndexMutation::PutAuxiliary {
            key: TOOL_PROVIDER_FIRST + super::slot(slot)?,
            payload: tool_call_id.0.as_bytes().to_vec(),
        },
    ])
}
