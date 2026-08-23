//! Deterministic full-session subagent orchestration.

use std::{
    collections::HashMap,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock, Weak},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use rw_ext::{AgentRegistry, LoadedAgent};
use rw_tools::{
    CancellationToken, CapabilityManifest, DiffArtifactAuthority, McpToolPolicy,
    SessionDiffArtifactAuthority, SubagentEventSink, SubagentLifecycleEvent, SubagentLifecycleMode,
    SubagentProgressEvent, Tool, ToolContext, ToolDescriptor, ToolError, ToolRegistry, ToolResult,
    WorkspaceBinding, WorktreeIsolation, WorktreeLease, WorktreeLeaseRecord,
    validate_mcp_virtual_tool,
};
#[cfg(test)]
use rw_types::config::PermissionDecision;
use rw_types::{
    Block, Cost, DiffArtifact, DiffArtifactRef, EngineEvent, Role, SessionId, SessionMode,
    SubagentActivity, SubagentDescriptor, SubagentId, SubagentIsolation, SubagentResult,
    SubagentStatus, ToolCapability, ToolOutput, ToolOutputPart, Turn, TurnMeta, TurnStatus, Usage,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{Semaphore, watch};

use crate::{AgentLoopError, ModelDriver, SessionActor, SessionActorConfig, SessionHandle};

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
pub struct SubagentHandle {
    pub subagent_id: SubagentId,
    pub session_id: SessionId,
}

/// Exact immutable child policy required to recreate a continuable session.
/// This remains host-private with the recovery record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentRecoveryPolicy {
    pub model_alias: String,
    pub system_prompt: Option<String>,
    pub permission_mode: SessionMode,
    pub max_turns: usize,
}

/// Host-private restart metadata. This must never enter model context or the
/// public parent event stream.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SubagentRecoveryRecord {
    pub parent_session_id: SessionId,
    pub handle: SubagentHandle,
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub agent: String,
    pub depth: usize,
    pub workspace_root: PathBuf,
    pub isolation: SubagentIsolation,
    pub worktree: Option<WorktreeLeaseRecord>,
    pub capabilities: CapabilityManifest,
    pub tool_names: Vec<String>,
    pub policy: SubagentRecoveryPolicy,
    #[serde(default)]
    pub phase: SubagentRecoveryPhase,
}

/// Two-phase host-private binding between a child lease and durable parent lifecycle.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentRecoveryPhase {
    Pending,
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
        _durable_artifact: Option<&DiffArtifact>,
    ) -> Result<(), OrchestrationError> {
        self.cancel().await
    }

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
    factory: Arc<dyn SubagentSessionFactory>,
    base_tools: Arc<ToolRegistry>,
    tools: RwLock<Weak<ToolRegistry>>,
    permits: Arc<Semaphore>,
    sequence: std::sync::atomic::AtomicU64,
    sessions: Mutex<HashMap<SubagentId, SessionRecord>>,
    session_depths: Mutex<HashMap<SessionId, usize>>,
    diff_artifact_authority: Arc<SessionDiffArtifactAuthority>,
    latest_artifacts: Mutex<HashMap<(SessionId, SubagentId), String>>,
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

impl SubagentOrchestrator {
    /// Builds an orchestrator over the same public registry used by the parent actor.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request error when a configured limit is zero.
    pub fn new(
        limits: SubagentLimits,
        factory: Arc<dyn SubagentSessionFactory>,
        tools: Arc<ToolRegistry>,
    ) -> Result<Self, OrchestrationError> {
        if limits.max_concurrency == 0 || limits.max_turns == 0 || limits.max_duration.is_zero() {
            return Err(OrchestrationError::InvalidRequest(
                "concurrency, turn, and duration limits must be greater than zero".to_owned(),
            ));
        }
        let weak_tools = Arc::downgrade(&tools);
        Ok(Self {
            inner: Arc::new(OrchestratorInner {
                limits,
                factory,
                base_tools: tools,
                tools: RwLock::new(weak_tools),
                permits: Arc::new(Semaphore::new(limits.max_concurrency)),
                sequence: std::sync::atomic::AtomicU64::new(0),
                sessions: Mutex::new(HashMap::new()),
                session_depths: Mutex::new(HashMap::new()),
                diff_artifact_authority: Arc::new(SessionDiffArtifactAuthority::default()),
                latest_artifacts: Mutex::new(HashMap::new()),
                metadata: RwLock::new(Arc::new(NoopSubagentMetadataStore)),
            }),
        })
    }

    #[must_use]
    pub fn limits(&self) -> SubagentLimits {
        self.inner.limits
    }

    /// Binds the final registry after cyclic orchestration tools are added.
    /// Future children inherit it, allowing safe nested spawning.
    pub fn bind_tools(&self, tools: Arc<ToolRegistry>) {
        let weak_tools = Arc::downgrade(&tools);
        drop(tools);
        *self
            .inner
            .tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = weak_tools;
    }

    /// Installs atomic host-private continuation metadata persistence.
    pub fn bind_metadata_store(&self, store: Arc<dyn SubagentMetadataStore>) {
        *self
            .inner
            .metadata
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = store;
    }

    fn tool_registry(&self) -> Arc<ToolRegistry> {
        self.inner
            .tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .upgrade()
            .unwrap_or_else(|| Arc::clone(&self.inner.base_tools))
    }

    /// Shared provenance authority for the registered `apply_worktree_diff` tool.
    #[must_use]
    pub fn diff_artifact_authority(&self) -> Arc<SessionDiffArtifactAuthority> {
        Arc::clone(&self.inner.diff_artifact_authority)
    }

    /// Rebuilds one exact grant from a durable parent `SubagentFinished` result.
    ///
    /// # Errors
    ///
    /// Returns when the durable artifact is malformed.
    pub fn record_recovered_result(
        &self,
        parent_session_id: SessionId,
        result: &SubagentResult,
    ) -> Result<(), OrchestrationError> {
        if let Some(artifact) = &result.diff_artifact {
            self.inner
                .diff_artifact_authority
                .record_durable(parent_session_id, artifact)
                .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        }
        Ok(())
    }

