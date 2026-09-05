//! Child lifecycle reads share the actor's owned workers and application source admission.
use super::DurableEventSink;
use async_trait::async_trait;
use rw_core::{
    AgentLoopError, OrchestrationError, SubagentArtifactSource,
    recovery::{RecoveryError, SubagentBinding, SubagentLifecycleIndex, SubagentLifecycleView},
};
use rw_tools::{AuthorizedDiffArtifact, DiffArtifactAuthority, ToolError};
use rw_types::{SequenceId, SessionId, SubagentId, SubagentResult};
use std::sync::{Arc, Mutex};

pub(in crate::session_runtime) struct ChildLifecycleReader {
    sink: Arc<DurableEventSink>,
    order: Arc<Mutex<()>>,
}
impl ChildLifecycleReader {
    pub(in crate::session_runtime) fn new(sink: Arc<DurableEventSink>) -> Arc<Self> {
        Arc::new(Self {
            sink,
            order: Arc::new(Mutex::new(())),
        })
    }
    pub(in crate::session_runtime) async fn open_sink(
        &self,
        root: &std::path::Path,
        session: &SessionId,
    ) -> Result<Arc<DurableEventSink>, AgentLoopError> {
        SessionId::validate(&session.0).map_err(persistence)?;
        let root = root.to_path_buf();
        let session = session.0.clone();
        let journal = Arc::clone(&self.sink.journal_service);
        self.sink
            .reads
            .run((root, session, journal), |(root, session, journal)| {
                let log =
                    rw_store::session::SessionEventLog::open(root.as_path(), session.as_str())
                        .map_err(persistence)?;
                DurableEventSink::new(log, root.clone(), session.clone(), Arc::clone(journal))
                    .map_err(persistence)
            })
            .await
    }
    async fn query<T: Send + 'static>(
        &self,
        session: &SessionId,
        query: impl FnOnce(
            &SubagentLifecycleView,
            &crate::journal_service::JournalReadLease,
        ) -> Result<T, RecoveryError>
        + Send
        + 'static,
    ) -> Result<T, AgentLoopError> {
        SessionId::validate(&session.0)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        let session = session.0.clone();
        let admission = self
            .sink
            .journal_service
            .admit_read()
            .map_err(persistence)?;
        let order = Arc::clone(&self.order);
        self.sink
            .reads
            .run((Some(admission), order), move |(admission, order)| {
                let _order = order
                    .lock()
                    .map_err(|_| persistence("child projection owner poisoned"))?;
                let admission = admission
                    .take()
                    .ok_or_else(|| persistence("child read already started"))?;
                let lease = admission.capture(&session).map_err(persistence)?;
                let mut index = SubagentLifecycleIndex::open(&lease.view).map_err(persistence)?;
                while index.advance(&lease.view).map_err(persistence)? {}
                let view = index.snapshot(&lease.view).map_err(persistence)?;
                query(&view, &lease).map_err(persistence)
            })
            .await
    }
    pub(in crate::session_runtime) async fn binding(
        &self,
        parent: &SessionId,
        child: &SubagentId,
    ) -> Result<Option<SubagentBinding>, AgentLoopError> {
        let child = child.clone();
        self.query(parent, move |view, _| view.binding(&child))
            .await
    }
    pub(in crate::session_runtime) async fn published(
        &self,
        parent: &SessionId,
        child: &SubagentId,
        session: &SessionId,
    ) -> Result<bool, AgentLoopError> {
        let child = child.clone();
        let session = session.clone();
        self.query(parent, move |view, _| {
            view.published(&child, &session)
                .map(|source| source.is_some())
        })
        .await
    }
    pub(in crate::session_runtime) async fn pending(
        &self,
        parent: &SessionId,
        after: Option<SequenceId>,
    ) -> Result<(Option<SequenceId>, Vec<SubagentBinding>), AgentLoopError> {
        self.query(parent, move |view, _| {
            Ok((view.through(), view.pending(after, 32)?))
        })
        .await
    }
}
#[async_trait]
impl DiffArtifactAuthority for ChildLifecycleReader {
    async fn resolve(
        &self,
        parent: &SessionId,
        id: &str,
    ) -> Result<Option<AuthorizedDiffArtifact>, ToolError> {
        if id.len() > 256 {
            return Err(ToolError::InvalidInput(
                "artifact identity exceeds admission".into(),
            ));
        }
        let id = id.to_owned();
        self.query(parent, move |view, lease| {
            view.artifact(&id).map(|artifact| {
                artifact.map(|artifact| AuthorizedDiffArtifact::new(artifact, lease.clone()))
            })
        })
        .await
        .map_err(|error| ToolError::Output(error.to_string()))
    }
}
#[async_trait]
impl SubagentArtifactSource for ChildLifecycleReader {
    async fn latest(
        &self,
        parent: &SessionId,
        child: &SubagentId,
    ) -> Result<Option<String>, OrchestrationError> {
        let child = child.clone();
        self.query(parent, move |view, _| view.latest_artifact(&child))
            .await
            .map_err(orchestration)
    }
    async fn verify_result(
        &self,
        parent: &SessionId,
        result: &SubagentResult,
    ) -> Result<(), OrchestrationError> {
        let child = result.subagent_id.clone();
        let session = result.session_id.clone();
        let digest = SubagentLifecycleView::result_digest(result)
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        self.query(parent, move |view, _| {
            view.verify_terminal(&child, &session, digest)
        })
        .await
        .map_err(orchestration)
    }
}
fn persistence(error: impl std::fmt::Display) -> AgentLoopError {
    AgentLoopError::Persistence(error.to_string())
}
fn orchestration(error: AgentLoopError) -> OrchestrationError {
    OrchestrationError::Session(error.to_string())
}
