//! Exact search-source to effective semantic-row resolution.
use super::{TranscriptProjectionError, decode, entity_binding};
use rw_store::session::transcript_index::{TranscriptIndex, TranscriptIndexRow};
use rw_types::{
    EngineEvent, SequenceId,
    transcript::{TranscriptContent, TranscriptToolStatus},
};

/// Resolve an exact published search source. Removed rows never choose nearby content.
/// # Errors
/// Rejects events without searchable semantic content or mismatched bindings.
pub fn search_source_row(
    index: &TranscriptIndex,
    event: &EngineEvent,
) -> Result<TranscriptIndexRow, TranscriptProjectionError> {
    let sequence = event
        .meta()
        .ok_or(TranscriptProjectionError::Invalid(
            "transient search source",
        ))?
        .sequence_id;
    let row = match event {
        EngineEvent::ConversationTurnCommitted { .. }
        | EngineEvent::ConversationInputCommitted { .. }
        | EngineEvent::ConversationContextCommitted { .. } => {
            index.row(&format!("item:{}", sequence.0))?
        }
        EngineEvent::ToolCallFinished { invocation_id, .. } => {
            index.bound_row(&entity_binding("tool", &[&invocation_id.0]))?
        }
        _ => {
            return Err(TranscriptProjectionError::Invalid(
                "event is not a search document",
            ));
        }
    }
    .ok_or(TranscriptProjectionError::Invalid(
        "search match is no longer effective",
    ))?;
    if !matches_source(&row, event, sequence)? {
        return Err(TranscriptProjectionError::Invalid(
            "search source does not match semantic row",
        ));
    }
    Ok(row)
}

fn matches_source(
    row: &TranscriptIndexRow,
    event: &EngineEvent,
    sequence: SequenceId,
) -> Result<bool, TranscriptProjectionError> {
    Ok(match (decode(row)?, event) {
        (
            TranscriptContent::Tool {
                invocation_id,
                status: TranscriptToolStatus::Finished { output, .. },
                ..
            },
            EngineEvent::ToolCallFinished {
                invocation_id: expected,
                ..
            },
        ) => invocation_id == *expected && output.source.sequence == sequence,
        (
            TranscriptContent::Conversation { .. },
            EngineEvent::ConversationTurnCommitted { .. }
            | EngineEvent::ConversationInputCommitted { .. }
            | EngineEvent::ConversationContextCommitted { .. },
        ) => row.source == sequence,
        _ => false,
    })
}
