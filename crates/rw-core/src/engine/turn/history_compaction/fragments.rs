//! Retain one source encoder through every summary continuation, never re-read its prefix.
use crate::engine::{
    AgentLoopError,
    recovery::{
        ConversationFragmentCursor, ConversationFragmentSource, HistoryRead,
        MAX_SUMMARY_FRAGMENT_BYTES, SessionHistoryView,
    },
};
use rw_types::{SequenceId, Turn};
use std::sync::Arc;

pub(super) struct PendingFragments {
    source: HistoryRead<ConversationFragmentSource>,
    cursor: ConversationFragmentCursor,
}
pub(super) async fn append(
    pending: &mut Option<PendingFragments>,
    history: &Arc<dyn SessionHistoryView>,
    next: &mut u64,
    expected: SequenceId,
    tokens: u64,
    carry: &mut Vec<Turn>,
) -> Result<HistoryRead<()>, AgentLoopError> {
    let mut active = if let Some(active) = pending.take() {
        active
    } else {
        PendingFragments {
            source: history.conversation_fragment_source(*next).await?,
            cursor: ConversationFragmentCursor {
                ordinal: *next,
                block_index: 0,
                byte_offset: 0,
            },
        }
    };
    let bytes = usize::try_from(tokens.saturating_sub(16).saturating_mul(4))
        .unwrap_or(usize::MAX)
        .min(MAX_SUMMARY_FRAGMENT_BYTES);
    let result = active
        .source
        .fragment(active.cursor, bytes)
        .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
    if result.source.sequence != expected {
        return Err(AgentLoopError::InvalidConfiguration(
            "fragment source changed".into(),
        ));
    }
    if let Some(turn) = result.turn {
        carry.push(turn);
    }
    if let Some(cursor) = result.next {
        active.cursor = cursor;
        *pending = Some(active);
        Ok(HistoryRead::new((), ()))
    } else {
        *next = next.checked_add(1).ok_or_else(|| {
            AgentLoopError::InvalidConfiguration("fragment source ordinal overflow".into())
        })?;
        // The final provider call keeps the source's allocation owner too.
        Ok(active.source.map(|_| ()))
    }
}
