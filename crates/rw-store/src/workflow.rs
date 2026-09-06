//! Bounded atomic workflow snapshots. A separate lock file survives replacement.
use rw_types::workflow::{
    TaskId, WorkflowChild, WorkflowRunId, WorkflowRunState, WorkflowTaskOutcome, WorkflowTaskState,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;

const MAX_STATE_BYTES: usize = 2 * 1024 * 1024;
use rw_types::workflow::{MAX_WORKFLOW_ARTIFACT_BYTES, MAX_WORKFLOW_STEPS, valid_workflow_name};

#[derive(Debug, Error)]
pub enum WorkflowStoreError {
    #[error("workflow run is already owned")]
    Busy,
    #[error("workflow identity or definition differs from its durable run")]
    Identity,
    #[error("workflow state transition is not valid")]
    Transition,
    #[error("workflow state exceeds its bounded contract")]
    Limit,
    #[error("workflow storage failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("workflow state is malformed: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct WorkflowRunStore {
    directory: PathBuf,
    #[cfg(unix)]
    _owner: crate::session::AdvisoryFileLock,
    #[cfg(not(unix))]
    owner: File,
    state: WorkflowRunState,
}

#[cfg(not(unix))]
impl Drop for WorkflowRunStore {
    fn drop(&mut self) {
        let _ = self.owner.unlock();
    }
}

impl WorkflowRunStore {
    /// Create a new run or reopen exactly the same definition and parent.
    ///
    /// # Errors
    /// Rejects a competing writer, changed definition, corrupt state or failed I/O.
    pub fn open(root: &Path, expected: WorkflowRunState) -> Result<Self, WorkflowStoreError> {
        validate(&expected)?;
        let directory = root.join("workflow-runs").join(expected.run_id.as_str());
        fs::create_dir_all(&directory)?;
        File::open(root.join("workflow-runs"))?.sync_all()?;
        File::open(root)?.sync_all()?;
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("writer.lock"))?;
        #[cfg(unix)]
        let owner = crate::session::AdvisoryFileLock::try_exclusive(owner).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                WorkflowStoreError::Busy
            } else {
                WorkflowStoreError::Io(error)
            }
        })?;
        #[cfg(not(unix))]
        owner.try_lock().map_err(|_| WorkflowStoreError::Busy)?;
        let state_path = directory.join("state.json");
        let state = match File::open(&state_path) {
            Ok(file) => {
                let state = read_state(file)?;
                if state.run_id != expected.run_id
                    || state.parent_session_id != expected.parent_session_id
                    || state.workflow != expected.workflow
                    || state.definition_digest != expected.definition_digest
                    || !state.tasks.keys().eq(expected.tasks.keys())
                {
                    return Err(WorkflowStoreError::Identity);
                }
                state
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if expected
                    .tasks
                    .values()
                    .any(|state| !matches!(state, WorkflowTaskState::Pending))
                {
                    return Err(WorkflowStoreError::Transition);
                }
                persist(&directory, &expected)?;
                expected
            }
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            directory,
            #[cfg(unix)]
            _owner: owner,
            #[cfg(not(unix))]
            owner,
            state,
        })
    }

    /// Read one complete atomic snapshot while its writer remains active.
    ///
    /// # Errors
    /// Rejects a missing, corrupt, oversized or foreign-parent run.
    pub fn snapshot(
        root: &Path,
        run_id: &WorkflowRunId,
        parent_session_id: &rw_types::SessionId,
    ) -> Result<WorkflowRunState, WorkflowStoreError> {
        let state = read_state(File::open(
            root.join("workflow-runs")
                .join(run_id.as_str())
                .join("state.json"),
        )?)?;
        if &state.run_id != run_id || &state.parent_session_id != parent_session_id {
            return Err(WorkflowStoreError::Identity);
        }
        Ok(state)
    }

    #[must_use]
    pub fn state(&self) -> &WorkflowRunState {
        &self.state
    }

    /// Reserve every node in a wave in one durable transition before execution.
    ///
    /// # Errors
    /// Rejects foreign or previously started tasks and failed persistence.
    pub fn claim(&mut self, tasks: &[TaskId]) -> Result<(), WorkflowStoreError> {
        if tasks.is_empty() {
            return Err(WorkflowStoreError::Transition);
        }
        let mut next = self.state.clone();
        for task in tasks {
            let state = task_state(&mut next, task)?;
            if !matches!(state, WorkflowTaskState::Pending) {
                return Err(WorkflowStoreError::Transition);
            }
            *state = WorkflowTaskState::Started { child: None };
        }
        self.commit(next)
    }

    /// Bind before provider/tool execution; retrying the exact binding is harmless.
    ///
    /// # Errors
    /// Rejects a changed child, unknown task or failed persistence.
    pub fn bind_child(
        &mut self,
        task: &TaskId,
        child: WorkflowChild,
    ) -> Result<(), WorkflowStoreError> {
        let mut next = self.state.clone();
        match task_state(&mut next, task)? {
            WorkflowTaskState::Started { child: bound }
                if bound.as_ref().is_none_or(|value| value == &child) =>
            {
                *bound = Some(child);
            }
            _ => return Err(WorkflowStoreError::Transition),
        }
        self.commit(next)
    }

    /// A terminal receipt replaces a started obligation, or skips an unstarted node.
    ///
    /// # Errors
    /// Rejects conflicting receipts, child identity mismatches, oversized results or I/O.
    pub fn settle(
        &mut self,
        task: &TaskId,
        outcome: WorkflowTaskOutcome,
    ) -> Result<(), WorkflowStoreError> {
        let mut next = self.state.clone();
        let state = task_state(&mut next, task)?;
        match &*state {
            WorkflowTaskState::Settled { outcome: previous } if previous == &outcome => {
                return Ok(());
            }
            WorkflowTaskState::Started { child }
                if !matches!(outcome, WorkflowTaskOutcome::Skipped) =>
            {
                if let WorkflowTaskOutcome::Completed { artifact } = &outcome
                    && child.as_ref().is_none_or(|child| {
                        child.subagent_id != artifact.subagent_id
                            || child.session_id != artifact.child_session_id
                    })
                {
                    return Err(WorkflowStoreError::Identity);
                }
            }
            WorkflowTaskState::Pending if matches!(outcome, WorkflowTaskOutcome::Skipped) => {}
            _ => return Err(WorkflowStoreError::Transition),
        }
        *state = WorkflowTaskState::Settled { outcome };
        self.commit(next)
    }

    fn commit(&mut self, next: WorkflowRunState) -> Result<(), WorkflowStoreError> {
        validate(&next)?;
        persist(&self.directory, &next)?;
        self.state = next;
        Ok(())
    }
}

