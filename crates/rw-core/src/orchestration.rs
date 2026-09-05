//! Deterministic full-session subagent orchestration.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock, Weak},
    time::Duration,
};

use async_trait::async_trait;
use rw_ext::LoadedAgent;
use rw_tools::{CancellationToken, CapabilityManifest, ToolRegistry, WorktreeLeaseRecord};
use rw_types::{
    Cost, DiffArtifact, EngineEvent, SessionId, SessionMode, SubagentActivity, SubagentDescriptor,
    SubagentId, SubagentIsolation, SubagentResult, SubagentStatus, TurnStatus, Usage,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Semaphore, watch};

pub const DEFAULT_SUBAGENT_MAX_DEPTH: usize = 2;
pub const DEFAULT_SUBAGENT_CONCURRENCY: usize = 4;
pub const DEFAULT_SUBAGENT_MAX_TURNS: usize = 32;
pub const DEFAULT_SUBAGENT_MAX_DURATION: Duration = Duration::from_mins(30);
const MAX_SUBAGENT_FINAL_TEXT_BYTES: usize = 256 * 1024;
const MAX_SUBAGENT_DIFF_BYTES: usize = 4 * 1024 * 1024;
const MAX_SUBAGENT_TOUCHED_FILES: usize = 4096;
const MAX_SUBAGENT_PROGRESS_BYTES: usize = 256 * 1024;
const MAX_MODEL_SUBAGENT_TEXT_BYTES: usize = 12 * 1024;
const MAX_MODEL_SUBAGENT_SUMMARY_BYTES: usize = 8 * 1024;
const MAX_ARTIFACT_REF_PREVIEW_BYTES: usize = 4 * 1024;
const MAX_ARTIFACT_REF_FILES: usize = 32;
const MAX_ARTIFACT_REF_PATH_BYTES: usize = 128;

/// Runtime bounds shared by model tool calls and headless workflows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubagentLimits {
    pub max_depth: usize,
    pub max_concurrency: usize,
    pub max_turns: usize,
    pub max_duration: Duration,
}

impl Default for SubagentLimits {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_SUBAGENT_MAX_DEPTH,
            max_concurrency: DEFAULT_SUBAGENT_CONCURRENCY,
            max_turns: DEFAULT_SUBAGENT_MAX_TURNS,
            max_duration: DEFAULT_SUBAGENT_MAX_DURATION,
        }
    }
}

/// Provider-blind request with an engine-resolved immutable launch policy.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SubagentRequest {
    pub task: String,
    pub agent: String,
    pub model: String,
    pub tools: Vec<String>,
    pub system_prompt: Option<String>,
    pub permission_mode: SessionMode,
    pub max_turns: Option<usize>,
    pub isolation: SubagentIsolation,
    #[serde(skip)]
    pub workspace_root: PathBuf,
}

impl SubagentRequest {
    /// Constructs a trusted launch request from an engine-resolved agent definition.
    #[must_use]
    pub fn from_loaded_agent(
        task: impl Into<String>,
        agent: LoadedAgent,
        inherited_model: impl Into<String>,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            task: task.into(),
            agent: agent.name,
            model: agent.model.unwrap_or_else(|| inherited_model.into()),
            tools: agent.tools,
            system_prompt: Some(agent.system_prompt),
            permission_mode: agent.permission_mode,
            max_turns: Some(agent.max_turns),
            isolation: SubagentIsolation::default(),
            workspace_root,
        }
    }
}

/// Stable handle retained by the parent for waiting, cancellation, and follow-up.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentHandle {
    pub subagent_id: SubagentId,
    pub session_id: SessionId,
}

/// Exact immutable child policy required to recreate a continuable session.
/// This remains host-private with the recovery record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentRecoveryPolicy {
    pub model_alias: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub system_prompt: Option<String>,
    pub permission_mode: SessionMode,
    pub max_turns: usize,
}

