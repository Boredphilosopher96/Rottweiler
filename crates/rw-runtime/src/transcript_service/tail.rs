//! Live display reads share transcript projection and ancestry admission.
use super::{ProjectionBudget, ProjectionRead, TranscriptReader, page::storage};
use rw_core::{
    HostError, HostReadResult,
    transcript::{read_transcript_tail, validate_tail_read},
};
use rw_types::{
    SessionId,
    session_read::SessionReadScope,
    transcript_tail::{TranscriptTailRead, TranscriptTailResult},
};
use std::sync::Arc;
use tokio::sync::OwnedSemaphorePermit;

/// A decoded tail keeps its admitted worker slot until consumption or reply encoding.
pub struct OwnedTranscriptTail {
    result: TranscriptTailResult,
    permit: OwnedSemaphorePermit,
}
impl OwnedTranscriptTail {
    #[must_use]
    pub fn value(&self) -> &TranscriptTailResult {
        &self.result
    }
    #[must_use]
    pub fn into_query(
        self,
        event: impl FnOnce(TranscriptTailResult) -> rw_types::EngineEvent,
    ) -> HostReadResult {
        HostReadResult::new(
            rw_types::CommandOutcome::Accepted {},
            vec![event(self.result)],
            self.permit,
        )
    }
}

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
    ) -> Result<OwnedTranscriptTail, HostError> {
        validate_tail_read(&request).map_err(storage)?;
        let permit = Arc::clone(&self.workers)
            .try_acquire_owned()
            .map_err(|_| HostError::Query("transcript worker admission is exhausted".into()))?;
        let reader = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let result = reader.read_tail(&session, &scope, &request)?;
            Ok(OwnedTranscriptTail { result, permit })
        })
        .await
        .map_err(|_| HostError::Query("transcript worker failed".into()))?
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