    /// Rebuilds artifact grants exclusively from committed parent events.
    /// Legacy text-only child results are ignored.
    ///
    /// # Errors
    ///
    /// Returns when a structured result disagrees with its durable subagent id
    /// or contains an invalid artifact.
    pub fn rebuild_artifact_authority(
        &self,
        parent_session_id: &SessionId,
        events: &[EngineEvent],
    ) -> Result<(), OrchestrationError> {
        self.inner
            .diff_artifact_authority
            .revoke_session(parent_session_id);
        self.inner
            .latest_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(session, _), _| session != parent_session_id);
        let mut artifacts = Vec::new();
        let mut latest = HashMap::<SubagentId, Option<String>>::new();
        for event in events {
            let EngineEvent::SubagentFinished {
                subagent_id,
                result,
                ..
            } = event
            else {
                continue;
            };
            if &result.subagent_id != subagent_id {
                return Err(OrchestrationError::Session(
                    "durable child result id does not match its lifecycle event".to_owned(),
                ));
            }
            if let Some(artifact) = &result.diff_artifact {
                self.inner
                    .diff_artifact_authority
                    .validate(artifact)
                    .map_err(|error| OrchestrationError::Session(error.to_string()))?;
                artifacts.push(artifact);
            }
            latest.insert(
                subagent_id.clone(),
                result
                    .diff_artifact
                    .as_ref()
                    .map(|artifact| artifact.id.clone()),
            );
        }
        for artifact in artifacts {
            self.inner
                .diff_artifact_authority
                .record_durable(parent_session_id.clone(), artifact)
                .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        }
        let mut recovered_latest = self
            .inner
            .latest_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (subagent_id, artifact_id) in latest {
            if let Some(artifact_id) = artifact_id {
                recovered_latest.insert((parent_session_id.clone(), subagent_id), artifact_id);
            }
        }
        Ok(())
    }

    /// Starts a new child and returns immediately with a stable parent handle.
    ///
    /// # Errors
    ///
    /// Returns validation, depth, concurrency, factory, or observer failures.
    #[allow(clippy::too_many_lines)]
    pub async fn start(
        &self,
        parent_session_id: SessionId,
        request: SubagentRequest,
        observer: Arc<dyn SubagentObserver>,
        cancellation: CancellationToken,
    ) -> Result<SubagentHandle, OrchestrationError> {
        validate_request(&request)?;
        let parent_depth = self
            .inner
            .session_depths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&parent_session_id)
            .copied()
            .unwrap_or(0);
        let depth = parent_depth.saturating_add(1);
        if depth > self.inner.limits.max_depth {
            return Err(OrchestrationError::DepthExceeded {
                requested: depth,
                maximum: self.inner.limits.max_depth,
            });
        }
        let permit = Arc::clone(&self.inner.permits)
            .try_acquire_owned()
            .map_err(|_| OrchestrationError::ConcurrencyExceeded {
                maximum: self.inner.limits.max_concurrency,
            })?;
        let tools = restricted_registry(
            &self.tool_registry(),
            &request.tools,
            request.permission_mode,
        )?;
        let capabilities = CapabilityManifest::new(
            tools
                .descriptors()
                .into_iter()
                .flat_map(|descriptor| descriptor.capabilities.capabilities().to_vec()),
        );
        let ordinal = self
            .inner
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let random = random_id()?;
        let handle = SubagentHandle {
            subagent_id: SubagentId(format!("agent-{ordinal}-{random}")),
            session_id: SessionId(format!("child-{random}")),
        };
        let resolved_max_turns = request
            .max_turns
            .unwrap_or(self.inner.limits.max_turns)
            .min(self.inner.limits.max_turns);
        let launch = SubagentLaunch {
            handle: handle.clone(),
            parent_session_id: parent_session_id.clone(),
            depth,
            request: request.clone(),
            tools: Arc::clone(&tools),
            max_turns: resolved_max_turns,
            workspace_root: request.workspace_root.clone(),
            cancellation: cancellation.clone(),
        };
        let session = self.inner.factory.create(launch).await?;
        if session.session_id() != &handle.session_id {
            return Err(OrchestrationError::Session(
                "child factory returned a different session id".to_owned(),
            ));
        }
        let mut recovery_record = SubagentRecoveryRecord {
            parent_session_id: parent_session_id.clone(),
            handle: handle.clone(),
            task: request.task.clone(),
            agent: request.agent.clone(),
            depth,
            workspace_root: request.workspace_root.clone(),
            isolation: request.isolation,
            worktree: session.worktree_record(),
            capabilities: capabilities.clone(),
            tool_names: tools
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.name)
                .chain(
                    request
                        .tools
                        .iter()
                        .filter(|name| name.starts_with("mcp:"))
                        .cloned(),
                )
                .collect(),
            policy: SubagentRecoveryPolicy {
                model_alias: request.model.clone(),
                system_prompt: request.system_prompt.clone(),
                permission_mode: request.permission_mode,
                max_turns: resolved_max_turns,
            },
            phase: SubagentRecoveryPhase::Pending,
        };
        let metadata = self
            .inner
            .metadata
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Err(error) = metadata.save(recovery_record.clone()).await {
            bounded_close(&session, None, self.inner.limits).await.map_err(|cleanup| {
                OrchestrationError::Session(format!(
                    "{error}; child cleanup after pending metadata failure also failed: {cleanup}"
                ))
            })?;
            return Err(error);
        }
        if let Err(error) = observer.spawned(&handle, &request.task).await {
            let _ = bounded_cancel(&session, self.inner.limits).await;
            return Err(error);
        }
        recovery_record.phase = SubagentRecoveryPhase::Active;
        if let Err(error) = metadata.save(recovery_record).await {
            let _ = bounded_cancel(&session, self.inner.limits).await;
            let terminal = SubagentResult {
                subagent_id: handle.subagent_id.clone(),
                session_id: handle.session_id.clone(),
                status: SubagentStatus::Failed,
                final_text: error.to_string(),
                touched_files: Vec::new(),
                diff_artifact: None,
                usage: zero_usage(),
                cost: Cost::Unavailable {
                    reason: "child metadata promotion failed".to_owned(),
                },
                turns: 0,
                duration_millis: 0,
            };
            observer.finished(&terminal).await?;
            return Err(error);
        }
        let (result_tx, result_rx) = watch::channel(None);
        self.inner
            .session_depths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(handle.session_id.clone(), depth);
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                handle.subagent_id.clone(),
                SessionRecord {
                    handle: handle.clone(),
                    task: request.task.clone(),
                    agent: request.agent.clone(),
                    model: request.model.clone(),
                    session: Arc::clone(&session),
                    state: SessionState::Active,
                    result: Some(result_rx),
                    isolation: request.isolation,
                    parent_session_id: parent_session_id.clone(),
                    latest_durable_artifact_id: None,
                    close_completed: false,
                    close_gate: Arc::new(tokio::sync::Mutex::new(())),
                },
            );
        self.spawn_turn(
            handle.clone(),
            parent_session_id,
            session,
            request.task,
            observer,
            cancellation,
            result_tx,
            permit,
        );
        Ok(handle)
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn spawn_turn(
        &self,
        handle: SubagentHandle,
        parent_session_id: SessionId,
        session: Arc<dyn SubagentSession>,
        prompt: String,
        observer: Arc<dyn SubagentObserver>,
        cancellation: CancellationToken,
        result_tx: watch::Sender<Option<Result<SubagentResult, String>>>,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let started = Instant::now();
            let progress: Arc<dyn SubagentProgressObserver> = Arc::new(ObserverProgress {
                observer: Arc::clone(&observer),
                handle: handle.clone(),
            });
            let turn = tokio::select! {
                () = cancellation.cancelled() => {
                    let _ = bounded_cancel(&session, inner.limits).await;
                    Err(OrchestrationError::Session("cancelled".to_owned()))
                },
                result = tokio::time::timeout(
                    inner.limits.max_duration,
                    session.run_turn(prompt, cancellation.clone(), progress),
                ) => if let Ok(result) = result {
                    result
                } else {
                        let _ = bounded_cancel(&session, inner.limits).await;
                        Err(OrchestrationError::Session("timed out".to_owned()))
                },
            };
            if turn.is_err() {
                let _ = bounded_cancel(&session, inner.limits).await;
            }
            let mut result = match turn {
                Ok(mut turn) => {
                    bound_turn_result(&mut turn);
                    SubagentResult {
                        subagent_id: handle.subagent_id.clone(),
                        session_id: handle.session_id.clone(),
                        status: turn.status,
                        final_text: turn.final_text,
                        touched_files: turn.touched_files,
                        diff_artifact: turn.diff_artifact,
                        usage: turn.usage,
                        cost: turn.cost,
                        turns: turn.turns,
                        duration_millis: u64::try_from(started.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                    }
                }
                Err(error) => SubagentResult {
                    subagent_id: handle.subagent_id.clone(),
                    session_id: handle.session_id.clone(),
                    status: if cancellation.is_cancelled() {
                        SubagentStatus::Cancelled
                    } else if started.elapsed() >= inner.limits.max_duration {
                        SubagentStatus::TimedOut
                    } else {
                        SubagentStatus::Failed
                    },
                    final_text: error.to_string(),
                    touched_files: Vec::new(),
                    diff_artifact: None,
                    usage: zero_usage(),
                    cost: Cost::Unavailable {
                        reason: error.to_string(),
                    },
                    turns: 0,
                    duration_millis: u64::try_from(started.elapsed().as_millis())
                        .unwrap_or(u64::MAX),
                },
            };
            if let Some(artifact) = result.diff_artifact.as_ref()
                && let Err(error) = inner.diff_artifact_authority.validate(artifact)
            {
                result.status = SubagentStatus::Failed;
                result.final_text = format!("isolated child returned an invalid diff: {error}");
                result.diff_artifact = None;
            }
            let durable_result = match observer.finished(&result).await {
                Ok(()) => {
                    let grant = result.diff_artifact.as_ref().map_or(Ok(()), |artifact| {
                        inner
                            .diff_artifact_authority
                            .record_durable(parent_session_id.clone(), artifact)
                            .map_err(|error| error.to_string())
                    });
                    grant.map(|()| result)
                }
                Err(error) => Err(error.to_string()),
            };
            {
                let mut sessions = inner
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(record) = sessions.get_mut(&handle.subagent_id) {
                    if let Ok(durable) = &durable_result {
                        record.latest_durable_artifact_id = durable
                            .diff_artifact
                            .as_ref()
                            .map(|artifact| artifact.id.clone());
                        let mut latest = inner
                            .latest_artifacts
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let key = (parent_session_id.clone(), handle.subagent_id.clone());
                        if let Some(artifact) = &durable.diff_artifact {
                            latest.insert(key, artifact.id.clone());
                        } else {
                            latest.remove(&key);
                        }
                    }
                    record.state = SessionState::Inactive;
                }
            }
            let _ = result_tx.send(Some(durable_result));
            drop(permit);
        });
    }

    /// Waits for the currently running turn associated with a handle.
    ///
    /// # Errors
    ///
    /// Returns when the handle has no pending result or its child failed.
    pub async fn wait(
        &self,
        handle: &SubagentHandle,
    ) -> Result<SubagentResult, OrchestrationError> {
        let mut receiver = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle.subagent_id)
            .and_then(|record| record.result.clone())
            .ok_or_else(|| OrchestrationError::NoPendingResult(handle.subagent_id.0.clone()))?;
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result.map_err(OrchestrationError::Session);
            }
            receiver.changed().await.map_err(|_| {
                OrchestrationError::Session("child result channel closed".to_owned())
            })?;
        }
    }

    /// Convenience start-and-wait operation used by the public tool.
    ///
    /// # Errors
    ///
    /// Returns any start, child-session, or durable-observer failure.
    pub async fn spawn(
        &self,
        parent_session_id: SessionId,
        request: SubagentRequest,
        observer: Arc<dyn SubagentObserver>,
        cancellation: CancellationToken,
    ) -> Result<SubagentResult, OrchestrationError> {
        let handle = self
            .start(parent_session_id, request, observer, cancellation)
            .await?;
        self.wait(&handle).await
    }

    /// Sends a follow-up to a completed child while retaining its context/log.
    ///
    /// # Errors
    ///
    /// Returns for unknown/running children, invalid prompts, exhausted concurrency, or failures.
    pub async fn follow_up(
        &self,
        caller_parent_session_id: &SessionId,
        subagent_id: &SubagentId,
        prompt: String,
        observer: Arc<dyn SubagentObserver>,
        cancellation: CancellationToken,
    ) -> Result<SubagentHandle, OrchestrationError> {
        if prompt.trim().is_empty() {
            return Err(OrchestrationError::InvalidRequest(
                "follow-up prompt must not be empty".to_owned(),
            ));
        }
        let (handle, parent_session_id, session, permit) = {
            let mut sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let record = sessions
                .get_mut(subagent_id)
                .ok_or_else(|| OrchestrationError::UnknownSubagent(subagent_id.0.clone()))?;
            ensure_child_owner(caller_parent_session_id, subagent_id, record)?;
            if record.state != SessionState::Inactive {
                return Err(OrchestrationError::AlreadyRunning(subagent_id.0.clone()));
            }
            let permit = Arc::clone(&self.inner.permits)
                .try_acquire_owned()
                .map_err(|_| OrchestrationError::ConcurrencyExceeded {
                    maximum: self.inner.limits.max_concurrency,
                })?;
            record.state = SessionState::Active;
            (
                record.handle.clone(),
                record.parent_session_id.clone(),
                Arc::clone(&record.session),
                permit,
            )
        };
        let (result_tx, result_rx) = watch::channel(None);
        if let Some(record) = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(subagent_id)
        {
            record.result = Some(result_rx);
        }
        if let Err(error) = observer.spawned(&handle, &prompt).await {
            let _ = bounded_cancel(&session, self.inner.limits).await;
            if let Some(record) = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(subagent_id)
            {
                record.state = SessionState::Inactive;
            }
            return Err(error);
        }
        self.spawn_turn(
            handle.clone(),
            parent_session_id,
            session,
            prompt,
            observer,
            cancellation,
            result_tx,
            permit,
        );
        Ok(handle)
    }

    /// Cooperatively cancels one active child.
    ///
    /// # Errors
    ///
    /// Returns when the child is unknown or cancellation fails.
    pub async fn cancel(
        &self,
        caller_parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<(), OrchestrationError> {
        let session = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(subagent_id)
            .ok_or_else(|| OrchestrationError::UnknownSubagent(subagent_id.0.clone()))
            .and_then(|record| {
                ensure_child_owner(caller_parent_session_id, subagent_id, record)?;
                Ok(Arc::clone(&record.session))
            })?;
        bounded_cancel(&session, self.inner.limits).await
    }

    /// Permanently closes a completed child and removes its private recovery metadata.
    ///
    /// # Errors
    ///
    /// Returns for unknown/active children, unsafe worktree finalization, or metadata failure.
    #[allow(clippy::too_many_lines)]
    pub async fn close(
        &self,
        caller_parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<(), OrchestrationError> {
        let close_gate = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(subagent_id)
            .ok_or_else(|| OrchestrationError::UnknownSubagent(subagent_id.0.clone()))
            .and_then(|record| {
                ensure_child_owner(caller_parent_session_id, subagent_id, record)?;
                Ok(Arc::clone(&record.close_gate))
            })?;
        let _close_guard = close_gate.lock().await;
        let (parent_session_id, session, artifact_id, already_finalized) = {
            let mut sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let record = sessions
                .get_mut(subagent_id)
                .ok_or_else(|| OrchestrationError::UnknownSubagent(subagent_id.0.clone()))?;
            ensure_child_owner(caller_parent_session_id, subagent_id, record)?;
            match record.state {
                SessionState::Inactive => record.state = SessionState::Closing,
                SessionState::Closing if record.close_completed => {}
                SessionState::Active | SessionState::Closing => {
                    return Err(OrchestrationError::AlreadyRunning(subagent_id.0.clone()));
                }
            }
            (
                record.parent_session_id.clone(),
                Arc::clone(&record.session),
                record.latest_durable_artifact_id.clone(),
                record.close_completed,
            )
        };
        let durable_artifact = artifact_id
            .as_deref()
            .map(|id| {
                self.inner
                    .diff_artifact_authority
                    .resolve(&parent_session_id, id)
                    .ok_or_else(|| {
                        OrchestrationError::Session(
                            "durable child artifact authority is unavailable".to_owned(),
                        )
                    })
            })
            .transpose();
        let durable_artifact = match durable_artifact {
            Ok(artifact) => artifact,
            Err(error) => {
                if !already_finalized
                    && let Some(record) = self
                        .inner
                        .sessions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get_mut(subagent_id)
                {
                    record.state = SessionState::Inactive;
                }
                return Err(error);
            }
        };
        if !already_finalized {
            if let Err(error) = tokio::time::timeout(
                control_timeout(self.inner.limits),
                session.close(durable_artifact.as_ref()),
            )
            .await
            .map_err(|_| OrchestrationError::Session("child close timed out".to_owned()))
            .and_then(std::convert::identity)
            {
                if let Some(record) = self
                    .inner
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_mut(subagent_id)
                {
                    record.state = SessionState::Inactive;
                }
                return Err(error);
            }
            if let Some(record) = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(subagent_id)
            {
                record.close_completed = true;
            }
        }
        let metadata = self
            .inner
            .metadata
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        metadata.remove(&parent_session_id, subagent_id).await?;
        let removed = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(subagent_id);
        if let Some(record) = removed {
            self.inner
                .session_depths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&record.handle.session_id);
        }
        self.inner
            .latest_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(parent_session_id, subagent_id.clone()));
        Ok(())
    }

    /// Lists retained children owned directly by one parent session.
    #[must_use]
    pub fn list_for_parent(&self, parent_session_id: &SessionId) -> Vec<SubagentDescriptor> {
        let mut descriptors = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|record| record.parent_session_id == *parent_session_id)
            .map(session_record_descriptor)
            .collect::<Vec<_>>();
        descriptors.sort_by(|left, right| left.subagent_id.0.cmp(&right.subagent_id.0));
        descriptors
    }

    /// Resolves one retained child only when it belongs directly to the caller parent.
    ///
    /// # Errors
    ///
    /// Returns the same opaque unknown-child error for missing and cross-parent ids.
    pub fn descriptor_for_parent(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<SubagentDescriptor, OrchestrationError> {
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = sessions
            .get(subagent_id)
            .ok_or_else(|| OrchestrationError::UnknownSubagent(subagent_id.0.clone()))?;
        ensure_child_owner(parent_session_id, subagent_id, record)?;
        Ok(session_record_descriptor(record))
    }

    /// Rebinds a persisted child so follow-up survives a parent process restart.
    ///
    /// # Errors
    ///
    /// Returns for invalid depth, missing recovery data, or factory rebind failure.
    pub async fn recover(
        &self,
        parent_session_id: SessionId,
        handle: SubagentHandle,
        depth: usize,
        workspace_root: &Path,
        worktree: Option<&WorktreeLeaseRecord>,
        policy: &SubagentRecoveryPolicy,
    ) -> Result<(), OrchestrationError> {
        if depth == 0 || depth > self.inner.limits.max_depth {
            return Err(OrchestrationError::DepthExceeded {
                requested: depth,
                maximum: self.inner.limits.max_depth,
            });
        }
        self.ensure_recovery_identity_available(&handle)?;
        let session = self
            .inner
            .factory
            .rebind(
                &handle.session_id,
                Some(workspace_root),
                worktree,
                None,
                policy,
            )
            .await?
            .ok_or_else(|| OrchestrationError::UnknownSubagent(handle.subagent_id.0.clone()))?;
        self.inner
            .session_depths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(handle.session_id.clone(), depth);
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                handle.subagent_id.clone(),
                SessionRecord {
                    handle,
                    task: "Recovered subagent".to_owned(),
                    agent: "subagent".to_owned(),
                    model: policy.model_alias.clone(),
                    session,
                    state: SessionState::Inactive,
                    result: None,
                    isolation: SubagentIsolation::Worktree,
                    parent_session_id,
                    latest_durable_artifact_id: None,
                    close_completed: false,
                    close_gate: Arc::new(tokio::sync::Mutex::new(())),
                },
            );
        Ok(())
    }

    /// Restores one child solely from validated host-private metadata.
    ///
    /// # Errors
    ///
    /// Returns when depth, lease identity, or child-log recovery fails.
    #[allow(clippy::too_many_lines)]
    pub async fn recover_record(
        &self,
        record: SubagentRecoveryRecord,
    ) -> Result<(), OrchestrationError> {
        if record.depth == 0 || record.depth > self.inner.limits.max_depth {
            return Err(OrchestrationError::DepthExceeded {
                requested: record.depth,
                maximum: self.inner.limits.max_depth,
            });
        }
        self.ensure_recovery_identity_available(&record.handle)?;
        let unique_tool_names = record
            .tool_names
            .iter()
            .collect::<std::collections::HashSet<_>>();
        if unique_tool_names.len() != record.tool_names.len() {
            return Err(OrchestrationError::InvalidRequest(
                "recovery tool allowlist contains duplicates".to_owned(),
            ));
        }
        let mut registered_names = Vec::new();
        let mut mcp_grants = Vec::new();
        for name in &record.tool_names {
            if name.starts_with("mcp:") {
                validate_mcp_virtual_tool(name)
                    .map_err(|error| OrchestrationError::InvalidRequest(error.to_string()))?;
                mcp_grants.push(name.clone());
            } else {
                registered_names.push(name.as_str());
            }
        }
        let mcp_policy = McpToolPolicy::restricted(mcp_grants)
            .map_err(|error| OrchestrationError::InvalidRequest(error.to_string()))?;
        let allowed_tools = Arc::new(
            self.tool_registry()
                .subset(registered_names)
                .map_err(|error| OrchestrationError::InvalidRequest(error.to_string()))?
                .with_mcp_tool_policy(mcp_policy),
        );
        let current_capabilities = CapabilityManifest::new(
            allowed_tools
                .descriptors()
                .into_iter()
                .flat_map(|descriptor| descriptor.capabilities.capabilities().to_vec()),
        );
        if current_capabilities != record.capabilities {
            return Err(OrchestrationError::InvalidRequest(
                "recovery capabilities differ from the current tool descriptors".to_owned(),
            ));
        }
        let session = self
            .inner
            .factory
            .rebind(
                &record.handle.session_id,
                Some(&record.workspace_root),
                record.worktree.as_ref(),
                Some(&allowed_tools),
                &record.policy,
            )
            .await?
            .ok_or_else(|| {
                OrchestrationError::UnknownSubagent(record.handle.subagent_id.0.clone())
            })?;
        let latest_durable_artifact_id = self
            .inner
            .latest_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(
                record.parent_session_id.clone(),
                record.handle.subagent_id.clone(),
            ))
            .cloned();
        self.inner
            .session_depths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(record.handle.session_id.clone(), record.depth);
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                record.handle.subagent_id.clone(),
                SessionRecord {
                    handle: record.handle,
                    task: if record.task.is_empty() {
                        "Recovered subagent".to_owned()
                    } else {
                        record.task
                    },
                    agent: if record.agent.is_empty() {
                        "subagent".to_owned()
                    } else {
                        record.agent
                    },
                    model: record.policy.model_alias.clone(),
                    session,
                    state: SessionState::Inactive,
                    result: None,
                    isolation: record.isolation,
                    parent_session_id: record.parent_session_id,
                    latest_durable_artifact_id,
                    close_completed: false,
                    close_gate: Arc::new(tokio::sync::Mutex::new(())),
                },
            );
        Ok(())
    }

    fn ensure_recovery_identity_available(
        &self,
        handle: &SubagentHandle,
    ) -> Result<(), OrchestrationError> {
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if sessions.contains_key(&handle.subagent_id)
            || sessions
                .values()
                .any(|record| record.handle.session_id == handle.session_id)
        {
            return Err(OrchestrationError::InvalidRequest(
                "duplicate recovered child identity".to_owned(),
            ));
        }
        Ok(())
    }

    /// Returns host-private recovery metadata; it must never enter model or parent logs.
    ///
    /// # Errors
    ///
    /// Returns when the child id is unknown.
    pub fn worktree_recovery_record(
        &self,
        subagent_id: &SubagentId,
    ) -> Result<Option<WorktreeLeaseRecord>, OrchestrationError> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(subagent_id)
            .map(|record| record.session.worktree_record())
            .ok_or_else(|| OrchestrationError::UnknownSubagent(subagent_id.0.clone()))
    }
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

