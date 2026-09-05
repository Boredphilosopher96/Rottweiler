use super::{TranscriptReader, page::storage};
use crate::journal_reads::JournalReads;
use rw_core::HostError;
use rw_types::{
    EngineEvent, SequenceId, SessionId,
    transcript::{
        TranscriptContentPage, TranscriptContentRead, TranscriptRead, TranscriptReadResult,
    },
};
use std::{path::Path, sync::Arc};

/// Initial durable header and captured history tail; this is not a replay receipt.
pub struct TranscriptBootstrap {
    pub created: Option<EngineEvent>,
    pub through_sequence: Option<SequenceId>,
}

impl TranscriptReader {
    /// Open a descriptor-bound read owner for offline session journals.
    ///
    /// # Errors
    /// Rejects an unsafe or unavailable storage root.
    pub fn open(storage_root: &Path) -> Result<Arc<Self>, HostError> {
        Ok(Self::new(JournalReads::new(storage_root).map_err(storage)?))
    }

    /// Read a bounded current-effective transcript page without starting a session.
    ///
    /// # Errors
    /// Rejects invalid ranges, busy admission, or corrupt/unsafe storage.
    pub async fn page(
        self: &Arc<Self>,
        session: SessionId,
        request: TranscriptRead,
    ) -> Result<TranscriptReadResult, HostError> {
        super::page::limits(&request)?;
        self.blocking(move |reader| reader.read(&session, &request))
            .await
    }

    /// Read a bounded UTF-8 slice of a canonical content source.
    ///
    /// # Errors
    /// Rejects invalid or stale source identities, busy admission, or unsafe storage.
    pub async fn content(
        self: &Arc<Self>,
        session: SessionId,
        request: TranscriptContentRead,
    ) -> Result<TranscriptContentPage, HostError> {
        super::content::validate(&session, &request)?;
        self.blocking(move |reader| reader.read_content(&session, &request))
            .await
    }

    /// Read only the initial source event and journal tail for historical readiness.
    ///
    /// # Errors
    /// Rejects invalid session identity, busy admission, or corrupt/unsafe storage.
    pub async fn bootstrap(
        self: &Arc<Self>,
        session: SessionId,
    ) -> Result<TranscriptBootstrap, HostError> {
        SessionId::validate(&session.0).map_err(storage)?;
        self.blocking(move |reader| {
            let journal = reader.journals.capture(&session.0).map_err(storage)?;
            let limits = rw_store::session::SessionEventPageLimits::default();
            let page = journal
                .view
                .page::<EngineEvent>(
                    None,
                    rw_store::session::SessionEventPageLimits {
                        max_page_events: 1,
                        max_page_bytes: limits.max_line_bytes as u64 + 1,
                        max_scan_bytes: limits.max_line_bytes as u64 * 2,
                        ..limits
                    },
                )
                .map_err(storage)?;
            let created = if let Some(envelope) = page.events.into_iter().next() {
                if envelope.event.meta().is_none_or(|meta| {
                    meta.session_id != session
                        || meta.sequence_id != envelope.sequence
                        || meta.protocol_version != rw_types::PROTOCOL_VERSION
                }) {
                    return Err(HostError::Persistence(
                        "historical header source identity is invalid".into(),
                    ));
                }
                matches!(envelope.event, EngineEvent::SessionCreated { .. })
                    .then_some(envelope.event)
            } else {
                None
            };
            Ok(TranscriptBootstrap {
                created,
                through_sequence: journal.view.last_sequence(),
            })
        })
        .await
    }
}
