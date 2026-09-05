use async_trait::async_trait;
use rw_ext::{DiscoveredWorkflow, WorkflowJournal, WorkflowRunError};
use rw_store::workflow::{WorkflowRunStore, WorkflowStoreError};
use rw_types::{
    SessionId,
    workflow::{
        TaskId, WorkflowChild, WorkflowRunId, WorkflowRunState, WorkflowTaskOutcome,
        WorkflowTaskState,
    },
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub(super) struct DurableWorkflowJournal {
    pub(super) parent_session_id: SessionId,
    store: Arc<Mutex<WorkflowRunStore>>,
}

impl DurableWorkflowJournal {
    pub(super) async fn open(
        root: PathBuf,
        run_id: WorkflowRunId,
        parent_session_id: SessionId,
        workflow: &DiscoveredWorkflow,
    ) -> Result<Arc<Self>, WorkflowRunError> {
        let expected = WorkflowRunState {
            run_id,
            parent_session_id: parent_session_id.clone(),
            workflow: workflow.name().to_owned(),
            definition_digest: workflow.definition_digest()?,
            tasks: workflow
                .steps()
                .iter()
                .map(|step| (step.id().to_owned(), WorkflowTaskState::Pending))
                .collect(),
        };
        let store = tokio::task::spawn_blocking(move || WorkflowRunStore::open(&root, expected))
            .await
            .map_err(|error| WorkflowRunError::Persistence(error.to_string()))?
            .map_err(|error| WorkflowRunError::Persistence(error.to_string()))?;
        Ok(Arc::new(Self {
            parent_session_id,
            store: Arc::new(Mutex::new(store)),
        }))
    }

    async fn write<T: Send + 'static>(
        &self,
        apply: impl FnOnce(&mut WorkflowRunStore) -> Result<T, WorkflowStoreError> + Send + 'static,
    ) -> Result<T, WorkflowRunError> {
        let store = Arc::clone(&self.store);
        // The worker owns the mutex and run lock even if its waiter disappears.
        tokio::task::spawn_blocking(move || {
            apply(
                &mut store
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        })
        .await
        .map_err(|error| WorkflowRunError::Persistence(error.to_string()))?
        .map_err(|error| WorkflowRunError::Persistence(error.to_string()))
    }
}

#[async_trait]
impl WorkflowJournal for DurableWorkflowJournal {
    async fn state(&self) -> Result<WorkflowRunState, WorkflowRunError> {
        self.write(|store| Ok(store.state().clone())).await
    }
    async fn claim(&self, tasks: Vec<TaskId>) -> Result<(), WorkflowRunError> {
        self.write(move |store| store.claim(&tasks)).await
    }
    async fn bind_child(&self, task: TaskId, child: WorkflowChild) -> Result<(), WorkflowRunError> {
        self.write(move |store| store.bind_child(&task, child))
            .await
    }
    async fn settle(
        &self,
        task: TaskId,
        outcome: WorkflowTaskOutcome,
    ) -> Result<(), WorkflowRunError> {
        self.write(move |store| store.settle(&task, outcome)).await
    }
}

pub(super) fn new_run_id() -> Result<WorkflowRunId, String> {
    use std::fmt::Write;
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    let mut encoded = String::with_capacity(32);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").map_err(|error| error.to_string())?;
    }
    WorkflowRunId::parse(encoded)
}

pub(super) struct TaskObserver {
    pub(super) inner: Arc<dyn rw_core::SubagentObserver>,
    pub(super) journal: Arc<DurableWorkflowJournal>,
    pub(super) task_id: TaskId,
    pub(super) children: Arc<Mutex<Vec<rw_core::SubagentHandle>>>,
}

#[async_trait]
impl rw_core::SubagentObserver for TaskObserver {
    async fn spawned(
        &self,
        handle: &rw_core::SubagentHandle,
        task: &str,
    ) -> Result<(), rw_core::OrchestrationError> {
        self.journal
            .bind_child(
                self.task_id.clone(),
                WorkflowChild {
                    subagent_id: handle.subagent_id.clone(),
                    session_id: handle.session_id.clone(),
                },
            )
            .await
            .map_err(|error| rw_core::OrchestrationError::Observer(error.to_string()))?;
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(handle.clone());
        self.inner.spawned(handle, task).await
    }
    async fn finished(
        &self,
        result: &rw_types::SubagentResult,
    ) -> Result<(), rw_core::OrchestrationError> {
        self.inner.finished(result).await
    }
    async fn progress(
        &self,
        handle: &rw_core::SubagentHandle,
        child_sequence: Option<u64>,
        event: serde_json::Value,
    ) -> Result<(), rw_core::OrchestrationError> {
        self.inner.progress(handle, child_sequence, event).await
    }
}