fn validate_request(request: &SubagentRequest) -> Result<(), OrchestrationError> {
    if request.task.trim().is_empty() || request.task.len() > 64 * 1024 {
        return Err(OrchestrationError::InvalidRequest(
            "task must be 1-65536 bytes".to_owned(),
        ));
    }
    if request.agent.trim().is_empty() || request.model.trim().is_empty() {
        return Err(OrchestrationError::InvalidRequest(
            "agent and model alias must not be empty".to_owned(),
        ));
    }
    Ok(())
}

fn restricted_registry(
    tools: &Arc<ToolRegistry>,
    requested: &[String],
    mode: SessionMode,
) -> Result<Arc<ToolRegistry>, OrchestrationError> {
    if requested.iter().any(|name| name == "ask_user") {
        return Err(OrchestrationError::InvalidRequest(
            "child agent allowlists cannot include interactive `ask_user`; delegate a bounded non-interactive task"
                .to_owned(),
        ));
    }
    if requested
        .iter()
        .any(|name| matches!(name.as_str(), "tool_search" | "mcp_call"))
    {
        return Err(OrchestrationError::InvalidRequest(
            "child agents must grant exact `mcp:<server>/<tool>` entries instead of generic MCP gateway tools"
                .to_owned(),
        ));
    }
    let mut mcp_grants = Vec::new();
    for name in requested.iter().filter(|name| name.starts_with("mcp:")) {
        validate_mcp_virtual_tool(name)
            .map_err(|error| OrchestrationError::InvalidRequest(error.to_string()))?;
        mcp_grants.push(name.clone());
    }
    if !mcp_grants.is_empty() && mode != SessionMode::Execute {
        return Err(OrchestrationError::InvalidRequest(
            "MCP tools require an execute-mode child because remote mutation capabilities are opaque"
                .to_owned(),
        ));
    }
    let allowed = requested.iter().filter(|name| {
        if name.starts_with("mcp:") {
            return false;
        }
        mode == SessionMode::Execute
            || tools.descriptor(name).is_some_and(|descriptor| {
                descriptor
                    .capabilities
                    .capabilities()
                    .iter()
                    .all(|capability| matches!(capability, ToolCapability::ReadFilesystem))
            })
            || (mode == SessionMode::Plan && name.as_str() == "submit_plan")
    });
    let mut allowed = allowed.map(String::as_str).collect::<Vec<_>>();
    if !mcp_grants.is_empty() {
        allowed.extend(["tool_search", "mcp_call"]);
    }
    let policy = McpToolPolicy::restricted(mcp_grants)
        .map_err(|error| OrchestrationError::InvalidRequest(error.to_string()))?;
    tools
        .subset(allowed)
        .map(|registry| Arc::new(registry.with_mcp_tool_policy(policy)))
        .map_err(|error| OrchestrationError::InvalidRequest(error.to_string()))
}

fn random_id() -> Result<String, OrchestrationError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| {
        OrchestrationError::Session(format!("child id entropy failed: {error}"))
    })?;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(encoded, "{byte:02x}")
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
    }
    Ok(encoded)
}

fn bound_turn_result(result: &mut SubagentTurnResult) {
    truncate_utf8(&mut result.final_text, MAX_SUBAGENT_FINAL_TEXT_BYTES);
    result.touched_files.truncate(MAX_SUBAGENT_TOUCHED_FILES);
    for path in &mut result.touched_files {
        truncate_utf8(path, 4096);
    }
    if result.diff_artifact.as_ref().is_some_and(|diff| {
        diff.unified_diff.len() > MAX_SUBAGENT_DIFF_BYTES
            || diff.touched_files.len() > MAX_SUBAGENT_TOUCHED_FILES
    }) {
        result.diff_artifact = None;
        result.status = SubagentStatus::Failed;
        "isolated child diff exceeded the durable artifact bound"
            .clone_into(&mut result.final_text);
    }
}

fn model_facing_subagent_result(result: &SubagentResult) -> Value {
    let mut final_text = result.final_text.clone();
    let final_text_truncated = final_text.len() > MAX_MODEL_SUBAGENT_TEXT_BYTES;
    truncate_utf8(&mut final_text, MAX_MODEL_SUBAGENT_TEXT_BYTES);
    let mut touched_files = result.touched_files.clone();
    let touched_files_truncated = touched_files.len() > MAX_ARTIFACT_REF_FILES;
    touched_files.truncate(MAX_ARTIFACT_REF_FILES);
    for path in &mut touched_files {
        truncate_utf8(path, MAX_ARTIFACT_REF_PATH_BYTES);
    }
    json!({
        "subagent_id": result.subagent_id,
        "session_id": result.session_id,
        "status": result.status,
        "final_text": final_text,
        "final_text_truncated": final_text_truncated,
        "touched_files": touched_files,
        "touched_files_truncated": touched_files_truncated,
        "diff_artifact": result.diff_artifact.as_ref().map(diff_artifact_reference),
        "usage": result.usage,
        "cost": result.cost,
        "turns": result.turns,
        "duration_millis": result.duration_millis,
    })
}

fn model_facing_subagent_tool_result(result: &SubagentResult) -> ToolResult {
    let mut summary = if result.final_text.is_empty() {
        format!("subagent {} finished", result.subagent_id.0)
    } else {
        result.final_text.clone()
    };
    truncate_utf8(&mut summary, MAX_MODEL_SUBAGENT_SUMMARY_BYTES);
    ToolResult::new(summary, model_facing_subagent_result(result))
}

/// Builds the canonical bounded model-facing reference for a durable diff artifact.
#[must_use]
pub fn diff_artifact_reference(artifact: &DiffArtifact) -> DiffArtifactRef {
    let mut touched_files = artifact.touched_files.clone();
    let manifest_truncated = touched_files.len() > MAX_ARTIFACT_REF_FILES;
    touched_files.truncate(MAX_ARTIFACT_REF_FILES);
    for file in &mut touched_files {
        truncate_utf8(&mut file.path, MAX_ARTIFACT_REF_PATH_BYTES);
    }
    let mut preview = artifact.unified_diff.clone();
    let preview_truncated = preview.len() > MAX_ARTIFACT_REF_PREVIEW_BYTES;
    truncate_utf8(&mut preview, MAX_ARTIFACT_REF_PREVIEW_BYTES);
    DiffArtifactRef {
        artifact_id: artifact.id.clone(),
        base_commit: artifact.base_commit.clone(),
        touched_files,
        manifest_truncated,
        patch_bytes: u64::try_from(artifact.unified_diff.len()).unwrap_or(u64::MAX),
        patch_hash: blake3::hash(artifact.unified_diff.as_bytes())
            .to_hex()
            .to_string(),
        preview,
        preview_truncated,
    }
}

fn truncate_utf8(value: &mut String, limit: usize) {
    if value.len() <= limit {
        return;
    }
    let boundary = value
        .char_indices()
        .take_while(|(index, _)| *index <= limit)
        .last()
        .map_or(0, |(index, _)| index);
    value.truncate(boundary);
}

fn zero_usage() -> Usage {
    Usage {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
    }
}

fn control_timeout(limits: SubagentLimits) -> Duration {
    limits.max_duration.min(Duration::from_secs(30))
}

async fn bounded_cancel(
    session: &Arc<dyn SubagentSession>,
    limits: SubagentLimits,
) -> Result<(), OrchestrationError> {
    tokio::time::timeout(control_timeout(limits), session.cancel())
        .await
        .map_err(|_| OrchestrationError::Session("child cancellation timed out".to_owned()))?
}

async fn bounded_close(
    session: &Arc<dyn SubagentSession>,
    durable_artifact: Option<&DiffArtifact>,
    limits: SubagentLimits,
) -> Result<(), OrchestrationError> {
    tokio::time::timeout(control_timeout(limits), session.close(durable_artifact))
        .await
        .map_err(|_| OrchestrationError::Session("child close timed out".to_owned()))?
}

/// Public registry tool. Depth is derived from the parent session handle, never model input.
pub struct SpawnAgentTool {
    orchestrator: SubagentOrchestrator,
    agents: Arc<AgentRegistry>,
    model: Arc<dyn ModelDriver>,
    capabilities: CapabilityManifest,
}

