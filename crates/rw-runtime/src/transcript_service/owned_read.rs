//! Decoded read ownership transfers only when a consumer releases it or encodes a reply.
use super::TranscriptReader;
use rw_core::{HostError, HostReadResult};
use std::sync::Arc;
use tokio::sync::OwnedSemaphorePermit;

pub struct OwnedTranscriptRead<T> {
    value: T,
    permit: OwnedSemaphorePermit,
}
impl<T> OwnedTranscriptRead<T> {
    #[must_use]
    pub fn value(&self) -> &T {
        &self.value
    }
    #[must_use]
    pub fn into_query(self, event: impl FnOnce(T) -> rw_types::EngineEvent) -> HostReadResult {
        HostReadResult::new(
            rw_types::CommandOutcome::Accepted {},
            vec![event(self.value)],
            self.permit,
        )
    }
}
impl TranscriptReader {
    pub(crate) async fn blocking_owned<T, F>(
        self: &Arc<Self>,
        operation: F,
    ) -> Result<OwnedTranscriptRead<T>, HostError>
    where
        T: Send + 'static,
        F: FnOnce(&Self) -> Result<T, HostError> + Send + 'static,
    {
        let permit = Arc::clone(&self.workers)
            .try_acquire_owned()
            .map_err(|_| HostError::Query("transcript worker admission is exhausted".into()))?;
        let reader = Arc::clone(self);
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            let value = operation(&reader)?;
            Ok(OwnedTranscriptRead { value, permit })
        })
        .await
        .map_err(|_| HostError::Query("transcript worker failed".into()))?
    }
}
