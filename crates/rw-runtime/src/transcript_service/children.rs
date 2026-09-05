//! Mode-independent active-child reads share the transcript ancestry and allocation owners.
use super::{OwnedTranscriptRead, ProjectionBudget, TranscriptReader, page::storage};
use rw_core::{HostError, recovery::SubagentLifecycleIndex};
use rw_types::{
    SessionId,
    session_children::{SessionChildState, SessionChildrenResult, SessionChildrenSnapshot},
    session_read::SessionReadScope,
};
use std::sync::Arc;

impl TranscriptReader {
    /// Read every effective active child without initializing an actor or extension graph.
    /// # Errors
    /// Rejects unsafe sources, invalid ancestry and exhausted read admission.
    pub async fn children(
        self: &Arc<Self>,
        session: SessionId,
        scope: SessionReadScope,
    ) -> Result<OwnedTranscriptRead<SessionChildrenResult>, HostError> {
        SessionId::validate(&session.0).map_err(storage)?;
        self.blocking_owned(move |reader| reader.read_children(&session, &scope))
            .await
    }
    pub(crate) fn read_children(
        &self,
        session: &SessionId,
        scope: &SessionReadScope,
    ) -> Result<SessionChildrenResult, HostError> {
        let mut budget = ProjectionBudget::new();
        self.authorize_scope(session, scope, &mut budget)?;
        let source = self.journals.capture(&session.0).map_err(storage)?;
        let mut index = SubagentLifecycleIndex::open(&source.view).map_err(storage)?;
        let ready = index.is_current(&source.view).map_err(storage)?;
        let mut complete = ready;
        while !complete && budget.take_batch() {
            complete = !index.advance(&source.view).map_err(storage)?;
        }
        if !complete {
            return Ok(SessionChildrenResult::CatchingUp {
                through: index.through().map_err(storage)?,
                target: source.view.last_sequence(),
            });
        }
        let view = index.snapshot(&source.view).map_err(storage)?;
        let children = view
            .active_children()
            .map_err(storage)?
            .into_iter()
            .map(|binding| SessionChildState {
                subagent_id: binding.subagent_id,
                child_session_id: binding.session_id,
                spawned: binding.spawned,
                spawned_turn: binding.spawned_turn,
                task_preview: binding.task_preview,
                task_truncated: binding.task_truncated,
            })
            .collect();
        let snapshot = SessionChildrenSnapshot {
            through: view.through(),
            children,
        };
        snapshot.validate().map_err(storage)?;
        Ok(SessionChildrenResult::Ready { snapshot })
    }
}