impl SpawnAgentTool {
    #[must_use]
    pub fn new(
        orchestrator: SubagentOrchestrator,
        agents: Arc<AgentRegistry>,
        model: Arc<dyn ModelDriver>,
    ) -> Self {
        // Spawning, resuming, interrupting, and closing a child are control-plane
        // operations. They do not exercise the child's tool authority. The child
        // receives a fork of the parent's effective permission gate and each tool
        // call is authorized there, exactly once.
        let capabilities = CapabilityManifest::default();
        Self {
            orchestrator,
            agents,
            model,
            capabilities,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SpawnAgentInput {
    #[serde(default)]
    action: Option<SpawnAgentAction>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    isolation: Option<SubagentIsolation>,
    #[serde(default)]
    subagent_id: Option<SubagentId>,
    #[serde(default)]
    follow_up: Option<String>,
}

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SpawnAgentAction {
    Spawn,
    FollowUp,
    Cancel,
    Close,
}

enum NormalizedSpawnAgentAction {
    Spawn {
        task: String,
        agent: String,
        isolation: SubagentIsolation,
    },
    FollowUp {
        subagent_id: SubagentId,
        prompt: String,
    },
    Cancel {
        subagent_id: SubagentId,
    },
    Close {
        subagent_id: SubagentId,
    },
}

fn normalize_spawn_agent_input(
    input: SpawnAgentInput,
) -> Result<NormalizedSpawnAgentAction, ToolError> {
    let action = input.action.unwrap_or_else(|| {
        if input.subagent_id.is_some() && input.follow_up.is_some() {
            SpawnAgentAction::FollowUp
        } else {
            SpawnAgentAction::Spawn
        }
    });
    let invalid = |message: &str| ToolError::InvalidInput(message.to_owned());
    match action {
        SpawnAgentAction::Spawn => {
            if input.subagent_id.is_some() || input.follow_up.is_some() {
                return Err(invalid("spawn forbids subagent_id and follow_up"));
            }
            let task = input.task.ok_or_else(|| invalid("spawn requires task"))?;
            Ok(NormalizedSpawnAgentAction::Spawn {
                task,
                agent: input.agent.unwrap_or_else(|| "general".to_owned()),
                isolation: input.isolation.unwrap_or_default(),
            })
        }
        SpawnAgentAction::FollowUp => {
            if input.task.is_some() || input.agent.is_some() || input.isolation.is_some() {
                return Err(invalid("follow_up forbids task, agent, and isolation"));
            }
            Ok(NormalizedSpawnAgentAction::FollowUp {
                subagent_id: input
                    .subagent_id
                    .ok_or_else(|| invalid("follow_up requires subagent_id"))?,
                prompt: input
                    .follow_up
                    .ok_or_else(|| invalid("follow_up requires a prompt"))?,
            })
        }
        SpawnAgentAction::Cancel | SpawnAgentAction::Close => {
            if input.task.is_some()
                || input.agent.is_some()
                || input.isolation.is_some()
                || input.follow_up.is_some()
            {
                return Err(invalid("cancel/close accepts only action and subagent_id"));
            }
            let subagent_id = input
                .subagent_id
                .ok_or_else(|| invalid("cancel/close requires subagent_id"))?;
            Ok(match action {
                SpawnAgentAction::Cancel => NormalizedSpawnAgentAction::Cancel { subagent_id },
                SpawnAgentAction::Close => NormalizedSpawnAgentAction::Close { subagent_id },
                SpawnAgentAction::Spawn | SpawnAgentAction::FollowUp => unreachable!(),
            })
        }
    }
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "spawn_agent".to_owned(),
            description: "Spawn a restricted full child session, or continue a completed child"
                .to_owned(),
            input_schema: serde_json::to_value(schemars::schema_for!(SpawnAgentInput))
                .unwrap_or(Value::Null),
            capabilities: self.capabilities.clone(),
        }
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    fn subagent_lifecycle_mode(&self) -> SubagentLifecycleMode {
        SubagentLifecycleMode::Single
    }

    fn parallel_safe(&self, input: &Value) -> bool {
        let Ok(input) = serde_json::from_value::<SpawnAgentInput>(input.clone()) else {
            return false;
        };
        let Ok(action) = normalize_spawn_agent_input(input) else {
            return false;
        };
        match action {
            NormalizedSpawnAgentAction::FollowUp { subagent_id, .. }
            | NormalizedSpawnAgentAction::Cancel { subagent_id }
            | NormalizedSpawnAgentAction::Close { subagent_id } => self
                .orchestrator
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&subagent_id)
                .is_some_and(|record| record.isolation == SubagentIsolation::Worktree),
            NormalizedSpawnAgentAction::Spawn {
                agent, isolation, ..
            } => {
                if isolation == SubagentIsolation::Worktree {
                    return true;
                }
                self.agents
                    .load(&agent)
                    .is_ok_and(|agent| agent.permission_mode != SessionMode::Execute)
            }
        }
    }

    fn invocation_capabilities(&self, input: &Value) -> Result<CapabilityManifest, ToolError> {
        let input: SpawnAgentInput = serde_json::from_value(input.clone())
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let action = normalize_spawn_agent_input(input)?;
        match action {
            NormalizedSpawnAgentAction::FollowUp { subagent_id, .. } => {
                self.orchestrator
                    .inner
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&subagent_id)
                    .ok_or_else(|| ToolError::InvalidInput("unknown child session".to_owned()))?;
                Ok(CapabilityManifest::default())
            }
            NormalizedSpawnAgentAction::Cancel { .. }
            | NormalizedSpawnAgentAction::Close { .. } => Ok(self.capabilities.clone()),
            NormalizedSpawnAgentAction::Spawn { agent, .. } => {
                self.agents
                    .load(&agent)
                    .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
                Ok(CapabilityManifest::default())
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let input: SpawnAgentInput = serde_json::from_value(input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let action = normalize_spawn_agent_input(input)?;
        let parent_session_id = context
            .session_id()
            .cloned()
            .ok_or_else(|| ToolError::InvalidInput("spawn_agent requires a session".to_owned()))?;
        let events = context.subagent_event_sink().cloned().ok_or_else(|| {
            ToolError::InvalidInput("spawn_agent requires engine lifecycle routing".to_owned())
        })?;
        let observer: Arc<dyn SubagentObserver> = Arc::new(ToolObserver { events });
        if let NormalizedSpawnAgentAction::Cancel { subagent_id }
        | NormalizedSpawnAgentAction::Close { subagent_id } = &action
        {
            match &action {
                NormalizedSpawnAgentAction::Cancel { .. } => self
                    .orchestrator
                    .cancel(&parent_session_id, subagent_id)
                    .await
                    .map_err(|error| ToolError::Command(error.to_string()))?,
                NormalizedSpawnAgentAction::Close { .. } => self
                    .orchestrator
                    .close(&parent_session_id, subagent_id)
                    .await
                    .map_err(|error| ToolError::Command(error.to_string()))?,
                NormalizedSpawnAgentAction::Spawn { .. }
                | NormalizedSpawnAgentAction::FollowUp { .. } => unreachable!(),
            }
            return Ok(ToolResult::new(
                format!("subagent {} action completed", subagent_id.0),
                json!({
                    "subagent_id": subagent_id,
                    "action": match action {
                        NormalizedSpawnAgentAction::Cancel { .. } => "cancel",
                        NormalizedSpawnAgentAction::Close { .. } => "close",
                        NormalizedSpawnAgentAction::Spawn { .. }
                        | NormalizedSpawnAgentAction::FollowUp { .. } => unreachable!(),
                    },
                    "completed": true,
                }),
            ));
        }
        let result = match action {
            NormalizedSpawnAgentAction::FollowUp {
                subagent_id,
                prompt,
            } => {
                let handle = self
                    .orchestrator
                    .follow_up(
                        &parent_session_id,
                        &subagent_id,
                        prompt,
                        observer,
                        context.cancellation.clone(),
                    )
                    .await
                    .map_err(|error| ToolError::Command(error.to_string()))?;
                self.orchestrator
                    .wait(&handle)
                    .await
                    .map_err(|error| ToolError::Command(error.to_string()))?
            }
            NormalizedSpawnAgentAction::Spawn {
                task,
                agent: agent_name,
                isolation,
            } => {
                let loaded = self
                    .agents
                    .load(&agent_name)
                    .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
                let inherited_model = context.model_alias().ok_or_else(|| {
                    ToolError::InvalidInput(
                        "spawn_agent requires the parent turn's selected model".to_owned(),
                    )
                })?;
                let resolved_model = loaded.model.as_deref().unwrap_or(inherited_model);
                if !self.model.has_model_alias(resolved_model) {
                    return Err(ToolError::InvalidInput(format!(
                        "agent `{agent_name}` selects unconfigured model alias `{resolved_model}`"
                    )));
                }
                let request = SubagentRequest::from_loaded_agent(
                    task,
                    loaded,
                    inherited_model,
                    context.workspace_root().to_path_buf(),
                );
                let request = SubagentRequest {
                    isolation,
                    ..request
                };
                self.orchestrator
                    .spawn(
                        parent_session_id,
                        request,
                        observer,
                        context.cancellation.clone(),
                    )
                    .await
                    .map_err(|error| ToolError::Command(error.to_string()))?
            }
            NormalizedSpawnAgentAction::Cancel { .. }
            | NormalizedSpawnAgentAction::Close { .. } => unreachable!(),
        };
        Ok(model_facing_subagent_tool_result(&result))
    }
}

struct ToolObserver {
    events: Arc<dyn SubagentEventSink>,
}

#[async_trait]
impl SubagentObserver for ToolObserver {
    async fn spawned(&self, handle: &SubagentHandle, task: &str) -> Result<(), OrchestrationError> {
        self.events
            .lifecycle(SubagentLifecycleEvent::Spawned {
                subagent_id: handle.subagent_id.clone(),
                child_session_id: handle.session_id.clone(),
                task: task.to_owned(),
            })
            .await
            .map_err(|error| OrchestrationError::Observer(error.to_string()))
    }

    async fn finished(&self, result: &SubagentResult) -> Result<(), OrchestrationError> {
        self.events
            .lifecycle(SubagentLifecycleEvent::Finished {
                subagent_id: result.subagent_id.clone(),
                result: Box::new(result.clone()),
            })
            .await
            .map_err(|error| OrchestrationError::Observer(error.to_string()))
    }

    async fn progress(
        &self,
        handle: &SubagentHandle,
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
        self.events
            .progress(SubagentProgressEvent {
                subagent_id: handle.subagent_id.clone(),
                child_session_id: handle.session_id.clone(),
                child_sequence,
                event,
            })
            .await
            .map_err(|error| OrchestrationError::Observer(error.to_string()))
    }
}

/// Canonical complete child result used by every durable lifecycle bridge.
#[must_use]
pub fn subagent_result_tool_output(result: &SubagentResult) -> ToolOutput {
    let bounded = model_facing_subagent_tool_result(result);
    ToolOutput::Mixed {
        parts: vec![
            ToolOutputPart::Text {
                text: bounded.content,
            },
            ToolOutputPart::Structured {
                value: bounded.data,
            },
        ],
    }
}

/// Factory for production child actors. The builder supplies a distinct event
/// sink and context; core overwrites security-sensitive launch fields.
type ActorConfigBuilder =
    dyn Fn(&SubagentLaunch) -> Result<SessionActorConfig, AgentLoopError> + Send + Sync;
type ActorResumeBuilder = dyn Fn(&SessionId, &Path, &SubagentRecoveryPolicy) -> Result<SessionActorConfig, AgentLoopError>
    + Send
    + Sync;

pub struct ActorSubagentSessionFactory {
    builder: Arc<ActorConfigBuilder>,
    rebuilder: Option<Arc<ActorResumeBuilder>>,
}

/// Isolation wrapper for the production actor factory. A lease remains bound
/// to the continuable child session; each completed turn refreshes its typed
/// diff artifact without mutating the parent tree.
pub struct WorktreeSubagentSessionFactory {
    inner: Arc<dyn SubagentSessionFactory>,
    isolation: Arc<WorktreeIsolation>,
}

impl WorktreeSubagentSessionFactory {
    #[must_use]
    pub fn new(inner: Arc<dyn SubagentSessionFactory>, isolation: Arc<WorktreeIsolation>) -> Self {
        Self { inner, isolation }
    }
}

#[async_trait]
impl SubagentSessionFactory for WorktreeSubagentSessionFactory {
    async fn create(
        &self,
        mut launch: SubagentLaunch,
    ) -> Result<Arc<dyn SubagentSession>, OrchestrationError> {
        if launch.request.isolation == SubagentIsolation::Shared {
            return self.inner.create(launch).await;
        }
        let lease = Arc::new(
            self.isolation
                .create(launch.cancellation.clone())
                .await
                .map_err(|error| OrchestrationError::Session(error.to_string()))?,
        );
        launch.workspace_root = lease.path().to_path_buf();
        let inner = match self.inner.create(launch).await {
            Ok(inner) => inner,
            Err(error) => {
                let _ = self
                    .isolation
                    .cleanup_if_untouched(&lease, CancellationToken::default())
                    .await;
                return Err(error);
            }
        };
        Ok(Arc::new(WorktreeSubagentSession {
            inner,
            isolation: Arc::clone(&self.isolation),
            lease,
        }))
    }

    async fn rebind(
        &self,
        session_id: &SessionId,
        workspace_root: Option<&Path>,
        worktree: Option<&WorktreeLeaseRecord>,
        allowed_tools: Option<&ToolRegistry>,
        policy: &SubagentRecoveryPolicy,
    ) -> Result<Option<Arc<dyn SubagentSession>>, OrchestrationError> {
        let Some(record) = worktree else {
            return self
                .inner
                .rebind(session_id, workspace_root, None, allowed_tools, policy)
                .await;
        };
        let lease = Arc::new(
            self.isolation
                .rebind(record, CancellationToken::default())
                .await
                .map_err(|error| OrchestrationError::Session(error.to_string()))?,
        );
        let Some(inner) = self
            .inner
            .rebind(session_id, Some(lease.path()), None, allowed_tools, policy)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(Arc::new(WorktreeSubagentSession {
            inner,
            isolation: Arc::clone(&self.isolation),
            lease,
        })))
    }
}

struct WorktreeSubagentSession {
    inner: Arc<dyn SubagentSession>,
    isolation: Arc<WorktreeIsolation>,
    lease: Arc<WorktreeLease>,
}

#[async_trait]
impl SubagentSession for WorktreeSubagentSession {
    fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }

    async fn run_turn(
        &self,
        prompt: String,
        cancellation: CancellationToken,
        progress: Arc<dyn SubagentProgressObserver>,
    ) -> Result<SubagentTurnResult, OrchestrationError> {
        let mut result = self
            .inner
            .run_turn(prompt, cancellation.clone(), progress)
            .await?;
        let artifact = self
            .isolation
            .collect(
                &self.lease,
                &result.final_text,
                result.usage.clone(),
                result.cost.clone(),
                cancellation,
            )
            .await
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        result.final_text = artifact.final_text;
        result.touched_files = artifact
            .touched_files
            .iter()
            .map(|file| file.path.clone())
            .collect();
        result.diff_artifact = artifact.diff;
        Ok(result)
    }

    async fn cancel(&self) -> Result<(), OrchestrationError> {
        self.inner.cancel().await
    }

    fn worktree_record(&self) -> Option<WorktreeLeaseRecord> {
        Some(self.lease.durable_record())
    }

    async fn close(
        &self,
        durable_artifact: Option<&DiffArtifact>,
    ) -> Result<(), OrchestrationError> {
        self.inner.close(None).await?;
        let removed = if let Some(artifact) = durable_artifact {
            self.isolation
                .finalize_captured(&self.lease, artifact, CancellationToken::default())
                .await
                .map_err(|error| OrchestrationError::Session(error.to_string()))?
        } else {
            self.isolation
                .cleanup_if_untouched(&self.lease, CancellationToken::default())
                .await
                .map_err(|error| OrchestrationError::Session(error.to_string()))?
        };
        if !removed {
            return Err(OrchestrationError::Session(
                "worktree changed after its latest durable artifact; child was not closed"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

impl ActorSubagentSessionFactory {
    #[must_use]
    pub fn new(
        builder: impl Fn(&SubagentLaunch) -> Result<SessionActorConfig, AgentLoopError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            builder: Arc::new(builder),
            rebuilder: None,
        }
    }

    /// Adds the host-specific recovery builder. It must reopen the child log
    /// and rebuild every dependency bound to the supplied root.
    #[must_use]
    pub fn with_rebuilder(
        mut self,
        rebuilder: impl Fn(
            &SessionId,
            &Path,
            &SubagentRecoveryPolicy,
        ) -> Result<SessionActorConfig, AgentLoopError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.rebuilder = Some(Arc::new(rebuilder));
        self
    }
}

#[async_trait]
impl SubagentSessionFactory for ActorSubagentSessionFactory {
    async fn create(
        &self,
        launch: SubagentLaunch,
    ) -> Result<Arc<dyn SubagentSession>, OrchestrationError> {
        let mut config = (self.builder)(&launch)
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        config.session_id = launch.handle.session_id.clone();
        config.workspace_root.clone_from(&launch.workspace_root);
        config.additional_workspace_roots.clear();
        config.model_alias.clone_from(&launch.request.model);
        config.tools = Arc::new(bind_child_tools(&config.tools, &launch.tools)?);
        apply_child_policy(
            &mut config,
            &launch.request.model,
            launch.request.system_prompt.as_deref(),
            launch.request.permission_mode,
            launch.max_turns,
        );
        let handle = SessionActor::spawn(config)
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        Ok(Arc::new(ActorSubagentSession { handle }))
    }

    async fn rebind(
        &self,
        session_id: &SessionId,
        workspace_root: Option<&Path>,
        _worktree: Option<&WorktreeLeaseRecord>,
        allowed_tools: Option<&ToolRegistry>,
        policy: &SubagentRecoveryPolicy,
    ) -> Result<Option<Arc<dyn SubagentSession>>, OrchestrationError> {
        let (Some(rebuilder), Some(workspace_root)) = (&self.rebuilder, workspace_root) else {
            return Ok(None);
        };
        let mut config = rebuilder(session_id, workspace_root, policy)
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        config.session_id = session_id.clone();
        config.workspace_root = workspace_root.to_path_buf();
        config.additional_workspace_roots.clear();
        if let Some(allowed_tools) = allowed_tools {
            config.tools = Arc::new(bind_child_tools(&config.tools, allowed_tools)?);
        }
        apply_child_policy(
            &mut config,
            &policy.model_alias,
            policy.system_prompt.as_deref(),
            policy.permission_mode,
            policy.max_turns,
        );
        let handle = SessionActor::spawn(config)
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        Ok(Some(Arc::new(ActorSubagentSession { handle })))
    }
}

fn apply_child_policy(
    config: &mut SessionActorConfig,
    model_alias: &str,
    system_prompt: Option<&str>,
    permission_mode: SessionMode,
    max_turns: usize,
) {
    model_alias.clone_into(&mut config.model_alias);
    config.max_turns = config.max_turns.min(max_turns).max(1);
    config.recovered.mode = permission_mode;
    config.recovered.plan_gate_active = permission_mode == SessionMode::Plan;
    let mode_prompt = match permission_mode {
        SessionMode::Discuss => {
            "Child permission mode: discuss. Use only read-only tools and do not mutate the workspace."
        }
        SessionMode::Plan => {
            "Child permission mode: plan. Use only read-only tools and return a structured plan."
        }
        SessionMode::Execute => {
            "Child permission mode: execute. Use the exact tool grant selected by the parent and the parent session's effective permission policy."
        }
    };
    if let Some(system_prompt) = system_prompt.filter(|prompt| !prompt.trim().is_empty()) {
        if let Some(system) = config
            .initial_session_context
            .iter_mut()
            .find(|turn| turn.role == Role::System)
        {
            system.blocks.push(Block::Text {
                text: system_prompt.to_owned(),
            });
        } else {
            config.initial_session_context.insert(
                0,
                Turn {
                    role: Role::System,
                    blocks: vec![Block::Text {
                        text: system_prompt.to_owned(),
                    }],
                    meta: TurnMeta::default(),
                },
            );
        }
    }
    if let Some(system) = config
        .initial_session_context
        .iter_mut()
        .find(|turn| turn.role == Role::System)
    {
        system.blocks.push(Block::Text {
            text: mode_prompt.to_owned(),
        });
    }
}

fn bind_child_tools(
    root_bound: &ToolRegistry,
    allowed: &ToolRegistry,
) -> Result<ToolRegistry, OrchestrationError> {
    let mut child = ToolRegistry::new();
    for approved in allowed.descriptors() {
        let tool = if let Some(tool) = root_bound.resolve(&approved.name) {
            tool
        } else {
            let fallback = allowed.resolve(&approved.name).ok_or_else(|| {
                OrchestrationError::InvalidRequest(format!(
                    "approved child tool `{}` disappeared during binding",
                    approved.name
                ))
            })?;
            if fallback.workspace_binding() != WorkspaceBinding::RootIndependent {
                return Err(OrchestrationError::InvalidRequest(format!(
                    "root-bound child tool `{}` was not rebuilt for the child workspace",
                    approved.name
                )));
            }
            fallback
        };
        let actual = tool.descriptor();
        if actual.capabilities != approved.capabilities {
            return Err(OrchestrationError::InvalidRequest(format!(
                "child tool `{}` capability manifest changed during root binding",
                approved.name
            )));
        }
        child
            .register(tool)
            .map_err(|error| OrchestrationError::InvalidRequest(error.to_string()))?;
    }
    Ok(child.with_mcp_tool_policy(allowed.mcp_tool_policy().clone()))
}

struct ActorSubagentSession {
    handle: SessionHandle,
}

#[async_trait]
impl SubagentSession for ActorSubagentSession {
    fn session_id(&self) -> &SessionId {
        self.handle.session_id()
    }

    async fn run_turn(
        &self,
        prompt: String,
        cancellation: CancellationToken,
        progress: Arc<dyn SubagentProgressObserver>,
    ) -> Result<SubagentTurnResult, OrchestrationError> {
        let mut subscription = self.handle.subscribe();
        self.handle
            .send_message(prompt)
            .await
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        let mut final_text = String::new();
        loop {
            let event = tokio::select! {
                () = cancellation.cancelled() => {
                    let _ = self.handle.interrupt().await;
                    return Err(OrchestrationError::Session("cancelled".to_owned()));
                }
                event = subscription.recv() => event
                    .map_err(|error| OrchestrationError::Session(error.to_string()))?,
            };
            let sequence = event.meta().map(|meta| meta.sequence_id.0);
            if let EngineEvent::TextDelta { text, .. } = &event {
                final_text.push_str(text);
            }
            let encoded = serde_json::to_value(&event)
                .map_err(|error| OrchestrationError::Session(error.to_string()))?;
            progress.progress(sequence, encoded).await?;
            if let EngineEvent::TurnFinished {
                status,
                usage,
                cost,
                ..
            } = event
            {
                return Ok(SubagentTurnResult {
                    status: subagent_status(&status),
                    final_text,
                    touched_files: Vec::new(),
                    diff_artifact: None,
                    usage,
                    cost,
                    turns: 1,
                });
            }
        }
    }

    async fn cancel(&self) -> Result<(), OrchestrationError> {
        self.handle
            .interrupt()
            .await
            .map(|_| ())
            .map_err(|error| OrchestrationError::Session(error.to_string()))
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct SelectedModel;

    impl ModelDriver for SelectedModel {
        fn stream(
            &self,
            _alias: &str,
            _request: rw_providers::ProviderRequest,
        ) -> Result<rw_providers::BoxEventStream, AgentLoopError> {
            Err(AgentLoopError::Provider(
                "selected-model fixture must not stream".to_owned(),
            ))
        }

        fn has_model_alias(&self, alias: &str) -> bool {
            alias == "openai_codex/gpt-5.6-sol"
        }
    }

    #[derive(Default)]
    struct RecordingSubagentSink {
        lifecycles: Mutex<Vec<SubagentLifecycleEvent>>,
    }

    #[async_trait]
    impl SubagentEventSink for RecordingSubagentSink {
        async fn lifecycle(&self, event: SubagentLifecycleEvent) -> Result<(), ToolError> {
            self.lifecycles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
            Ok(())
        }

        async fn progress(&self, _event: SubagentProgressEvent) -> Result<(), ToolError> {
            Ok(())
        }
    }

    struct RejectingApprover(AtomicUsize);

    #[async_trait]
    impl crate::PermissionApprover for RejectingApprover {
        async fn decide(&self, _request: crate::PermissionRequest) -> rw_types::ApprovalDecision {
            self.0.fetch_add(1, Ordering::SeqCst);
            rw_types::ApprovalDecision::Deny
        }
    }

    #[derive(Default)]
    struct FakeFactory {
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        cancelled: Arc<AtomicUsize>,
        hang_cancel: bool,
        closed_artifacts: Arc<Mutex<Vec<Option<String>>>>,
        fail_close: bool,
        launches: Arc<Mutex<Vec<SubagentRequest>>>,
    }

    struct FakeSession {
        session_id: SessionId,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        cancelled: Arc<AtomicUsize>,
        history: Mutex<Vec<String>>,
        hang_cancel: bool,
        closed_artifacts: Arc<Mutex<Vec<Option<String>>>>,
        fail_close: bool,
    }

    struct NoopProgress;

    #[async_trait]
    impl SubagentProgressObserver for NoopProgress {
        async fn progress(
            &self,
            _child_sequence: Option<u64>,
            _event: Value,
        ) -> Result<(), OrchestrationError> {
            Ok(())
        }
    }

    #[async_trait]
    impl SubagentSessionFactory for FakeFactory {
        async fn create(
            &self,
            launch: SubagentLaunch,
        ) -> Result<Arc<dyn SubagentSession>, OrchestrationError> {
            self.launches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(launch.request.clone());
            Ok(Arc::new(FakeSession {
                session_id: launch.handle.session_id,
                active: Arc::clone(&self.active),
                peak: Arc::clone(&self.peak),
                cancelled: Arc::clone(&self.cancelled),
                history: Mutex::new(Vec::new()),
                hang_cancel: self.hang_cancel,
                closed_artifacts: Arc::clone(&self.closed_artifacts),
                fail_close: self.fail_close,
            }))
        }

        async fn rebind(
            &self,
            session_id: &SessionId,
            _workspace_root: Option<&Path>,
            _worktree: Option<&WorktreeLeaseRecord>,
            _allowed_tools: Option<&ToolRegistry>,
            _policy: &SubagentRecoveryPolicy,
        ) -> Result<Option<Arc<dyn SubagentSession>>, OrchestrationError> {
            Ok(Some(Arc::new(FakeSession {
                session_id: session_id.clone(),
                active: Arc::clone(&self.active),
                peak: Arc::clone(&self.peak),
                cancelled: Arc::clone(&self.cancelled),
                history: Mutex::new(Vec::new()),
                hang_cancel: self.hang_cancel,
                closed_artifacts: Arc::clone(&self.closed_artifacts),
                fail_close: self.fail_close,
            })))
        }
    }

    #[async_trait]
    impl SubagentSession for FakeSession {
        fn session_id(&self) -> &SessionId {
            &self.session_id
        }

        async fn run_turn(
            &self,
            prompt: String,
            cancellation: CancellationToken,
            progress: Arc<dyn SubagentProgressObserver>,
        ) -> Result<SubagentTurnResult, OrchestrationError> {
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.peak.fetch_max(active, Ordering::AcqRel);
            let delay = prompt
                .strip_prefix("delay:")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(1);
            tokio::select! {
                () = cancellation.cancelled() => {
                    self.active.fetch_sub(1, Ordering::AcqRel);
                    return Err(OrchestrationError::Session("cancelled".to_owned()));
                }
                () = tokio::time::sleep(Duration::from_millis(delay)) => {}
            }
            progress
                .progress(Some(0), json!({"type":"text_delta","text":prompt}))
                .await?;
            let invalid_artifact = prompt == "invalid-artifact";
            let valid_artifact = prompt == "valid-artifact";
            let count = {
                let mut history = self
                    .history
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                history.push(prompt);
                history.len()
            };
            self.active.fetch_sub(1, Ordering::AcqRel);
            let diff_artifact = if invalid_artifact {
                let mut artifact = test_artifact();
                artifact.id = "0".repeat(64);
                Some(artifact)
            } else if valid_artifact {
                Some(test_artifact())
            } else {
                None
            };
            Ok(SubagentTurnResult {
                status: SubagentStatus::Completed,
                final_text: format!("history:{count}"),
                touched_files: Vec::new(),
                diff_artifact,
                usage: zero_usage(),
                cost: Cost::Unavailable {
                    reason: "fixture".to_owned(),
                },
                turns: 1,
            })
        }

        async fn cancel(&self) -> Result<(), OrchestrationError> {
            self.cancelled.fetch_add(1, Ordering::Relaxed);
            if self.hang_cancel {
                std::future::pending::<()>().await;
            }
            Ok(())
        }

        async fn close(
            &self,
            durable_artifact: Option<&DiffArtifact>,
        ) -> Result<(), OrchestrationError> {
            self.cancel().await?;
            if self.fail_close {
                return Err(OrchestrationError::Session(
                    "fixture close failed".to_owned(),
                ));
            }
            self.closed_artifacts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(durable_artifact.map(|artifact| artifact.id.clone()));
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingObserver {
        events: Mutex<Vec<String>>,
        results: Mutex<Vec<SubagentResult>>,
        fail_finished: bool,
        fail_spawned: bool,
    }

    struct FailingMetadataStore;

    #[derive(Default)]
    struct FailingPromotionMetadataStore {
        saves: AtomicUsize,
        retained: Mutex<Option<SubagentRecoveryRecord>>,
    }

    #[derive(Default)]
    struct FailOnceRemoveMetadataStore {
        removes: AtomicUsize,
    }

    #[derive(Default)]
    struct RecordingMetadataStore {
        record: Mutex<Option<SubagentRecoveryRecord>>,
        removes: AtomicUsize,
    }

    #[async_trait]
    impl SubagentMetadataStore for FailingMetadataStore {
        async fn save(&self, _record: SubagentRecoveryRecord) -> Result<(), OrchestrationError> {
            Err(OrchestrationError::Session(
                "metadata persistence failed".to_owned(),
            ))
        }

        async fn remove(
            &self,
            _parent_session_id: &SessionId,
            _subagent_id: &SubagentId,
        ) -> Result<(), OrchestrationError> {
            Ok(())
        }
    }

    #[async_trait]
    impl SubagentMetadataStore for FailingPromotionMetadataStore {
        async fn save(&self, record: SubagentRecoveryRecord) -> Result<(), OrchestrationError> {
            if self.saves.fetch_add(1, Ordering::AcqRel) == 0 {
                *self
                    .retained
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(record);
                Ok(())
            } else {
                Err(OrchestrationError::Session(
                    "metadata promotion failed".to_owned(),
                ))
            }
        }

        async fn remove(
            &self,
            _parent_session_id: &SessionId,
            _subagent_id: &SubagentId,
        ) -> Result<(), OrchestrationError> {
            Ok(())
        }
    }

    #[async_trait]
    impl SubagentMetadataStore for FailOnceRemoveMetadataStore {
        async fn save(&self, _record: SubagentRecoveryRecord) -> Result<(), OrchestrationError> {
            Ok(())
        }

        async fn remove(
            &self,
            _parent_session_id: &SessionId,
            _subagent_id: &SubagentId,
        ) -> Result<(), OrchestrationError> {
            if self.removes.fetch_add(1, Ordering::AcqRel) == 0 {
                Err(OrchestrationError::Session(
                    "fixture metadata remove failed".to_owned(),
                ))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl SubagentMetadataStore for RecordingMetadataStore {
        async fn save(&self, record: SubagentRecoveryRecord) -> Result<(), OrchestrationError> {
            *self
                .record
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(record);
            Ok(())
        }

        async fn remove(
            &self,
            _parent_session_id: &SessionId,
            _subagent_id: &SubagentId,
        ) -> Result<(), OrchestrationError> {
            self.removes.fetch_add(1, Ordering::AcqRel);
            *self
                .record
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            Ok(())
        }
    }

    #[async_trait]
    impl SubagentObserver for RecordingObserver {
        async fn spawned(
            &self,
            handle: &SubagentHandle,
            _task: &str,
        ) -> Result<(), OrchestrationError> {
            if self.fail_spawned {
                return Err(OrchestrationError::Observer(
                    "spawn fixture failure".to_owned(),
                ));
            }
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("spawn:{}", handle.subagent_id.0));
            Ok(())
        }

        async fn finished(&self, result: &SubagentResult) -> Result<(), OrchestrationError> {
            if self.fail_finished {
                return Err(OrchestrationError::Observer("fixture failure".to_owned()));
            }
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("finish:{}", result.subagent_id.0));
            self.results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(result.clone());
            Ok(())
        }

        async fn progress(
            &self,
            _handle: &SubagentHandle,
            _child_sequence: Option<u64>,
            _event: Value,
        ) -> Result<(), OrchestrationError> {
            Ok(())
        }
    }

    fn request(task: &str) -> SubagentRequest {
        SubagentRequest {
            task: task.to_owned(),
            agent: "fixture".to_owned(),
            model: "fast".to_owned(),
            tools: Vec::new(),
            system_prompt: Some("fixture".to_owned()),
            permission_mode: SessionMode::Execute,
            max_turns: Some(4),
            isolation: SubagentIsolation::Shared,
            workspace_root: std::env::current_dir().expect("cwd"),
        }
    }

    fn test_artifact() -> DiffArtifact {
        let base_commit = "1".repeat(40);
        let touched_files = vec![rw_types::TouchedFile {
            path: "src/lib.rs".to_owned(),
            status: rw_types::TouchedFileStatus::Modified,
        }];
        let unified_diff = "diff --git a/src/lib.rs b/src/lib.rs\n".to_owned();
        let manifest = serde_json::to_vec(&touched_files).expect("manifest");
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"rottweiler.worktree-diff.v1\0");
        hasher.update(base_commit.as_bytes());
        hasher.update(b"\0");
        hasher.update(&manifest);
        hasher.update(b"\0");
        hasher.update(unified_diff.as_bytes());
        DiffArtifact {
            id: hasher.finalize().to_hex().to_string(),
            base_commit,
            touched_files,
            unified_diff,
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn cancelled_worktree_lease_rebinds_and_accepts_follow_up() {
        use std::process::Command;

        let repository = tempfile::tempdir().expect("repository");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(repository.path())
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "Rottweiler Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Rottweiler Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .output()
                .expect("git");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);
        std::fs::write(repository.path().join("tracked.txt"), b"base\n").expect("tracked");
        git(&["add", "tracked.txt"]);
        git(&["commit", "--quiet", "-m", "base"]);
        let private = tempfile::tempdir().expect("private");
        let isolation = Arc::new(
            WorktreeIsolation::new(
                repository.path(),
                private.path(),
                rw_tools::WorktreeLimits::default(),
                CancellationToken::default(),
            )
            .await
            .expect("isolation"),
        );
        let inner: Arc<dyn SubagentSessionFactory> = Arc::new(FakeFactory::default());
        let factory = WorktreeSubagentSessionFactory::new(inner, Arc::clone(&isolation));
        let handle = SubagentHandle {
            subagent_id: SubagentId("cancelled".to_owned()),
            session_id: SessionId("cancelled-session".to_owned()),
        };
        let mut child_request = request("first");
        child_request.isolation = SubagentIsolation::Worktree;
        child_request.workspace_root = repository.path().to_path_buf();
        let session = factory
            .create(SubagentLaunch {
                handle: handle.clone(),
                parent_session_id: SessionId("parent".to_owned()),
                depth: 1,
                request: child_request,
                tools: Arc::new(ToolRegistry::new()),
                max_turns: 4,
                workspace_root: repository.path().to_path_buf(),
                cancellation: CancellationToken::default(),
            })
            .await
            .expect("create worktree child");
        let record = session.worktree_record().expect("durable lease");
        session.cancel().await.expect("cancel child only");
        isolation
            .rebind(&record, CancellationToken::default())
            .await
            .expect("cancel preserved lease");
        drop(session);

        let rebound = factory
            .rebind(
                &handle.session_id,
                Some(repository.path()),
                Some(&record),
                None,
                &SubagentRecoveryPolicy {
                    model_alias: "fast".to_owned(),
                    system_prompt: None,
                    permission_mode: SessionMode::Execute,
                    max_turns: 4,
                },
            )
            .await
            .expect("rebind")
            .expect("rebound session");
        let result = rebound
            .run_turn(
                "follow-up".to_owned(),
                CancellationToken::default(),
                Arc::new(NoopProgress),
            )
            .await
            .expect("follow-up turn");
        assert_eq!(result.status, SubagentStatus::Completed);
        rebound.close(None).await.expect("close rebound child");
        assert!(
            isolation
                .rebind(&record, CancellationToken::default())
                .await
                .is_err()
        );
    }

    fn rehash_test_artifact(artifact: &mut DiffArtifact) {
        let manifest = serde_json::to_vec(&artifact.touched_files).expect("manifest");
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"rottweiler.worktree-diff.v1\0");
        hasher.update(artifact.base_commit.as_bytes());
        hasher.update(b"\0");
        hasher.update(&manifest);
        hasher.update(b"\0");
        hasher.update(artifact.unified_diff.as_bytes());
        artifact.id = hasher.finalize().to_hex().to_string();
    }

    fn recovery_record(subagent: &str, session: &str) -> SubagentRecoveryRecord {
        SubagentRecoveryRecord {
            parent_session_id: SessionId("parent".to_owned()),
            handle: SubagentHandle {
                subagent_id: SubagentId(subagent.to_owned()),
                session_id: SessionId(session.to_owned()),
            },
            task: "fixture task".to_owned(),
            agent: "fixture agent".to_owned(),
            depth: 1,
            workspace_root: std::env::current_dir().expect("cwd"),
            isolation: SubagentIsolation::Shared,
            worktree: None,
            capabilities: CapabilityManifest::default(),
            tool_names: Vec::new(),
            policy: SubagentRecoveryPolicy {
                model_alias: "fast".to_owned(),
                system_prompt: Some("fixture".to_owned()),
                permission_mode: SessionMode::Execute,
                max_turns: 4,
            },
            phase: SubagentRecoveryPhase::Active,
        }
    }

    fn test_event_meta(sequence: u64) -> rw_types::EventMeta {
        rw_types::EventMeta {
            protocol_version: rw_types::PROTOCOL_VERSION,
            session_id: SessionId("parent".to_owned()),
            sequence_id: rw_types::SequenceId(sequence),
            emitted_at: "2026-01-01T00:00:00Z".to_owned(),
            caused_by: None,
        }
    }

    fn orchestrator(limits: SubagentLimits, factory: Arc<FakeFactory>) -> SubagentOrchestrator {
        SubagentOrchestrator::new(limits, factory, Arc::new(ToolRegistry::new()))
            .expect("orchestrator")
    }

    struct MutatingTool;

    struct FixedResultTool {
        result: ToolResult,
    }

    struct GatewayTool(&'static str);

    #[async_trait]
    impl Tool for GatewayTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: self.0.to_owned(),
                description: "MCP gateway fixture".to_owned(),
                input_schema: Value::Null,
                capabilities: CapabilityManifest::new([
                    ToolCapability::Network,
                    ToolCapability::Execute,
                ]),
            }
        }

        fn workspace_binding(&self) -> WorkspaceBinding {
            WorkspaceBinding::RootIndependent
        }

        async fn execute(
            &self,
            _context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            panic!("gateway fixture must not execute")
        }
    }

    #[async_trait]
    impl Tool for MutatingTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "write".to_owned(),
                description: "fixture mutation".to_owned(),
                input_schema: Value::Null,
                capabilities: CapabilityManifest::new([ToolCapability::WriteFilesystem]),
            }
        }

        async fn execute(
            &self,
            _context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            panic!("restricted child must never execute mutating fixture")
        }
    }

    #[async_trait]
    impl Tool for FixedResultTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "fixed_result".to_owned(),
                description: "fixture".to_owned(),
                input_schema: Value::Null,
                capabilities: CapabilityManifest::default(),
            }
        }

        async fn execute(
            &self,
            _context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            Ok(self.result.clone())
        }
    }

