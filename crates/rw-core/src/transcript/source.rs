//! Direct content resolves only input commits already published by the semantic projector.
use super::{TranscriptProjectionError, TranscriptProjector};
use rw_store::session::{
    SessionEventPageLimits,
    journal::JournalReadView,
    transcript_index::{TranscriptIndex, TranscriptIndexError},
};
use rw_types::{EngineEvent, SequenceId};

impl TranscriptProjector {
    /// Read an exact source from the content-bound published projection.
    /// # Errors
    /// Rejects unpublished input commits, changed sources and invalid input bodies.
    pub fn materialize_source(
        index: &TranscriptIndex,
        source: &JournalReadView,
        sequence: SequenceId,
    ) -> Result<EngineEvent, TranscriptProjectionError> {
        Self::checkpoint_for(index)?;
        let head = index.head()?;
        let pinned = source
            .at_prefix(head.prefix)
            .map_err(TranscriptIndexError::from)?;
        if sequence.0 >= head.prefix.next_sequence {
            return Err(TranscriptProjectionError::Invalid(
                "content source is not published",
            ));
        }
        let limits = SessionEventPageLimits::default();
        let event = pinned
            .page::<EngineEvent>(
                sequence.0.checked_sub(1).map(SequenceId),
                SessionEventPageLimits {
                    max_page_events: 1,
                    max_page_bytes: limits.max_line_bytes as u64 + 1,
                    max_scan_bytes: limits.max_line_bytes as u64 * 2,
                    ..limits
                },
            )
            .map_err(TranscriptIndexError::from)?
            .events
            .into_iter()
            .next()
            .ok_or(TranscriptProjectionError::Invalid("missing content source"))?
            .event;
        if event.meta().is_none_or(|meta| meta.sequence_id != sequence) {
            return Err(TranscriptProjectionError::Invalid(
                "content source identity",
            ));
        }
        if matches!(
            event,
            EngineEvent::ConversationInputCommitted { .. }
                | EngineEvent::ConversationContextCommitted { .. }
        ) && index
            .at_or_before_source(sequence)?
            .is_none_or(|row| row.source != sequence)
        {
            return Err(TranscriptProjectionError::Invalid(
                "input source has no published claim",
            ));
        }
        match crate::recovery::materialize_indexed_event(&pinned, &event)
            .map_err(|_| TranscriptProjectionError::Invalid("conversation source"))?
        {
            std::borrow::Cow::Borrowed(_) => Ok(event),
            std::borrow::Cow::Owned(resolved) => Ok(resolved),
        }
    }
}