fn read_state(file: File) -> Result<WorkflowRunState, WorkflowStoreError> {
    if file.metadata()?.len() > MAX_STATE_BYTES as u64 {
        return Err(WorkflowStoreError::Limit);
    }
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_STATE_BYTES {
        return Err(WorkflowStoreError::Limit);
    }
    let state = serde_json::from_slice(&bytes)?;
    validate(&state)?;
    Ok(state)
}

fn task_state<'a>(
    state: &'a mut WorkflowRunState,
    task: &TaskId,
) -> Result<&'a mut WorkflowTaskState, WorkflowStoreError> {
    if state.run_id != task.run_id {
        return Err(WorkflowStoreError::Identity);
    }
    state
        .tasks
        .get_mut(&task.step_id)
        .ok_or(WorkflowStoreError::Identity)
}

fn validate(state: &WorkflowRunState) -> Result<(), WorkflowStoreError> {
    WorkflowRunId::parse(state.run_id.as_str().to_owned())
        .map_err(|_| WorkflowStoreError::Identity)?;
    rw_types::SessionId::validate(&state.parent_session_id.0)
        .map_err(|_| WorkflowStoreError::Identity)?;
    if state.tasks.is_empty()
        || state.tasks.len() > MAX_WORKFLOW_STEPS
        || state.parent_session_id.0.len() > 128
        || !valid_workflow_name(&state.workflow)
        || state.definition_digest.len() != 64
        || !state
            .definition_digest
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(WorkflowStoreError::Limit);
    }
    let mut total = 0;
    for (id, task) in &state.tasks {
        if !valid_workflow_name(id) {
            return Err(WorkflowStoreError::Identity);
        }
        if let WorkflowTaskState::Started { child: Some(child) } = task {
            rw_types::SessionId::validate(&child.session_id.0)
                .map_err(|_| WorkflowStoreError::Identity)?;
            if child.subagent_id.0.is_empty() || child.subagent_id.0.len() > 128 {
                return Err(WorkflowStoreError::Limit);
            }
        }
        if let WorkflowTaskState::Settled { outcome } = task {
            let bytes = rw_types::workflow::workflow_outcome_bytes(outcome)
                .map_err(|_| WorkflowStoreError::Limit)?;
            total += bytes;
            if total > MAX_WORKFLOW_ARTIFACT_BYTES {
                return Err(WorkflowStoreError::Limit);
            }
        }
    }
    Ok(())
}

fn encoded(value: &impl serde::Serialize, limit: usize) -> Result<Vec<u8>, WorkflowStoreError> {
    struct Output {
        bytes: Vec<u8>,
        limit: usize,
        exceeded: bool,
    }
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(std::io::Error::other("workflow bound"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Output {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut output, value);
    if output.exceeded {
        return Err(WorkflowStoreError::Limit);
    }
    result?;
    Ok(output.bytes)
}

fn persist(directory: &Path, state: &WorkflowRunState) -> Result<(), WorkflowStoreError> {
    let bytes = encoded(state, MAX_STATE_BYTES)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(directory.join("state.json"))
        .map_err(|error| error.error)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests;
