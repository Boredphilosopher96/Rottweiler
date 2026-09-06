//! Exact, bounded action authority from the effective invocation binding.

use super::{ProjectionRead, TranscriptReader, page::storage};
use rw_core::{HostError, transcript::finished_tool_source};
use rw_store::session::SessionEventPageLimits;
use rw_types::{
    EngineEvent, SequenceId, SessionId, ToolInvocationId, extension_ui::UiPresentation,
};
use std::sync::Arc;

impl TranscriptReader {
    /// Resolve an action's completed tool presentation at the actor's exact durable prefix.
    /// The caller authorizes the session and holds serialized actor dispatch.
    ///
    /// # Errors
    /// Rejects stale prefixes, incomplete projection, busy admission and corrupt source identity.
    pub async fn tool_presentation(
        self: &Arc<Self>,
        session: SessionId,
        invocation: ToolInvocationId,
        expected_through: Option<SequenceId>,
    ) -> Result<Option<UiPresentation>, HostError> {
        self.blocking(move |reader| {
            match reader.projected(&session, |index, journal| {
                if journal.last_sequence() != expected_through {
                    return Err(HostError::Protocol(
                        "tool action source prefix changed".into(),
                    ));
                }
                let Some(source) = finished_tool_source(index, &invocation).map_err(storage)?
                else {
                    return Ok(None);
                };
                let limits = SessionEventPageLimits::default();
                let page = journal
                    .page::<EngineEvent>(
                        source.0.checked_sub(1).map(SequenceId),
                        SessionEventPageLimits {
                            max_page_events: 1,
                            max_page_bytes: limits.max_line_bytes as u64 + 1,
                            max_scan_bytes: limits.max_line_bytes as u64 * 2,
                            ..limits
                        },
                    )
                    .map_err(storage)?;
                match page.events.into_iter().next() {
                    Some(envelope) if envelope.sequence == source => match envelope.event {
                        EngineEvent::ToolCallFinished {
                            meta,
                            invocation_id,
                            presentation,
                            ..
                        } if meta.sequence_id == source
                            && meta.session_id == session
                            && invocation_id == invocation =>
                        {
                            Ok(presentation)
                        }
                        _ => Err(HostError::Persistence(
                            "tool presentation source identity is invalid".into(),
                        )),
                    },
                    _ => Err(HostError::Persistence(
                        "tool presentation source is missing".into(),
                    )),
                }
            })? {
                ProjectionRead::Ready(presentation) => Ok(presentation),
                ProjectionRead::CatchingUp { .. } => Err(HostError::Query(
                    "tool presentation is catching up; retry the action".into(),
                )),
            }
        })
        .await
    }
}
