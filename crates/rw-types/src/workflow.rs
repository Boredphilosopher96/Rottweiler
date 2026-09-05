//! Durable identity and outcomes for the existing declarative workflow runner.
use crate::{Cost, DiffArtifactRef, SessionId, SubagentId, Usage};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

/// One explicitly requested execution of a workflow definition.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkflowRunId(String);

impl WorkflowRunId {
    /// Validate a portable, randomly generated 128-bit run identity.
    ///
    /// # Errors
    /// Rejects values outside the canonical 128-bit hexadecimal encoding.
    pub fn parse(value: String) -> Result<Self, String> {
        if value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err("workflow run id must be 32 lowercase hexadecimal characters".to_owned())
        }
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for WorkflowRunId {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}
impl From<WorkflowRunId> for String {
    fn from(value: WorkflowRunId) -> Self {
        value.0
    }
}

/// Stable obligation within a workflow run, independent of its child session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskId {
    pub run_id: WorkflowRunId,
    pub step_id: String,
}

/// Bounded result retained by the scheduler and referenced by dependent steps.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStepArtifact {
    pub subagent_id: SubagentId,
    pub child_session_id: SessionId,
    pub final_text: String,
    pub touched_files: Vec<String>,
    pub diff_artifact: Option<DiffArtifactRef>,
    pub usage: Usage,
    pub cost: Cost,
}

/// A terminal result is recorded only after the executor settles child cleanup.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowTaskOutcome {
    Completed { artifact: Arc<WorkflowStepArtifact> },
    Failed { message: String },
    Skipped,
}

/// Child identity persisted before the orchestrator admits its first turn.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowChild {
    pub subagent_id: SubagentId,
    pub session_id: SessionId,
}

/// Started without a terminal receipt is ambiguous and must never be re-executed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowTaskState {
    Pending,
    Started { child: Option<WorkflowChild> },
    Settled { outcome: WorkflowTaskOutcome },
}

/// One bounded scheduler snapshot protected by an exclusive run owner.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRunState {
    pub run_id: WorkflowRunId,
    pub parent_session_id: SessionId,
    pub workflow: String,
    pub definition_digest: String,
    pub tasks: BTreeMap<String, WorkflowTaskState>,
}

/// Maximum nodes in one workflow and its durable snapshot.
pub const MAX_WORKFLOW_STEPS: usize = 64;
/// Maximum dependency edges in one workflow.
pub const MAX_WORKFLOW_EDGES: usize = 256;
/// Maximum serialized result for one node.
pub const MAX_STEP_ARTIFACT_BYTES: usize = 256 * 1024;
/// Maximum serialized results retained by a run.
pub const MAX_WORKFLOW_ARTIFACT_BYTES: usize = 1024 * 1024;

/// Canonical workflow and step names. Names are keys, never unchecked paths.
#[must_use]
pub fn valid_workflow_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

/// Count an outcome's serialized bytes without allocating a second artifact buffer.
///
/// # Errors
/// Returns an error when a result cannot be encoded within its per-step allowance.
pub fn workflow_outcome_bytes(outcome: &WorkflowTaskOutcome) -> Result<usize, String> {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_STEP_ARTIFACT_BYTES.saturating_sub(self.0) {
                return Err(std::io::Error::other("workflow artifact limit"));
            }
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, outcome).map_err(|error| error.to_string())?;
    Ok(count.0)
}