/// Host-private restart metadata. This must never enter model context or the
/// public parent event stream.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentRecoveryRecord {
    pub parent_session_id: SessionId,
    pub handle: SubagentHandle,
    pub task: String,
    pub agent: String,
    pub depth: usize,
    pub workspace_root: PathBuf,
    pub isolation: SubagentIsolation,
    #[serde(deserialize_with = "Option::deserialize")]
    pub worktree: Option<WorktreeLeaseRecord>,
    pub capabilities: CapabilityManifest,
    pub tool_names: Vec<String>,
    pub policy: SubagentRecoveryPolicy,
    pub phase: SubagentRecoveryPhase,
}

/// Two-phase host-private binding between a child lease and durable parent lifecycle.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentRecoveryPhase {
    Pending,
    /// The child closed before its first turn; reconcile lifecycle without reattaching its workspace.
    Closed,
    #[default]
    Active,
}

/// Finds durable child spawns that have no matching terminal lifecycle event.
/// Returned handles retain spawn order so recovery appends deterministic repairs.
///
/// # Errors
///
/// Returns when lifecycle identities are inconsistent or overlap for one child id.
pub fn incomplete_subagent_lifecycles(
    events: &[EngineEvent],
) -> Result<Vec<SubagentHandle>, OrchestrationError> {
    let mut active = Vec::<SubagentHandle>::new();
    for event in events {
        match event {
            EngineEvent::SubagentSpawned {
                subagent_id,
                child_session_id,
                ..
            } => {
                if active
                    .iter()
                    .any(|handle| handle.subagent_id == *subagent_id)
                {
                    return Err(OrchestrationError::Session(
                        "durable child spawned twice without a terminal result".to_owned(),
                    ));
                }
                active.push(SubagentHandle {
                    subagent_id: subagent_id.clone(),
                    session_id: child_session_id.clone(),
                });
            }
            EngineEvent::SubagentFinished {
                subagent_id,
                result,
                ..
            } => {
                let position = active
                    .iter()
                    .position(|handle| handle.subagent_id == *subagent_id)
                    .ok_or_else(|| {
                        OrchestrationError::Session(
                            "durable child result has no active spawn".to_owned(),
                        )
                    })?;
                let handle = active.remove(position);
                if result.subagent_id != *subagent_id || result.session_id != handle.session_id {
                    return Err(OrchestrationError::Session(
                        "durable child result identity is inconsistent".to_owned(),
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(active)
}

/// Builds the artifact-free terminal used to repair a parent crash after Spawned committed.
#[must_use]
pub fn interrupted_subagent_recovery_result(handle: &SubagentHandle) -> SubagentResult {
    let reason = "parent process stopped before the child terminal event committed".to_owned();
    SubagentResult {
        subagent_id: handle.subagent_id.clone(),
        session_id: handle.session_id.clone(),
        status: SubagentStatus::Failed,
        final_text: reason.clone(),
        touched_files: Vec::new(),
        diff_artifact: None,
        usage: zero_usage(),
        cost: Cost::Unavailable { reason },
        turns: 0,
        duration_millis: 0,
    }
}

/// Atomic host persistence for continuable child metadata.
#[async_trait]
pub trait SubagentMetadataStore: Send + Sync {
    async fn save(&self, record: SubagentRecoveryRecord) -> Result<(), OrchestrationError>;

    async fn remove(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<(), OrchestrationError>;
}

#[derive(Debug, Default)]
pub struct NoopSubagentMetadataStore;

#[async_trait]
impl SubagentMetadataStore for NoopSubagentMetadataStore {
    async fn save(&self, _record: SubagentRecoveryRecord) -> Result<(), OrchestrationError> {
        Ok(())
    }

    async fn remove(
        &self,
        _parent_session_id: &SessionId,
        _subagent_id: &SubagentId,
    ) -> Result<(), OrchestrationError> {
        Ok(())
    }
}

/// Input supplied to a session factory after every orchestrator invariant is checked.
#[derive(Clone)]
pub struct SubagentLaunch {
    pub handle: SubagentHandle,
    pub parent_session_id: SessionId,
    pub depth: usize,
    pub request: SubagentRequest,
    pub tools: Arc<ToolRegistry>,
    pub max_turns: usize,
    pub workspace_root: PathBuf,
    pub cancellation: CancellationToken,
}

impl std::fmt::Debug for SubagentLaunch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubagentLaunch")
            .field("handle", &self.handle)
            .field("parent_session_id", &self.parent_session_id)
            .field("depth", &self.depth)
            .field("request", &self.request)
            .field("tool_count", &self.tools.len())
            .field("max_turns", &self.max_turns)
            .field("workspace_root", &self.workspace_root)
            .finish_non_exhaustive()
    }
}

/// One completed child turn before stable ids and elapsed time are attached.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentTurnResult {
    pub status: SubagentStatus,
    pub final_text: String,
    pub touched_files: Vec<String>,
    pub diff_artifact: Option<DiffArtifact>,
    pub usage: Usage,
    pub cost: Cost,
    pub turns: u64,
}

/// A persistent child session. Implementations must keep context and their own event log.
#[async_trait]
pub trait SubagentSession: Send + Sync {
    fn session_id(&self) -> &SessionId;

    async fn run_turn(
        &self,
        prompt: String,
        cancellation: CancellationToken,
        progress: Arc<dyn SubagentProgressObserver>,
    ) -> Result<SubagentTurnResult, OrchestrationError>;

    async fn cancel(&self) -> Result<(), OrchestrationError>;

    async fn close(
        &self,
        durable_artifact: Option<&DiffArtifact>,
    ) -> Result<(), OrchestrationError>;

    /// Host-private worktree identity used for restart-safe continuation.
    fn worktree_record(&self) -> Option<WorktreeLeaseRecord> {
        None
    }
}

/// Factory boundary used for normal child sessions and replay fixtures.
#[async_trait]
pub trait SubagentSessionFactory: Send + Sync {
    async fn create(
        &self,
        launch: SubagentLaunch,
    ) -> Result<Arc<dyn SubagentSession>, OrchestrationError>;

    /// Rebinds a child event log/session during parent recovery.
    async fn rebind(
        &self,
        _session_id: &SessionId,
        _workspace_root: Option<&Path>,
        _worktree: Option<&WorktreeLeaseRecord>,
        _allowed_tools: Option<&ToolRegistry>,
        _policy: &SubagentRecoveryPolicy,
    ) -> Result<Option<Arc<dyn SubagentSession>>, OrchestrationError> {
        Ok(None)
    }
}

/// Display-only progress receiver. Implementations must not append to a parent log.
#[async_trait]
pub trait SubagentProgressObserver: Send + Sync {
    async fn progress(
        &self,
        child_sequence: Option<u64>,
        event: Value,
    ) -> Result<(), OrchestrationError>;
}

/// Lifecycle observer. Engine integration persists spawned/finished and forwards progress.
#[async_trait]
pub trait SubagentObserver: Send + Sync {
    async fn spawned(&self, handle: &SubagentHandle, task: &str) -> Result<(), OrchestrationError>;

    async fn finished(&self, result: &SubagentResult) -> Result<(), OrchestrationError>;

    async fn progress(
        &self,
        handle: &SubagentHandle,
        child_sequence: Option<u64>,
        event: Value,
    ) -> Result<(), OrchestrationError>;
}

#[derive(Debug, Error)]
pub enum OrchestrationError {
    #[error("subagent depth {requested} exceeds configured maximum {maximum}")]
    DepthExceeded { requested: usize, maximum: usize },
    #[error("subagent concurrency limit {maximum} is exhausted")]
    ConcurrencyExceeded { maximum: usize },
    #[error("subagent request is invalid: {0}")]
    InvalidRequest(String),
    #[error("subagent `{0}` does not exist")]
    UnknownSubagent(String),
    #[error("subagent `{0}` is already running")]
    AlreadyRunning(String),
    #[error("subagent `{0}` has no pending result")]
    NoPendingResult(String),
    #[error("subagent effects remain unproven: {0}")]
    EffectsUnsettled(String),
    #[error("subagent session failed: {0}")]
    Session(String),
    #[error("subagent observer failed: {0}")]
    Observer(String),
}

#[derive(Clone)]
pub struct SubagentOrchestrator {
    inner: Arc<OrchestratorInner>,
}

struct OrchestratorInner {
    limits: SubagentLimits,
    startups: startup::Startups,
    factory: Arc<dyn SubagentSessionFactory>,
    base_tools: Arc<ToolRegistry>,
    tools: RwLock<Weak<ToolRegistry>>,
    permits: Arc<Semaphore>,
    sequence: std::sync::atomic::AtomicU64,
    sessions: Mutex<HashMap<SubagentId, SessionRecord>>,
    session_depths: Mutex<HashMap<SessionId, usize>>,
    diff_artifact_authority: Arc<dyn SubagentArtifactSource>,
    metadata: RwLock<Arc<dyn SubagentMetadataStore>>,
}

struct SessionRecord {
    handle: SubagentHandle,
    task: String,
    agent: String,
    model: String,
    session: Arc<dyn SubagentSession>,
    state: SessionState,
    result: Option<watch::Receiver<Option<Result<SubagentResult, String>>>>,
    isolation: SubagentIsolation,
    parent_session_id: SessionId,
    latest_durable_artifact_id: Option<String>,
    closing_artifact: Option<Arc<rw_tools::AuthorizedDiffArtifact>>,
    close_completed: bool,
    close_gate: Arc<tokio::sync::Mutex<()>>,
}

fn ensure_child_owner(
    caller_parent_session_id: &SessionId,
    subagent_id: &SubagentId,
    record: &SessionRecord,
) -> Result<(), OrchestrationError> {
    if record.parent_session_id == *caller_parent_session_id {
        Ok(())
    } else {
        // Deliberately hide whether a guessed id belongs to another parent.
        Err(OrchestrationError::UnknownSubagent(subagent_id.0.clone()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionState {
    Inactive,
    Active,
    Closing,
}

fn session_record_descriptor(record: &SessionRecord) -> SubagentDescriptor {
    SubagentDescriptor {
        subagent_id: record.handle.subagent_id.clone(),
        child_session_id: record.handle.session_id.clone(),
        task: record.task.clone(),
        agent: record.agent.clone(),
        model: record.model.clone(),
        isolation: record.isolation,
        activity: if record.state == SessionState::Active {
            SubagentActivity::Running
        } else {
            SubagentActivity::Idle
        },
    }
}

struct ObserverProgress {
    observer: Arc<dyn SubagentObserver>,
    handle: SubagentHandle,
}

#[async_trait]
impl SubagentProgressObserver for ObserverProgress {
    async fn progress(
        &self,
        child_sequence: Option<u64>,
        event: Value,
    ) -> Result<(), OrchestrationError> {
        if serde_json::to_vec(&event)
            .is_ok_and(|encoded| encoded.len() > MAX_SUBAGENT_PROGRESS_BYTES)
        {
            return Err(OrchestrationError::Observer(
                "child progress event exceeds size limit".to_owned(),
            ));
        }
        self.observer
            .progress(&self.handle, child_sequence, event)
            .await
    }
}

fn subagent_status(status: &TurnStatus) -> SubagentStatus {
    match status {
        TurnStatus::Completed => SubagentStatus::Completed,
        TurnStatus::Interrupted => SubagentStatus::Cancelled,
        TurnStatus::MaxTurns => SubagentStatus::MaxTurns,
        TurnStatus::Failed | TurnStatus::DoomLoop | TurnStatus::BudgetExceeded => {
            SubagentStatus::Failed
        }
    }
}

mod artifact_source;
mod lifecycle;
pub use artifact_source::SubagentArtifactSource;
mod startup;

mod policy;
mod presentation;
pub use policy::diff_artifact_reference;
use policy::{
    bound_turn_result, bounded_cancel, bounded_close, control_timeout,
    model_facing_subagent_tool_result, random_id, restricted_registry, validate_request,
    zero_usage,
};

mod tools;
pub use tools::{SpawnAgentTool, subagent_result_tool_output};

mod sessions;
pub use sessions::{ActorSubagentSessionFactory, WorktreeSubagentSessionFactory};

#[cfg(test)]
mod tests;
