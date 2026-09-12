//! Exact search-source bindings are published with their effective semantic rows.
use super::{TranscriptProjectionError, decode};
use rw_store::session::transcript_index::{TranscriptIndex, TranscriptIndexRow};
use rw_types::{
    Block, EngineEvent, Role, SequenceId,
    transcript::{TranscriptContent, TranscriptToolStatus},
};

pub(super) fn search_binding(event: &EngineEvent) -> Option<String> {
    let searchable = match event {
        EngineEvent::ConversationTurnCommitted { turn, .. } => {
            matches!(turn.role, Role::User | Role::Assistant)
                && turn
                    .blocks
                    .iter()
                    .any(|block| matches!(block, Block::Text {text} if !text.is_empty()))
        }
        EngineEvent::ToolCallFinished { .. } => true,
        _ => false,
    };
    if !searchable {
        return None;
    }
    event.meta().map(|meta| binding(meta.sequence_id))
}
fn binding(sequence: SequenceId) -> String {
    format!("search:{}", sequence.0)
}

/// Resolve one exact published search source without reading its original body.
///
/// Bindings are written only by the semantic projector after canonical input claims
/// are validated. Removing a row also makes every binding to it unresolved.
/// # Errors
/// Rejects unpublished/removed sources and mismatched semantic bindings.
pub fn search_source_row(
    index: &TranscriptIndex,
    sequence: SequenceId,
) -> Result<TranscriptIndexRow, TranscriptProjectionError> {
    let row = index
        .bound_row(&binding(sequence))?
        .ok_or(TranscriptProjectionError::Invalid(
            "search match is no longer effective",
        ))?;
    let valid = match decode(&row)? {
        TranscriptContent::Tool {
            status: TranscriptToolStatus::Finished { output, .. },
            ..
        } => output.source.sequence == sequence,
        TranscriptContent::Conversation { role, source, .. } => {
            matches!(role, Role::User | Role::Assistant)
                && row.source == sequence
                && source.sequence == sequence
        }
        _ => false,
    };
    if !valid || row.revision < sequence {
        return Err(TranscriptProjectionError::Invalid(
            "search source does not match semantic row",
        ));
    }
    Ok(row)
}