    #[test]
    fn non_execute_children_filter_mutations_and_reject_interaction() {
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MutatingTool)).expect("register");
        let tools = Arc::new(tools);
        let discuss = restricted_registry(&tools, &["write".to_owned()], SessionMode::Discuss)
            .expect("discuss subset");
        assert!(discuss.is_empty());
        let error = restricted_registry(&tools, &["ask_user".to_owned()], SessionMode::Execute)
            .err()
            .expect("interactive child tool must fail");
        assert!(error.to_string().contains("cannot include interactive"));
        let missing_root_bound = bind_child_tools(&ToolRegistry::new(), &tools)
            .err()
            .expect("root-bound fallback must fail");
        assert!(missing_root_bound.to_string().contains("was not rebuilt"));
    }

    #[test]
    fn child_mcp_virtual_tools_mint_only_exact_gateway_authority() {
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(GatewayTool("tool_search")))
            .expect("search gateway");
        tools
            .register(Arc::new(GatewayTool("mcp_call")))
            .expect("call gateway");
        let tools = Arc::new(tools);

        let restricted = restricted_registry(
            &tools,
            &["mcp:github/get_issue".to_owned()],
            SessionMode::Execute,
        )
        .expect("exact MCP policy");
        assert!(restricted.descriptor("tool_search").is_some());
        assert!(restricted.descriptor("mcp_call").is_some());
        assert!(restricted.mcp_tool_policy().allows("github", "get_issue"));
        assert!(
            !restricted
                .mcp_tool_policy()
                .allows("github", "delete_issue")
        );

        for invalid in ["mcp:github/*", "tool_search", "mcp_call"] {
            assert!(
                restricted_registry(&tools, &[invalid.to_owned()], SessionMode::Execute,).is_err(),
                "{invalid} must not widen child MCP authority"
            );
        }
        assert!(
            restricted_registry(
                &tools,
                &["mcp:github/get_issue".to_owned()],
                SessionMode::Discuss,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn children_overlap_and_concurrency_limit_fails_closed() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
        let recorded = Arc::new(RecordingObserver::default());
        let observer: Arc<dyn SubagentObserver> = recorded.clone();
        let parent = SessionId("parent".to_owned());
        let mut handles = Vec::new();
        for _ in 0..4 {
            handles.push(
                orchestrator
                    .start(
                        parent.clone(),
                        request("delay:100"),
                        Arc::clone(&observer),
                        CancellationToken::default(),
                    )
                    .await
                    .expect("start"),
            );
        }
        let exceeded = orchestrator
            .start(
                parent,
                request("delay:1"),
                Arc::clone(&observer),
                CancellationToken::default(),
            )
            .await
            .expect_err("fifth child must be rejected");
        assert!(matches!(
            exceeded,
            OrchestrationError::ConcurrencyExceeded { maximum: 4 }
        ));
        for handle in &handles {
            orchestrator.wait(handle).await.expect("result");
        }
        assert_eq!(factory.peak.load(Ordering::Acquire), 4);
    }

    #[tokio::test]
    async fn spawn_control_never_prompts_and_inherits_selected_live_model_for_builtin_children() {
        let workspace = tempfile::tempdir().expect("workspace");
        let factory = Arc::new(FakeFactory::default());
        let launches = Arc::clone(&factory.launches);
        let orchestrator = orchestrator(SubagentLimits::default(), factory);
        let mut agents = rw_ext::compose_agent_registry(&rw_ext::ExtensionCatalog::default())
            .expect("built-in agents");
        agents
            .resolve_tool_names(std::iter::empty())
            .expect("built-in tools filter to the available registry");
        let tool = SpawnAgentTool::new(orchestrator, Arc::new(agents), Arc::new(SelectedModel));
        let sink = Arc::new(RecordingSubagentSink::default());
        let context = ToolContext::new(workspace.path())
            .expect("tool context")
            .with_session_id(SessionId("parent".to_owned()))
            .with_model_alias("openai_codex/gpt-5.6-sol")
            .with_subagent_event_sink(sink.clone());
        let gate = crate::PermissionGate::from_config(crate::PermissionConfig {
            default: PermissionDecision::Ask,
            rules: Vec::new(),
        })
        .with_workspace_roots([workspace.path()]);
        let approver = RejectingApprover(AtomicUsize::new(0));

        for (agent, isolation) in [("explore", "shared"), ("general", "shared")] {
            let input = json!({
                "action": "spawn",
                "task": "delay:1",
                "agent": agent,
                "isolation": isolation,
            });
            let capabilities = tool
                .invocation_capabilities(&input)
                .expect("spawn capabilities")
                .capabilities()
                .to_vec();
            assert!(
                capabilities.is_empty(),
                "the parent control call must not claim the child's tool authority"
            );
            let permission = crate::PermissionRequest {
                id: format!("spawn-{agent}"),
                tool_name: "spawn_agent".to_owned(),
                arguments: input.clone(),
                capabilities,
                approval_diff: None,
            };
            assert_eq!(
                gate.authorize(permission, &approver).await,
                crate::PermissionOutcome::Allowed,
                "subagent control must bypass the parent approval modal"
            );
            tool.execute(&context, input)
                .await
                .expect("built-in child uses parent model");
        }

        assert_eq!(approver.0.load(Ordering::SeqCst), 0);
        let launches = launches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(launches.len(), 2);
        assert_eq!(launches[0].agent, "explore");
        assert_eq!(launches[1].agent, "general");
        assert!(
            launches
                .iter()
                .all(|launch| launch.model == "openai_codex/gpt-5.6-sol")
        );
        assert_eq!(
            sink.lifecycles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            4,
            "each child emits spawned and finished lifecycle events"
        );
    }

    #[tokio::test]
    async fn recovery_metadata_preserves_exact_child_policy_before_the_first_turn() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), factory);
        let metadata = Arc::new(RecordingMetadataStore::default());
        orchestrator.bind_metadata_store(metadata.clone());
        let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
        let mut launch = request("delay:1");
        launch.model = "subscription-fast".to_owned();
        launch.system_prompt = Some("exact recovered prompt".to_owned());
        launch.permission_mode = SessionMode::Plan;
        launch.max_turns = Some(3);

        let handle = orchestrator
            .start(
                SessionId("parent".to_owned()),
                launch,
                observer,
                CancellationToken::default(),
            )
            .await
            .expect("start");
        let record = metadata
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("metadata saved before child turn");
        assert_eq!(record.policy.model_alias, "subscription-fast");
        assert_eq!(
            record.policy.system_prompt.as_deref(),
            Some("exact recovered prompt")
        );
        assert_eq!(record.policy.permission_mode, SessionMode::Plan);
        assert_eq!(record.policy.max_turns, 3);
        assert_eq!(record.phase, SubagentRecoveryPhase::Active);
        orchestrator.wait(&handle).await.expect("result");
    }

    #[tokio::test]
    async fn ambiguous_spawn_observer_failure_retains_pending_metadata() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
        let metadata = Arc::new(RecordingMetadataStore::default());
        orchestrator.bind_metadata_store(metadata.clone());
        let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver {
            fail_spawned: true,
            ..RecordingObserver::default()
        });
        let error = orchestrator
            .start(
                SessionId("parent".to_owned()),
                request("must-not-run"),
                observer,
                CancellationToken::default(),
            )
            .await
            .expect_err("spawn observer failure");
        assert!(error.to_string().contains("spawn fixture failure"));
        assert_eq!(factory.active.load(Ordering::Acquire), 0);
        assert_eq!(factory.cancelled.load(Ordering::Acquire), 1);
        assert_eq!(metadata.removes.load(Ordering::Acquire), 0);
        assert_eq!(
            metadata
                .record
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map(|record| record.phase),
            Some(SubagentRecoveryPhase::Pending)
        );
        assert!(
            factory
                .closed_artifacts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[test]
    fn mixed_action_shapes_are_rejected_by_the_shared_normalizer() {
        for value in [
            json!({"action":"spawn","task":"x","subagent_id":"child"}),
            json!({"action":"follow_up","subagent_id":"child","follow_up":"x","agent":"general"}),
            json!({"action":"cancel","subagent_id":"child","follow_up":"x"}),
            json!({"action":"close","subagent_id":"child","isolation":"worktree"}),
        ] {
            let input = serde_json::from_value(value).expect("shape parses before normalization");
            assert!(normalize_spawn_agent_input(input).is_err());
        }
        let follow_up: SpawnAgentInput =
            serde_json::from_value(json!({"subagent_id":"child","follow_up":"continue"}))
                .expect("legacy follow-up shape");
        assert!(matches!(
            normalize_spawn_agent_input(follow_up),
            Ok(NormalizedSpawnAgentAction::FollowUp { .. })
        ));
    }

    #[test]
    fn crash_after_spawn_gets_one_artifact_free_terminal_before_continuation() {
        let first = SubagentId("first".to_owned());
        let second = SubagentId("second".to_owned());
        let mut events = vec![
            EngineEvent::SubagentSpawned {
                meta: test_event_meta(1),
                subagent_id: first.clone(),
                child_session_id: SessionId("first-session".to_owned()),
                task: "first".to_owned(),
            },
            EngineEvent::SubagentFinished {
                meta: test_event_meta(2),
                subagent_id: first.clone(),
                result: SubagentResult {
                    subagent_id: first,
                    session_id: SessionId("first-session".to_owned()),
                    status: SubagentStatus::Completed,
                    final_text: "done".to_owned(),
                    touched_files: Vec::new(),
                    diff_artifact: None,
                    usage: zero_usage(),
                    cost: Cost::Unavailable {
                        reason: "fixture".to_owned(),
                    },
                    turns: 1,
                    duration_millis: 1,
                },
            },
            EngineEvent::SubagentSpawned {
                meta: test_event_meta(3),
                subagent_id: second.clone(),
                child_session_id: SessionId("second-session".to_owned()),
                task: "second".to_owned(),
            },
        ];
        let incomplete = incomplete_subagent_lifecycles(&events).expect("scan");
        assert_eq!(incomplete.len(), 1);
        assert_eq!(incomplete[0].subagent_id, second);
        let repair = interrupted_subagent_recovery_result(&incomplete[0]);
        assert_eq!(repair.status, SubagentStatus::Failed);
        assert!(repair.diff_artifact.is_none());
        events.push(EngineEvent::SubagentFinished {
            meta: test_event_meta(4),
            subagent_id: repair.subagent_id.clone(),
            result: repair,
        });
        assert!(
            incomplete_subagent_lifecycles(&events)
                .expect("repaired scan")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn invalid_diff_is_rejected_before_the_durable_finished_observer() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), factory);
        let recorded = Arc::new(RecordingObserver::default());
        let observer: Arc<dyn SubagentObserver> = recorded.clone();
        let result = orchestrator
            .spawn(
                SessionId("parent".to_owned()),
                request("invalid-artifact"),
                observer,
                CancellationToken::default(),
            )
            .await
            .expect("terminal result");
        assert_eq!(result.status, SubagentStatus::Failed);
        assert!(result.diff_artifact.is_none());
        let durable_results = recorded
            .results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(durable_results.as_slice(), [result]);
    }

    #[test]
    fn failed_authority_rebuild_revokes_old_grants_before_validation() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), factory);
        let parent = SessionId("parent".to_owned());
        let artifact = test_artifact();
        orchestrator
            .diff_artifact_authority()
            .record_durable(parent.clone(), &artifact)
            .expect("initial grant");
        let mut invalid = artifact.clone();
        invalid.id = "0".repeat(64);
        let event = EngineEvent::SubagentFinished {
            meta: rw_types::EventMeta {
                protocol_version: rw_types::PROTOCOL_VERSION,
                session_id: parent.clone(),
                sequence_id: rw_types::SequenceId(1),
                emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                caused_by: None,
            },
            subagent_id: SubagentId("child".to_owned()),
            result: SubagentResult {
                subagent_id: SubagentId("child".to_owned()),
                session_id: SessionId("child-session".to_owned()),
                status: SubagentStatus::Completed,
                final_text: String::new(),
                touched_files: Vec::new(),
                diff_artifact: Some(invalid),
                usage: zero_usage(),
                cost: Cost::Unavailable {
                    reason: "fixture".to_owned(),
                },
                turns: 1,
                duration_millis: 1,
            },
        };
        assert!(
            orchestrator
                .rebuild_artifact_authority(&parent, &[event])
                .is_err()
        );
        assert!(
            orchestrator
                .diff_artifact_authority()
                .resolve(&parent, &artifact.id)
                .is_none()
        );
    }

    #[tokio::test]
    async fn recovery_rejects_capability_drift_and_duplicate_identities() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), factory);
        let mut drifted = recovery_record("drift", "drift-session");
        drifted.capabilities = CapabilityManifest::new([ToolCapability::WriteFilesystem]);
        assert!(orchestrator.recover_record(drifted).await.is_err());

        let record = recovery_record("child", "child-session");
        orchestrator
            .recover_record(record.clone())
            .await
            .expect("first recovery");
        assert!(orchestrator.recover_record(record).await.is_err());
        assert!(
            orchestrator
                .recover_record(recovery_record("other", "child-session"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn metadata_remove_failure_keeps_child_closing_and_retry_does_not_finalize_twice() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
        let metadata = Arc::new(FailOnceRemoveMetadataStore::default());
        orchestrator.bind_metadata_store(metadata.clone());
        let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
        let handle = orchestrator
            .start(
                SessionId("parent".to_owned()),
                request("done"),
                Arc::clone(&observer),
                CancellationToken::default(),
            )
            .await
            .expect("start");
        orchestrator.wait(&handle).await.expect("finish");
        let parent = SessionId("parent".to_owned());
        assert!(
            orchestrator
                .close(&parent, &handle.subagent_id)
                .await
                .is_err()
        );
        assert!(matches!(
            orchestrator
                .follow_up(
                    &parent,
                    &handle.subagent_id,
                    "must not run".to_owned(),
                    observer,
                    CancellationToken::default(),
                )
                .await,
            Err(OrchestrationError::AlreadyRunning(_))
        ));
        orchestrator
            .close(&parent, &handle.subagent_id)
            .await
            .expect("metadata cleanup retry");
        assert_eq!(factory.cancelled.load(Ordering::Acquire), 1);
        assert_eq!(metadata.removes.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn concurrent_close_calls_finalize_the_child_exactly_once() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
        let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
        let handle = orchestrator
            .start(
                SessionId("parent".to_owned()),
                request("done"),
                observer,
                CancellationToken::default(),
            )
            .await
            .expect("start");
        orchestrator.wait(&handle).await.expect("finish");
        let first = orchestrator.clone();
        let second = orchestrator.clone();
        let first_id = handle.subagent_id.clone();
        let second_id = handle.subagent_id;
        let parent = SessionId("parent".to_owned());
        let (first_result, second_result) = tokio::join!(
            first.close(&parent, &first_id),
            second.close(&parent, &second_id),
        );
        assert!(first_result.is_ok() ^ second_result.is_ok());
        assert_eq!(factory.cancelled.load(Ordering::Acquire), 1);
        assert_eq!(
            factory
                .closed_artifacts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn clean_follow_up_clears_the_previous_durable_artifact_before_close() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
        let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
        let handle = orchestrator
            .start(
                SessionId("parent".to_owned()),
                request("valid-artifact"),
                Arc::clone(&observer),
                CancellationToken::default(),
            )
            .await
            .expect("start");
        assert!(
            orchestrator
                .wait(&handle)
                .await
                .expect("dirty result")
                .diff_artifact
                .is_some()
        );
        let follow_up = orchestrator
            .follow_up(
                &SessionId("parent".to_owned()),
                &handle.subagent_id,
                "clean".to_owned(),
                observer,
                CancellationToken::default(),
            )
            .await
            .expect("follow-up");
        assert!(
            orchestrator
                .wait(&follow_up)
                .await
                .expect("clean result")
                .diff_artifact
                .is_none()
        );
        orchestrator
            .close(&SessionId("parent".to_owned()), &handle.subagent_id)
            .await
            .expect("close clean child");
        assert_eq!(
            factory
                .closed_artifacts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            [None]
        );
    }

    #[tokio::test]
    async fn recovered_dirty_child_closes_with_the_full_durable_artifact() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
        let parent = SessionId("parent".to_owned());
        let artifact = test_artifact();
        let artifact_id = artifact.id.clone();
        let event = EngineEvent::SubagentFinished {
            meta: rw_types::EventMeta {
                protocol_version: rw_types::PROTOCOL_VERSION,
                session_id: parent.clone(),
                sequence_id: rw_types::SequenceId(1),
                emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                caused_by: None,
            },
            subagent_id: SubagentId("child".to_owned()),
            result: SubagentResult {
                subagent_id: SubagentId("child".to_owned()),
                session_id: SessionId("child-session".to_owned()),
                status: SubagentStatus::Completed,
                final_text: "dirty".to_owned(),
                touched_files: vec!["src/lib.rs".to_owned()],
                diff_artifact: Some(artifact),
                usage: zero_usage(),
                cost: Cost::Unavailable {
                    reason: "fixture".to_owned(),
                },
                turns: 1,
                duration_millis: 1,
            },
        };
        orchestrator
            .rebuild_artifact_authority(&parent, &[event])
            .expect("authority rebuild");
        orchestrator
            .recover_record(recovery_record("child", "child-session"))
            .await
            .expect("recover child");
        orchestrator
            .close(&parent, &SubagentId("child".to_owned()))
            .await
            .expect("close recovered dirty child");
        assert_eq!(
            factory
                .closed_artifacts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            [Some(artifact_id)]
        );
    }

    #[tokio::test]
    async fn hung_cancel_is_bounded_and_the_permit_is_eventually_released() {
        let factory = Arc::new(FakeFactory {
            hang_cancel: true,
            ..FakeFactory::default()
        });
        let limits = SubagentLimits {
            max_concurrency: 1,
            max_duration: Duration::from_millis(20),
            ..SubagentLimits::default()
        };
        let orchestrator = orchestrator(limits, factory);
        let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
        let handle = orchestrator
            .start(
                SessionId("parent".to_owned()),
                request("delay:1000"),
                Arc::clone(&observer),
                CancellationToken::default(),
            )
            .await
            .expect("start");
        assert!(
            orchestrator
                .cancel(&SessionId("parent".to_owned()), &handle.subagent_id)
                .await
                .is_err()
        );
        let _ = orchestrator.wait(&handle).await;
        let next = orchestrator
            .start(
                SessionId("parent".to_owned()),
                request("done"),
                observer,
                CancellationToken::default(),
            )
            .await
            .expect("permit released after bounded cleanup");
        let _ = orchestrator.wait(&next).await;
    }

    #[tokio::test]
    async fn worst_case_model_handoff_keeps_artifact_reference_under_wire_limit() {
        let mut artifact = test_artifact();
        artifact.unified_diff = "d".repeat(MAX_SUBAGENT_DIFF_BYTES);
        rehash_test_artifact(&mut artifact);
        let artifact_id = artifact.id.clone();
        let result = SubagentResult {
            subagent_id: SubagentId("child".to_owned()),
            session_id: SessionId("child-session".to_owned()),
            status: SubagentStatus::Completed,
            final_text: "\0".repeat(MAX_SUBAGENT_FINAL_TEXT_BYTES),
            touched_files: (0..MAX_SUBAGENT_TOUCHED_FILES)
                .map(|index| format!("{}-{index}", "\"".repeat(4090)))
                .collect(),
            diff_artifact: Some(artifact),
            usage: zero_usage(),
            cost: Cost::Unavailable {
                reason: "fixture".to_owned(),
            },
            turns: 1,
            duration_millis: 1,
        };
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(FixedResultTool {
                result: model_facing_subagent_tool_result(&result),
            }))
            .expect("register");
        let tool = registry.resolve("fixed_result").expect("tool");
        let context = ToolContext::new(std::env::current_dir().expect("cwd")).expect("context");
        let output = tool.execute(&context, Value::Null).await.expect("execute");
        assert!(!output.truncated);
        assert_eq!(
            output.data["diff_artifact"]["artifact_id"].as_str(),
            Some(artifact_id.as_str())
        );
    }

    #[tokio::test]
    async fn depth_limit_and_completed_child_continuity_are_enforced() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), factory);
        let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
        let root = SessionId("parent".to_owned());
        let first = orchestrator
            .start(
                root,
                request("first"),
                Arc::clone(&observer),
                CancellationToken::default(),
            )
            .await
            .expect("first");
        assert_eq!(
            orchestrator
                .wait(&first)
                .await
                .expect("first result")
                .final_text,
            "history:1"
        );
        let continued = orchestrator
            .follow_up(
                &SessionId("parent".to_owned()),
                &first.subagent_id,
                "follow-up".to_owned(),
                Arc::clone(&observer),
                CancellationToken::default(),
            )
            .await
            .expect("follow-up");
        assert_eq!(
            orchestrator
                .wait(&continued)
                .await
                .expect("continued result")
                .final_text,
            "history:2"
        );
        let second = orchestrator
            .start(
                first.session_id,
                request("second depth"),
                Arc::clone(&observer),
                CancellationToken::default(),
            )
            .await
            .expect("second depth");
        orchestrator.wait(&second).await.expect("second result");
        let exceeded = orchestrator
            .start(
                second.session_id,
                request("third depth"),
                observer,
                CancellationToken::default(),
            )
            .await
            .expect_err("depth three must fail");
        assert!(matches!(
            exceeded,
            OrchestrationError::DepthExceeded {
                requested: 3,
                maximum: 2
            }
        ));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn child_control_is_scoped_to_the_exact_parent_session() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
        let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
        let parent = SessionId("parent".to_owned());
        let victim = orchestrator
            .start(
                parent.clone(),
                request("victim"),
                Arc::clone(&observer),
                CancellationToken::default(),
            )
            .await
            .expect("start victim");
        orchestrator.wait(&victim).await.expect("finish victim");

        let nested_attacker = orchestrator
            .start(
                parent.clone(),
                request("attacker"),
                Arc::clone(&observer),
                CancellationToken::default(),
            )
            .await
            .expect("start nested attacker");
        orchestrator
            .wait(&nested_attacker)
            .await
            .expect("finish nested attacker");

        let listed = orchestrator.list_for_parent(&parent);
        assert_eq!(listed.len(), 2);
        let victim_descriptor = listed
            .iter()
            .find(|descriptor| descriptor.subagent_id == victim.subagent_id)
            .expect("victim descriptor");
        assert_eq!(victim_descriptor.task, "victim");
        assert_eq!(victim_descriptor.agent, "fixture");
        assert_eq!(victim_descriptor.model, "fast");
        assert_eq!(victim_descriptor.activity, SubagentActivity::Idle);
        assert!(
            orchestrator
                .list_for_parent(&SessionId("sibling-parent".to_owned()))
                .is_empty()
        );
        assert!(matches!(
            orchestrator.descriptor_for_parent(
                &SessionId("sibling-parent".to_owned()),
                &victim.subagent_id
            ),
            Err(OrchestrationError::UnknownSubagent(_))
        ));

        for attacker in [
            SessionId("sibling-parent".to_owned()),
            nested_attacker.session_id.clone(),
        ] {
            let follow_up = orchestrator
                .follow_up(
                    &attacker,
                    &victim.subagent_id,
                    "steal child".to_owned(),
                    Arc::clone(&observer),
                    CancellationToken::default(),
                )
                .await;
            assert!(matches!(
                follow_up,
                Err(OrchestrationError::UnknownSubagent(_))
            ));
            assert!(matches!(
                orchestrator.cancel(&attacker, &victim.subagent_id).await,
                Err(OrchestrationError::UnknownSubagent(_))
            ));
            assert!(matches!(
                orchestrator.close(&attacker, &victim.subagent_id).await,
                Err(OrchestrationError::UnknownSubagent(_))
            ));
        }

        let guessed = SubagentId("guessed-child-id".to_owned());
        assert!(matches!(
            orchestrator
                .follow_up(
                    &parent,
                    &guessed,
                    "probe".to_owned(),
                    Arc::clone(&observer),
                    CancellationToken::default(),
                )
                .await,
            Err(OrchestrationError::UnknownSubagent(_))
        ));
        assert!(matches!(
            orchestrator.cancel(&parent, &guessed).await,
            Err(OrchestrationError::UnknownSubagent(_))
        ));
        assert!(matches!(
            orchestrator.close(&parent, &guessed).await,
            Err(OrchestrationError::UnknownSubagent(_))
        ));

        let continued = orchestrator
            .follow_up(
                &parent,
                &victim.subagent_id,
                "authorized".to_owned(),
                observer,
                CancellationToken::default(),
            )
            .await
            .expect("owner retains child control");
        assert_eq!(
            orchestrator
                .wait(&continued)
                .await
                .expect("authorized follow-up")
                .final_text,
            "history:2"
        );
        orchestrator
            .close(&parent, &victim.subagent_id)
            .await
            .expect("owner closes child");
        assert!(
            orchestrator
                .descriptor_for_parent(&parent, &victim.subagent_id)
                .is_err()
        );
        assert_eq!(factory.cancelled.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn durable_finished_failure_is_returned_to_parent() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), factory);
        let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver {
            fail_finished: true,
            ..RecordingObserver::default()
        });
        let handle = orchestrator
            .start(
                SessionId("parent".to_owned()),
                request("finish"),
                observer,
                CancellationToken::default(),
            )
            .await
            .expect("start");
        let error = orchestrator
            .wait(&handle)
            .await
            .expect_err("persistence failure");
        assert!(error.to_string().contains("fixture failure"));
    }

    #[tokio::test]
    async fn close_permanently_removes_continuation_handle() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), factory);
        let observer: Arc<dyn SubagentObserver> = Arc::new(RecordingObserver::default());
        let handle = orchestrator
            .start(
                SessionId("parent".to_owned()),
                request("finish"),
                Arc::clone(&observer),
                CancellationToken::default(),
            )
            .await
            .expect("start");
        orchestrator.wait(&handle).await.expect("finish");
        orchestrator
            .close(&SessionId("parent".to_owned()), &handle.subagent_id)
            .await
            .expect("close");
        let error = orchestrator
            .follow_up(
                &SessionId("parent".to_owned()),
                &handle.subagent_id,
                "too late".to_owned(),
                observer,
                CancellationToken::default(),
            )
            .await
            .expect_err("closed child cannot continue");
        assert!(matches!(error, OrchestrationError::UnknownSubagent(_)));
    }

    #[tokio::test]
    async fn pending_metadata_failure_happens_before_durable_spawn() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
        orchestrator.bind_metadata_store(Arc::new(FailingMetadataStore));
        let recorded = Arc::new(RecordingObserver::default());
        let observer: Arc<dyn SubagentObserver> = recorded.clone();
        let error = orchestrator
            .start(
                SessionId("parent".to_owned()),
                request("must-not-run"),
                observer,
                CancellationToken::default(),
            )
            .await
            .expect_err("metadata failure");
        assert!(error.to_string().contains("metadata persistence failed"));
        assert_eq!(factory.active.load(Ordering::Acquire), 0);
        assert_eq!(factory.cancelled.load(Ordering::Acquire), 1);
        assert_eq!(
            factory
                .closed_artifacts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
        let events = recorded
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn pending_metadata_failure_surfaces_exact_close_failure_without_spawning() {
        let factory = Arc::new(FakeFactory {
            fail_close: true,
            ..FakeFactory::default()
        });
        let orchestrator = orchestrator(SubagentLimits::default(), factory);
        orchestrator.bind_metadata_store(Arc::new(FailingMetadataStore));
        let recorded = Arc::new(RecordingObserver::default());
        let error = orchestrator
            .start(
                SessionId("parent".to_owned()),
                request("must-not-run"),
                recorded.clone(),
                CancellationToken::default(),
            )
            .await
            .expect_err("metadata and cleanup failure");
        assert!(error.to_string().contains("metadata persistence failed"));
        assert!(error.to_string().contains("fixture close failed"));
        assert!(
            recorded
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn promotion_failure_retains_pending_record_and_commits_terminal_lifecycle() {
        let factory = Arc::new(FakeFactory::default());
        let orchestrator = orchestrator(SubagentLimits::default(), Arc::clone(&factory));
        let metadata = Arc::new(FailingPromotionMetadataStore::default());
        orchestrator.bind_metadata_store(metadata.clone());
        let recorded = Arc::new(RecordingObserver::default());
        let observer: Arc<dyn SubagentObserver> = recorded.clone();
        let error = orchestrator
            .start(
                SessionId("parent".to_owned()),
                request("must-not-run"),
                observer,
                CancellationToken::default(),
            )
            .await
            .expect_err("promotion failure");
        assert!(error.to_string().contains("metadata promotion failed"));
        assert_eq!(factory.active.load(Ordering::Acquire), 0);
        assert_eq!(factory.cancelled.load(Ordering::Acquire), 1);
        let pending = metadata
            .retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("pending record retained");
        assert_eq!(pending.phase, SubagentRecoveryPhase::Pending);
        let events = recorded
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(events.len(), 2);
        assert!(events[0].starts_with("spawn:"));
        assert!(events[1].starts_with("finish:"));
    }

    #[test]
    fn result_schema_round_trips_cost_and_usage() {
        let result = SubagentResult {
            subagent_id: SubagentId("agent".to_owned()),
            session_id: SessionId("child".to_owned()),
            status: SubagentStatus::Completed,
            final_text: "done".to_owned(),
            touched_files: vec!["src/lib.rs".to_owned()],
            diff_artifact: None,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 3,
                cache_write_tokens: 1,
                reasoning_tokens: 2,
            },
            cost: Cost::Monetary {
                amount_micros: 42,
                currency: "USD".to_owned(),
            },
            turns: 2,
            duration_millis: 7,
        };
        let encoded = serde_json::to_vec(&result).expect("encode");
        let decoded: SubagentResult = serde_json::from_slice(&encoded).expect("decode");
        assert_eq!(decoded, result);
    }
}
