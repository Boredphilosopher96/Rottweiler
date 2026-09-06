//! Child lifecycle reads share the actor's owned workers and application source admission.
use super::DurableEventSink;
use async_trait::async_trait;
use rw_core::{
    AgentLoopError, OrchestrationError, SubagentArtifactSource,
    recovery::{RecoveryError, SubagentBinding, SubagentLifecycleIndex, SubagentLifecycleView},
};
use rw_tools::{AuthorizedDiffArtifact, DiffArtifactAuthority, ToolError};
use rw_types::{SequenceId, SessionId, SubagentId, SubagentResult};
use std::sync::Arc;

pub(in crate::session_runtime) struct ChildLifecycleReader {
    sink: Arc<DurableEventSink>,
    metadata_pages: Arc<tokio::sync::Semaphore>,
}
pub(in crate::session_runtime) struct MetadataRead<T> {
    pub(in crate::session_runtime) value: T,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
impl ChildLifecycleReader {
    pub(in crate::session_runtime) fn new(sink: Arc<DurableEventSink>) -> Arc<Self> {
        Arc::new(Self {
            sink,
            metadata_pages: Arc::new(tokio::sync::Semaphore::new(4)),
        })
    }
    pub(in crate::session_runtime) async fn metadata_read<T: Send + 'static>(
        &self,
        metadata: &crate::subagent_metadata::PrivateSubagentMetadataStore,
        query: impl FnOnce(
            &crate::subagent_metadata::PrivateSubagentMetadataStore,
        ) -> Result<T, OrchestrationError>
        + Send
        + 'static,
    ) -> Result<MetadataRead<T>, AgentLoopError> {
        let permit = self
            .metadata_pages
            .clone()
            .try_acquire_owned()
            .map_err(|_| persistence("child metadata read allocation exhausted"))?;
        let metadata = metadata.clone_for_read().map_err(persistence)?;
        self.sink
            .reads
            .run((metadata, Some(permit)), move |(metadata, permit)| {
                let value = query(metadata).map_err(persistence)?;
                Ok(MetadataRead {
                    value,
                    _permit: permit
                        .take()
                        .ok_or_else(|| persistence("child metadata read already delivered"))?,
                })
            })
            .await
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
        let order = self
            .sink
            .journal_service
            .child_projection_order(&session)
            .map_err(persistence)?;
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
            .map_err(|error| orchestration(&error))
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
        .map_err(|error| orchestration(&error))
    }
}
fn persistence(error: impl std::fmt::Display) -> AgentLoopError {
    AgentLoopError::Persistence(error.to_string())
}
fn orchestration(error: &AgentLoopError) -> OrchestrationError {
    OrchestrationError::Session(error.to_string())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{journal_service::JournalService, subagent_metadata::PrivateSubagentMetadataStore};
    use rw_store::session::SessionEventLog;

    fn reader(root: &std::path::Path) -> Arc<ChildLifecycleReader> {
        ChildLifecycleReader::new(
            DurableEventSink::new(
                SessionEventLog::open(root, "parent").expect("log"),
                root.to_owned(),
                "parent".into(),
                JournalService::new(root).expect("journal"),
            )
            .expect("sink"),
        )
    }
    #[tokio::test]
    async fn returned_metadata_pages_keep_their_shared_admission() {
        let root = tempfile::tempdir().expect("root");
        let reader = reader(root.path());
        let metadata = PrivateSubagentMetadataStore::open(root.path()).expect("metadata");
        let mut pages = Vec::new();
        for _ in 0..4 {
            pages.push(
                reader
                    .metadata_read(&metadata, |metadata| {
                        metadata.load_parent_page(&SessionId("parent".into()), None)
                    })
                    .await
                    .expect("page"),
            );
        }
        assert!(reader.metadata_read(&metadata, |_| Ok(())).await.is_err());
        pages.pop();
        assert!(reader.metadata_read(&metadata, |_| Ok(())).await.is_ok());
        drop(pages);
        reader.sink.reads.settle().await.expect("settled");
    }
    #[tokio::test]
    async fn dropped_metadata_caller_keeps_the_blocking_owner_until_settlement() {
        let root = tempfile::tempdir().expect("root");
        let reader = reader(root.path());
        let metadata = PrivateSubagentMetadataStore::open(root.path()).expect("metadata");
        let (started, entered) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let caller_reader = reader.clone();
        let caller = tokio::spawn(async move {
            caller_reader
                .metadata_read(&metadata, move |metadata| {
                    started.send(()).expect("started");
                    wait.recv().expect("release");
                    metadata.load_parent_page(&SessionId("parent".into()), None)
                })
                .await
        });
        entered.await.expect("entered");
        caller.abort();
        let _ = caller.await;
        assert_eq!(reader.metadata_pages.available_permits(), 3);
        assert_eq!(reader.sink.reads.active(), 1);
        release.send(()).expect("release");
        reader.sink.reads.settle().await.expect("settled");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while reader.metadata_pages.available_permits() != 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completion released permit");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_and_display_queries_share_the_child_index_writer() {
        let root = tempfile::tempdir().expect("root");
        let lifecycle = reader(root.path());
        let presentation = crate::transcript_service::TranscriptReader::new(Arc::clone(
            &lifecycle.sink.journal_service,
        ));
        let (started, entered) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = Arc::clone(&lifecycle);
        let first = tokio::spawn(async move {
            worker
                .query(&SessionId("parent".into()), move |view, _| {
                    started.send(()).expect("entered");
                    wait.recv().expect("release");
                    Ok(view.through())
                })
                .await
        });
        entered.await.expect("lifecycle owns writer");
        let mut second = tokio::spawn(async move {
            presentation
                .children(
                    SessionId("parent".into()),
                    rw_types::session_read::SessionReadScope::Session {},
                )
                .await
        });
        let early = tokio::time::timeout(std::time::Duration::from_millis(50), &mut second).await;
        release.send(()).expect("release writer");
        assert!(
            early.is_err(),
            "display must await the shared writer instead of failing an independent open"
        );
        first
            .await
            .expect("lifecycle worker")
            .expect("lifecycle query");
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), second)
            .await
            .expect("display settled")
            .expect("display worker")
            .expect("display query");
        assert!(
            matches!(result.value(), rw_types::session_children::SessionChildrenResult::Ready { snapshot } if snapshot.children.is_empty())
        );
        lifecycle.sink.reads.settle().await.expect("settled reads");
    }
}
