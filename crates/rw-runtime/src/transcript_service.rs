//! Bounded runtime ownership for the rebuildable semantic transcript projection.

use crate::journal_service::JournalService;
use rw_core::{HostError, transcript::TranscriptProjector};
use rw_types::{
    SequenceId, SessionId,
    transcript::{TranscriptRead, TranscriptReadResult},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

mod authority;
mod content;
use crate::projection_budget::ProjectionBudget;
mod page;
pub(crate) mod reader;
mod tool_presentation;

const MAX_OPEN_PROJECTORS: usize = 8;

type ProjectorOwner = Arc<Mutex<Option<TranscriptProjector>>>;
struct CachedProjector {
    owner: ProjectorOwner,
    touched: u64,
}

/// Shared bounded transcript projection, document cache and blocking-worker owner.
pub struct TranscriptReader {
    journals: Arc<JournalService>,
    projectors: Mutex<HashMap<SessionId, CachedProjector>>,
    clock: AtomicU64,
    documents: Mutex<content::DocumentCache>,
    workers: Arc<tokio::sync::Semaphore>,
}

impl TranscriptReader {
    pub(crate) fn new(journals: Arc<JournalService>) -> Arc<Self> {
        Arc::new(Self {
            journals,
            projectors: Mutex::new(HashMap::new()),
            clock: AtomicU64::new(0),
            documents: Mutex::new(content::DocumentCache::default()),
            workers: Arc::new(tokio::sync::Semaphore::new(MAX_OPEN_PROJECTORS)),
        })
    }

    /// Admission stays with the blocking worker when its awaiting request is cancelled.
    pub(crate) async fn blocking<R, F>(self: &Arc<Self>, operation: F) -> Result<R, HostError>
    where
        R: Send + 'static,
        F: FnOnce(&Self) -> Result<R, HostError> + Send + 'static,
    {
        let permit = Arc::clone(&self.workers)
            .try_acquire_owned()
            .map_err(|_| HostError::Query("transcript worker admission is exhausted".into()))?;
        let service = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            operation(&service)
        })
        .await
        .map_err(|_| HostError::Query("transcript worker failed".into()))?
    }

    fn projector(&self, session: &SessionId) -> Result<ProjectorOwner, HostError> {
        let touched = self.clock.fetch_add(1, Ordering::Relaxed);
        let (owner, evicted) = {
            let mut cache = self
                .projectors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = cache.get_mut(session) {
                entry.touched = touched;
                return Ok(Arc::clone(&entry.owner));
            }
            let evicted = if cache.len() >= MAX_OPEN_PROJECTORS {
                let key = cache
                    .iter()
                    .filter(|(_, entry)| Arc::strong_count(&entry.owner) == 1)
                    .min_by_key(|(_, entry)| entry.touched)
                    .map(|(key, _)| key.clone())
                    .ok_or_else(|| {
                        HostError::Query("transcript projector admission is exhausted".into())
                    })?;
                cache.remove(&key)
            } else {
                None
            };
            let owner = Arc::new(Mutex::new(None));
            cache.insert(
                session.clone(),
                CachedProjector {
                    owner: Arc::clone(&owner),
                    touched,
                },
            );
            (owner, evicted)
        };
        // Closing an evicted redb owner can flush; never do it under the registry lock.
        drop(evicted);
        Ok(owner)
    }

    /// Called by a bounded blocking query worker after workspace authorization.
    pub(crate) fn read(
        &self,
        session: &SessionId,
        scope: &rw_types::session_read::SessionReadScope,
        request: &TranscriptRead,
    ) -> Result<TranscriptReadResult, HostError> {
        page::limits(request)?;
        let mut budget = ProjectionBudget::new();
        self.authorize_scope(session, scope, &mut budget)?;
        match self.projected_with_budget(session, &mut budget, |index, journal| {
            page::read(index, journal, session, request)
        })? {
            ProjectionRead::Ready(result) => Ok(result),
            ProjectionRead::CatchingUp { through, target } => {
                Ok(TranscriptReadResult::CatchingUp { through, target })
            }
        }
    }

    pub(crate) fn read_content(
        &self,
        session: &SessionId,
        scope: &rw_types::session_read::SessionReadScope,
        request: &rw_types::transcript::TranscriptContentRead,
    ) -> Result<rw_types::transcript::TranscriptContentPage, HostError> {
        content::validate(session, request)?;
        let mut budget = ProjectionBudget::new();
        self.authorize_scope(session, scope, &mut budget)?;
        match self.projected_with_budget(session, &mut budget, |index, journal| {
            content::read(&self.documents, index, journal, request)
        })? {
            ProjectionRead::Ready(page) => Ok(page),
            ProjectionRead::CatchingUp { .. } => Err(HostError::Query(
                "transcript is catching up; retry content read".into(),
            )),
        }
    }

    fn projected<R>(
        &self,
        session: &SessionId,
        operation: impl FnOnce(
            &rw_store::session::transcript_index::TranscriptIndex,
            &rw_store::session::journal::JournalReadView,
        ) -> Result<R, HostError>,
    ) -> Result<ProjectionRead<R>, HostError> {
        self.projected_with_budget(session, &mut ProjectionBudget::new(), operation)
    }

    fn projected_with_budget<R>(
        &self,
        session: &SessionId,
        budget: &mut ProjectionBudget,
        operation: impl FnOnce(
            &rw_store::session::transcript_index::TranscriptIndex,
            &rw_store::session::journal::JournalReadView,
        ) -> Result<R, HostError>,
    ) -> Result<ProjectionRead<R>, HostError> {
        SessionId::validate(&session.0).map_err(page::storage)?;
        let owner = self.projector(session)?;
        let mut slot = owner
            .try_lock()
            .map_err(|_| HostError::Query("transcript session is busy".into()))?;
        // Capture after entering the session owner: a queued older read cannot roll it back.
        let journal = self.journals.capture(&session.0).map_err(page::storage)?;
        if slot.is_none() {
            let opened = match TranscriptProjector::open(&journal.view) {
                Err(rw_core::transcript::TranscriptProjectionError::Index(
                    rw_store::session::transcript_index::TranscriptIndexError::IncompatibleVersion { .. }
                )) => TranscriptProjector::rebuild(&journal.view),
                result => result,
            };
            *slot = Some(opened.map_err(page::storage)?);
        }
        let projector = slot
            .as_mut()
            .ok_or_else(|| HostError::Query("transcript projector missing".into()))?;
        let current = projector.index().head().map_err(page::storage)?;
        if !current.rebuilding && current.prefix == journal.view.prefix_identity() {
            return operation(projector.index(), &journal.view).map(ProjectionRead::Ready);
        }
        while budget.take_batch() {
            let progress = projector.advance(&journal.view).map_err(page::storage)?;
            if !progress.has_more && !progress.rebuilding {
                return operation(projector.index(), &journal.view).map(ProjectionRead::Ready);
            }
        }
        let head = projector.index().head().map_err(page::storage)?;
        Ok(ProjectionRead::CatchingUp {
            through: head.prefix.next_sequence.checked_sub(1).map(SequenceId),
            target: journal
                .view
                .prefix_identity()
                .next_sequence
                .checked_sub(1)
                .map(SequenceId),
        })
    }
}

enum ProjectionRead<R> {
    Ready(R),
    CatchingUp {
        through: Option<SequenceId>,
        target: Option<SequenceId>,
    },
}

#[cfg(test)]
mod tests;
