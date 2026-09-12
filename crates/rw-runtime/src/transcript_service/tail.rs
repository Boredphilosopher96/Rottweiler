//! Live display reads share transcript projection and ancestry admission.
use super::{
    OwnedTranscriptRead, ProjectionBudget, ProjectionRead, TranscriptReader, page::storage,
};
use rw_core::{
    HostError,
    transcript::{read_transcript_tail, validate_tail_read},
};
use rw_types::{
    SessionId,
    session_read::SessionReadScope,
    transcript_tail::{TranscriptTailRead, TranscriptTailResult},
};
use std::sync::Arc;
impl TranscriptReader {
    /// Read one bounded in-progress display component without starting a session.
    ///
    /// # Errors
    /// Rejects unsafe storage, invalid ancestry, exhausted read admission or corrupt sources.
    pub async fn tail(
        self: &Arc<Self>,
        session: SessionId,
        scope: SessionReadScope,
        request: TranscriptTailRead,
    ) -> Result<OwnedTranscriptRead<TranscriptTailResult>, HostError> {
        validate_tail_read(&request).map_err(storage)?;
        self.blocking_owned(move |reader| reader.read_tail(&session, &scope, &request))
            .await
    }
    pub(crate) fn read_tail(
        &self,
        session: &SessionId,
        scope: &SessionReadScope,
        request: &TranscriptTailRead,
    ) -> Result<TranscriptTailResult, HostError> {
        validate_tail_read(request).map_err(storage)?;
        let mut budget = ProjectionBudget::new();
        self.authorize_scope(session, scope, &mut budget)?;
        match self.projected_with_budget(session, &mut budget, |index, _journal| {
            read_transcript_tail(index, session, request).map_err(storage)
        })? {
            ProjectionRead::Ready(result) => Ok(result),
            ProjectionRead::CatchingUp { through, target } => {
                Ok(TranscriptTailResult::CatchingUp { through, target })
            }
        }
    }
}
