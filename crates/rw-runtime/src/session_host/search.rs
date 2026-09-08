//! Session search shares posting work and keeps physical cancellation with SQLite.
use super::{
    HostError, RuntimeSessionFactory, SESSION_INDEX_SEARCH_MAX_ATTEMPTS,
    SESSION_INDEX_SEARCH_RETRY_DELAY, SessionIndex, SessionStoreError, load_session_metadata_any,
    workspace_name,
};
use rw_store::session::{SessionIndexReadControl, SessionSummary};
use rw_types::{
    ModelAlias, SequenceId, SessionDescriptor, SessionId,
    session_search::{SessionSearchHit, SessionSearchMatch},
};

enum SearchFailure {
    Index(SessionStoreError),
    Metadata(HostError),
}
impl From<SessionStoreError> for SearchFailure {
    fn from(error: SessionStoreError) -> Self {
        Self::Index(error)
    }
}

struct SearchWaiter(SessionIndexReadControl);
impl Drop for SearchWaiter {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl RuntimeSessionFactory {
    fn search_descriptor(
        &self,
        summary: &SessionSummary,
    ) -> Result<Option<SessionDescriptor>, HostError> {
        let metadata =
            load_session_metadata_any(&self.options.storage_root, &summary.id).map_err(|_| {
                HostError::Persistence("session metadata is unavailable or invalid".into())
            })?;
        let workspace = std::fs::canonicalize(&metadata.workspace)
            .map_err(|_| HostError::Query("session workspace is unavailable".into()))?;
        if !self.workspace_is_allowed(&workspace) {
            return Ok(None);
        }
        Ok(Some(SessionDescriptor {
            session_id: SessionId(summary.id.clone()),
            title: summary.title.clone(),
            workspace_name: workspace_name(&workspace),
            model: ModelAlias(metadata.model_alias),
            driver_client_id: None,
            shell_active: false,
        }))
    }
    fn search_sessions_blocking(
        &self,
        query: &str,
        limit: u32,
        control: &SessionIndexReadControl,
    ) -> Result<(Vec<SessionSearchHit>, bool), SearchFailure> {
        let requested = usize::try_from(limit)
            .map_err(|_| SearchFailure::Index(SessionStoreError::SearchLimitTooLarge))?;
        let rows = SessionIndex::search_selected_read_only(
            &self.options.storage_root,
            query,
            requested.saturating_add(1),
            control,
            |summary| {
                control.check().map_err(SearchFailure::Index)?;
                self.search_descriptor(summary)
                    .map_err(SearchFailure::Metadata)
            },
        )?;
        let truncated = rows.len() > requested;
        let mut hits = Vec::with_capacity(rows.len().min(requested));
        for (row, session) in rows.into_iter().take(requested) {
            control.check().map_err(SearchFailure::Index)?;
            let matched = if let Some(sequence) = row.sequence {
                let through = row
                    .source
                    .next_sequence
                    .checked_sub(1)
                    .map(SequenceId)
                    .ok_or(SearchFailure::Index(
                        SessionStoreError::CorruptProjectionWatermark,
                    ))?;
                Some(SessionSearchMatch {
                    session_id: session.session_id.clone(),
                    source_sequence: sequence,
                    through,
                    digest: row.source.digest,
                })
            } else {
                None
            };
            hits.push(SessionSearchHit {
                session,
                r#match: matched,
            });
        }
        control.check().map_err(SearchFailure::Index)?;
        Ok((hits, truncated))
    }
    pub(super) async fn search_sessions_with_retry(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<(Vec<SessionSearchHit>, bool), HostError> {
        let waiter = SearchWaiter(SessionIndexReadControl::new());
        for attempt in 1..=SESSION_INDEX_SEARCH_MAX_ATTEMPTS {
            let factory = self.clone();
            let query = query.to_owned();
            let control = waiter.0.clone();
            let result =
                rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
                    factory.search_sessions_blocking(&query, limit, &control)
                })
                .await
                .map_err(|_| HostError::Query("session search worker failed".into()))?;
            match result {
                Ok((rows, _)) if rows.is_empty() && attempt < SESSION_INDEX_SEARCH_MAX_ATTEMPTS => {
                    tokio::time::sleep(SESSION_INDEX_SEARCH_RETRY_DELAY).await
                }
                Ok(result) => return Ok(result),
                Err(SearchFailure::Index(SessionStoreError::UnsafeSessionIndex))
                    if attempt < SESSION_INDEX_SEARCH_MAX_ATTEMPTS =>
                {
                    tokio::time::sleep(SESSION_INDEX_SEARCH_RETRY_DELAY).await
                }
                Err(SearchFailure::Metadata(error)) => return Err(error),
                Err(SearchFailure::Index(error)) => {
                    tracing::warn!(reason=%error,attempt,"hosted session index search failed");
                    return Err(HostError::Query(format!(
                        "session index search failed: {error}"
                    )));
                }
            }
        }
        Err(HostError::Query("session index search failed".into()))
    }
}

#[cfg(test)]
mod tests;
