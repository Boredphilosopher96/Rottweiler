use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use rw_types::{
    ClientCommand, ClientId, ClientRole, CommandAckMeta, CommandDescriptor, CommandMeta,
    CommandOutcome, EngineError, EngineErrorCategory, EngineEvent, ModelAlias, ModelDescriptor,
    RequestId, SequenceId, SessionDescriptor, SessionId, ShellId, TurnId, WorkspaceFileMatch,
    WorkspaceFilePreview, WorkspaceStatus,
};
use thiserror::Error;
use tokio::sync::{Notify, broadcast, mpsc, watch};

use crate::{AgentLoopError, EventClock, SessionHandle, SystemEventClock};

const HOST_EVENT_CAPACITY: usize = 256;

/// Transport-authenticated client identity. The host overwrites every
/// untrusted wire `CommandMeta.client_id` with this value before authorization.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BoundClient {
    pub client_id: ClientId,
}

/// Bounded process-wide engine-host settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineHostConfig {
    pub max_sessions: usize,
    pub max_deduplicated_requests: usize,
}

impl Default for EngineHostConfig {
    fn default() -> Self {
        Self {
            max_sessions: 32,
            max_deduplicated_requests: 4_096,
        }
    }
}

/// Inputs required to create a new hosted session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateSessionRequest {
    pub session_id: SessionId,
    pub workspace: String,
    pub model: Option<ModelAlias>,
}

/// Inputs required to fork one completed parent-session boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForkSessionRequest {
    pub operation_key: ForkOperationKey,
    pub parent: SessionDescriptor,
    pub child_session_id: SessionId,
    pub at_turn: TurnId,
    pub through_sequence: Option<SequenceId>,
    pub include_idle_tail: bool,
    pub driver_client_id: ClientId,
}

/// Durable identity of one authenticated fork command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForkOperationKey {
    pub operation_id: String,
    pub client_id: ClientId,
    pub request_id: RequestId,
    pub payload_hash: String,
}

/// One authorized, boundary-fixed fork operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedForkOperation {
    pub key: ForkOperationKey,
    pub request: ForkSessionRequest,
}

/// Connection-scoped result retained for exact fork replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedForkOperation {
    pub protocol_version: u16,
    pub command_ack_emitted_at: String,
    pub fork_event_emitted_at: String,
    pub acknowledged_session_id: SessionId,
    pub outcome: CommandOutcome,
    pub parent_session_id: SessionId,
    pub child: SessionDescriptor,
    pub at_turn: TurnId,
}

/// Durable state of one fork operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ForkOperationState {
    Missing,
    Pending(PreparedForkOperation),
    Completed(CompletedForkOperation),
}

/// One actor and its remote-safe host descriptor.
#[derive(Clone)]
pub struct HostedSession {
    descriptor: Arc<RwLock<SessionDescriptor>>,
    handle: SessionHandle,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
}

impl fmt::Debug for HostedSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostedSession")
            .field("descriptor", &self.descriptor())
            .finish_non_exhaustive()
    }
}

impl HostedSession {
    #[must_use]
    pub fn new(descriptor: SessionDescriptor, handle: SessionHandle) -> Self {
        Self {
            descriptor: Arc::new(RwLock::new(descriptor)),
            handle,
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    #[must_use]
    pub fn descriptor(&self) -> SessionDescriptor {
        self.descriptor
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn handle(&self) -> SessionHandle {
        self.handle.clone()
    }

    fn set_driver(&self, client_id: Option<ClientId>) {
        self.descriptor
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .driver_client_id = client_id;
    }

    fn set_shell_active(&self, active: bool) {
        self.descriptor
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shell_active = active;
    }

    /// Starts a projection of the actor's durable state changes into the
    /// lightweight host descriptor. The subscription is created before the
    /// task is spawned, so events committed between registration and the
    /// first poll are either replayed from the sink or retained by broadcast.
    async fn project_durable_descriptor(&self) -> Result<(), HostError> {
        let descriptor = Arc::clone(&self.descriptor);
        // The factory-provided descriptor already represents recovered state.
        // Start at the current durable tail so it is never rolled backward by
        // replaying historical state transitions. The session is not visible
        // in the host registry until this subscription has been installed.
        let tail = self.handle.last_sequence().await.map_err(HostError::from)?;
        let mut events = self
            .handle
            .subscribe_client(ClientId("host-descriptor-projector".to_owned()), tail);
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                let mut descriptor = descriptor
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match event {
                    EngineEvent::SessionCreated {
                        driver_client_id, ..
                    }
                    | EngineEvent::DriverChanged {
                        driver_client_id, ..
                    } => descriptor.driver_client_id = Some(driver_client_id),
                    EngineEvent::ModelChanged { model, .. } => descriptor.model = model,
                    EngineEvent::UserShellStateChanged { active, .. } => {
                        descriptor.shell_active = active;
                    }
                    _ => {}
                }
            }
        });
        Ok(())
    }
}

/// CLI composition boundary for opening durable session actors.
#[async_trait]
pub trait SessionFactory: Send + Sync + 'static {
    /// Allocates a storage-safe id before an asynchronous create reserves
    /// capacity.
    ///
    /// # Errors
    ///
    /// Returns a sanitized factory error when an id cannot be allocated.
    fn allocate_session_id(&self) -> Result<SessionId, HostError>;

    async fn create(&self, request: CreateSessionRequest) -> Result<HostedSession, HostError>;

    async fn resume(&self, session_id: &SessionId) -> Result<HostedSession, HostError>;

    async fn fork(&self, _request: ForkSessionRequest) -> Result<HostedSession, HostError> {
        Err(HostError::Protocol(
            "session forking is not configured".to_owned(),
        ))
    }

    async fn load_fork_operation(
        &self,
        _key: &ForkOperationKey,
    ) -> Result<ForkOperationState, HostError> {
        Ok(ForkOperationState::Missing)
    }

    async fn prepare_fork_operation(
        &self,
        operation: PreparedForkOperation,
    ) -> Result<PreparedForkOperation, HostError> {
        Ok(operation)
    }

    async fn complete_fork_operation(
        &self,
        _key: &ForkOperationKey,
        result: &CompletedForkOperation,
    ) -> Result<CompletedForkOperation, HostError> {
        Ok(result.clone())
    }

    async fn abandon_prepared_fork_operation(
        &self,
        _key: &ForkOperationKey,
    ) -> Result<(), HostError> {
        Ok(())
    }

    async fn persisted_sessions(&self) -> Result<Vec<SessionDescriptor>, HostError> {
        Ok(Vec::new())
    }

    async fn search_persisted_sessions(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<(Vec<SessionDescriptor>, bool), HostError> {
        let query = query.to_ascii_lowercase();
        let mut sessions = self.persisted_sessions().await?;
        sessions.retain(|session| {
            session.session_id.0.to_ascii_lowercase().contains(&query)
                || session.workspace_name.to_ascii_lowercase().contains(&query)
        });
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let truncated = sessions.len() > limit;
        sessions.truncate(limit);
        Ok((sessions, truncated))
    }

    async fn shutdown(&self) -> Result<(), HostError> {
        Ok(())
    }
}

/// Remote-safe host query boundary implemented by the CLI/storage layer.
#[async_trait]
pub trait HostQueryService: Send + Sync + 'static {
    async fn command_descriptors(&self) -> Result<Vec<CommandDescriptor>, HostError>;
    async fn model_descriptors(&self) -> Result<Vec<ModelDescriptor>, HostError>;
    async fn search_workspace_files(
        &self,
        session: &SessionDescriptor,
        query: &str,
        limit: u32,
    ) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError>;
    async fn preview_workspace_file(
        &self,
        session: &SessionDescriptor,
        path: &str,
        max_bytes: u32,
    ) -> Result<WorkspaceFilePreview, HostError>;
    async fn workspace_status(
        &self,
        session: &SessionDescriptor,
    ) -> Result<WorkspaceStatus, HostError>;
}

/// A host operation could not be completed safely.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HostError {
    #[error("engine host is shutting down")]
    ShuttingDown,
    #[error("engine host session capacity is exhausted")]
    SessionCapacity,
    #[error("session {0:?} is not loaded")]
    SessionNotLoaded(String),
    #[error("session factory returned an unexpected session id")]
    SessionIdentityMismatch,
    #[error("host request id was reused with a different command")]
    RequestConflict,
    #[error("host persistence failure: {0}")]
    Persistence(String),
    #[error("host query failure: {0}")]
    Query(String),
    #[error("host protocol failure: {0}")]
    Protocol(String),
}

impl From<AgentLoopError> for HostError {
    fn from(value: AgentLoopError) -> Self {
        Self::Persistence(value.to_string())
    }
}

#[derive(Debug)]
enum SessionSlot {
    Opening(watch::Sender<bool>),
    Ready(Arc<HostedSession>),
}

#[derive(Debug, Default)]
struct HostRegistry {
    sessions: HashMap<SessionId, SessionSlot>,
    anonymous_openings: usize,
}

#[derive(Clone, Debug)]
struct CachedDispatch {
    outcome: CommandOutcome,
    events: Vec<EngineEvent>,
    cacheable: bool,
}

#[derive(Debug)]
enum DedupeState {
    Running {
        payload_hash: String,
        notify: Arc<Notify>,
    },
    Complete {
        payload_hash: String,
        dispatch: CachedDispatch,
        retry_same_request: bool,
    },
}

#[derive(Debug, Default)]
struct DedupeRegistry {
    entries: HashMap<(ClientId, RequestId), DedupeState>,
    order: VecDeque<(ClientId, RequestId)>,
}

/// Process-wide router and supervisor-neutral owner of session actors.
#[derive(Clone)]
pub struct EngineHost {
    config: EngineHostConfig,
    factory: Arc<dyn SessionFactory>,
    queries: Arc<dyn HostQueryService>,
    clock: Arc<dyn EventClock>,
    registry: Arc<tokio::sync::Mutex<HostRegistry>>,
    dedupe: Arc<Mutex<DedupeRegistry>>,
    client_events: Arc<Mutex<HashMap<ClientId, broadcast::Sender<EngineEvent>>>>,
    shutting_down: Arc<AtomicBool>,
}

impl fmt::Debug for EngineHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EngineHost")
            .field("config", &self.config)
            .field("shutting_down", &self.shutting_down)
            .finish_non_exhaustive()
    }
}

impl EngineHost {
    /// Builds a bounded host around injected session and query boundaries.
    ///
    /// # Errors
    ///
    /// Rejects zero session or request-deduplication capacities.
    pub fn new(
        config: EngineHostConfig,
        factory: Arc<dyn SessionFactory>,
        queries: Arc<dyn HostQueryService>,
    ) -> Result<Self, HostError> {
        if config.max_sessions == 0 || config.max_deduplicated_requests == 0 {
            return Err(HostError::Protocol(
                "host capacities must be greater than zero".to_owned(),
            ));
        }
        Ok(Self {
            config,
            factory,
            queries,
            clock: Arc::new(SystemEventClock),
            registry: Arc::new(tokio::sync::Mutex::new(HostRegistry::default())),
            dedupe: Arc::new(Mutex::new(DedupeRegistry::default())),
            client_events: Arc::new(Mutex::new(HashMap::new())),
            shutting_down: Arc::new(AtomicBool::new(false)),
        })
    }

    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn EventClock>) -> Self {
        self.clock = clock;
        self
    }

    /// Looks up one ready session without opening or resuming it.
    pub async fn session(&self, session_id: &SessionId) -> Option<Arc<HostedSession>> {
        let registry = self.registry.lock().await;
        match registry.sessions.get(session_id) {
            Some(SessionSlot::Ready(session)) => Some(Arc::clone(session)),
            Some(SessionSlot::Opening(_)) | None => None,
        }
    }

    /// Releases a durable foreground-shell gate from the trusted CLI broker.
    ///
    /// The broker observes the normal authenticated event stream but must not
    /// take the TUI driver's lease merely to report the real TTY child's exit.
    /// The session actor validates the engine-generated shell id before it
    /// persists the inactive event.
    ///
    /// # Errors
    ///
    /// Returns a typed host error when the session is not loaded, the shell id
    /// is stale, captured output is invalid, or the durable write fails.
    pub async fn complete_user_shell(
        &self,
        session_id: &SessionId,
        shell_id: ShellId,
        status: i32,
        captured_output: Option<String>,
    ) -> Result<(), HostError> {
        let session = self.ready_session(session_id).await?;
        session
            .handle()
            .complete_user_shell(shell_id, status, captured_output)
            .await
            .map_err(HostError::from)?;
        // Trusted completion returns only after the inactive shell event is
        // durable, so this eager update cannot get ahead of persistence. The
        // descriptor projector independently observes the same event.
        session.set_shell_active(false);
        Ok(())
    }

    /// Opens the supervisor-selected initial session before accepting client
    /// traffic. A fresh engine creates it; a restarted engine resumes the same
    /// durable identity. Driver ownership remains unset until an authenticated
    /// client attaches.
    ///
    /// # Errors
    ///
    /// Returns a typed host error when capacity, persistence, recovery, or
    /// session identity validation fails.
    pub async fn prepare_session(
        &self,
        request: CreateSessionRequest,
        resume: bool,
    ) -> Result<SessionDescriptor, HostError> {
        let session = if resume {
            self.resume_session(&request.session_id).await?
        } else {
            self.create_session(request).await?
        };
        Ok(session.descriptor())
    }

    /// Dispatches a command under a transport-authenticated identity. Duplicate
    /// request ids replay their original outcome and connection-scoped events.
    #[allow(clippy::too_many_lines)]
    pub async fn dispatch(&self, bound: BoundClient, mut command: ClientCommand) -> CommandOutcome {
        command.meta_mut().client_id = bound.client_id.clone();
        let meta = command.meta().clone();
        let payload_hash = match serde_json::to_vec(&command) {
            Ok(bytes) => blake3::hash(&bytes).to_hex().to_string(),
            Err(_) => return rejected("command_serialization", "command could not serialize"),
        };
        let key = (bound.client_id.clone(), meta.request_id.clone());
        let session_id_hint = command_session_id(&command);
        let mut pending_command = Some(command);

        loop {
            let wait = {
                let mut dedupe = self
                    .dedupe
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match dedupe.entries.get(&key) {
                    Some(DedupeState::Complete {
                        payload_hash: existing,
                        dispatch,
                        retry_same_request,
                    }) => {
                        if existing != &payload_hash {
                            drop(dedupe);
                            let outcome = rejected(
                                "request_id_conflict",
                                "request id was reused with a different command",
                            );
                            self.emit_one(
                                &bound.client_id,
                                command_ack(
                                    &meta,
                                    session_id_hint.clone(),
                                    outcome.clone(),
                                    &*self.clock,
                                ),
                            );
                            return outcome;
                        }
                        let cached = dispatch.clone();
                        let retry_same_request = *retry_same_request;
                        if retry_same_request {
                            dedupe.entries.remove(&key);
                            dedupe.order.retain(|queued| queued != &key);
                        }
                        drop(dedupe);
                        self.emit_many(&bound.client_id, &cached.events);
                        return cached.outcome;
                    }
                    Some(DedupeState::Running {
                        payload_hash: existing,
                        notify,
                    }) => {
                        if existing != &payload_hash {
                            drop(dedupe);
                            let outcome = rejected(
                                "request_id_conflict",
                                "request id was reused with a different command",
                            );
                            self.emit_one(
                                &bound.client_id,
                                command_ack(
                                    &meta,
                                    session_id_hint.clone(),
                                    outcome.clone(),
                                    &*self.clock,
                                ),
                            );
                            return outcome;
                        }
                        Some(Arc::clone(notify).notified_owned())
                    }
                    None => {
                        let notify = Arc::new(Notify::new());
                        dedupe.entries.insert(
                            key.clone(),
                            DedupeState::Running {
                                payload_hash: payload_hash.clone(),
                                notify,
                            },
                        );
                        dedupe.order.push_back(key.clone());
                        drop(dedupe);
                        let host = self.clone();
                        let bound_client = bound.client_id.clone();
                        let operation_key = key.clone();
                        let operation_hash = payload_hash.clone();
                        let Some(operation) = pending_command.take() else {
                            return rejected(
                                "request_state_invalid",
                                "request execution state was unavailable",
                            );
                        };
                        tokio::spawn(async move {
                            let dispatch = host.execute(operation, operation_hash.clone()).await;
                            host.complete_request(
                                operation_key,
                                operation_hash,
                                &dispatch,
                                &bound_client,
                            );
                        });
                        let dedupe = self
                            .dedupe
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        match dedupe.entries.get(&key) {
                            Some(DedupeState::Running { notify, .. }) => {
                                Some(Arc::clone(notify).notified_owned())
                            }
                            Some(DedupeState::Complete { .. }) | None => None,
                        }
                    }
                }
            };
            if let Some(wait) = wait {
                wait.await;
            }
        }
    }

    fn complete_request(
        &self,
        key: (ClientId, RequestId),
        payload_hash: String,
        dispatch: &CachedDispatch,
        client_id: &ClientId,
    ) {
        let notify = {
            let mut dedupe = self
                .dedupe
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let notify = match dedupe.entries.get(&key) {
                Some(DedupeState::Running { notify, .. }) => Some(Arc::clone(notify)),
                Some(DedupeState::Complete { .. }) | None => None,
            };
            dedupe.entries.insert(
                key,
                DedupeState::Complete {
                    payload_hash,
                    dispatch: dispatch.clone(),
                    retry_same_request: !dispatch.cacheable,
                },
            );
            while dedupe.entries.len() > self.config.max_deduplicated_requests {
                let Some(oldest) = dedupe.order.pop_front() else {
                    break;
                };
                if matches!(
                    dedupe.entries.get(&oldest),
                    Some(DedupeState::Complete { .. })
                ) {
                    dedupe.entries.remove(&oldest);
                } else {
                    dedupe.order.push_back(oldest);
                    break;
                }
            }
            notify
        };
        self.emit_many(client_id, &dispatch.events);
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
    }

    async fn execute(&self, command: ClientCommand, payload_hash: String) -> CachedDispatch {
        let meta = command.meta().clone();
        let command = match command {
            ClientCommand::Fork {
                meta,
                session_id,
                at_turn,
                operation_id,
            } => {
                return self
                    .execute_fork(meta, session_id, at_turn, operation_id, payload_hash)
                    .await;
            }
            command => command,
        };
        let result = self.execute_inner(command).await;
        match result {
            Ok((outcome, session_id, mut events)) => {
                events.insert(
                    0,
                    command_ack(&meta, session_id, outcome.clone(), &*self.clock),
                );
                CachedDispatch {
                    outcome,
                    events,
                    cacheable: true,
                }
            }
            Err(error) => {
                let outcome = rejected(host_error_code(&error), &error.to_string());
                CachedDispatch {
                    events: vec![command_ack(&meta, None, outcome.clone(), &*self.clock)],
                    outcome,
                    cacheable: true,
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_fork(
        &self,
        meta: CommandMeta,
        session_id: SessionId,
        at_turn: Option<TurnId>,
        operation_id: Option<String>,
        _request_payload_hash: String,
    ) -> CachedDispatch {
        let operation_id = operation_id.unwrap_or_else(|| {
            let mut legacy = b"rw-legacy-fork-operation\0".to_vec();
            legacy.extend_from_slice(meta.client_id.0.as_bytes());
            legacy.push(0);
            legacy.extend_from_slice(meta.request_id.0.as_bytes());
            blake3::hash(&legacy).to_hex().to_string()
        });
        if operation_id.is_empty()
            || operation_id.len() > 128
            || !operation_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            let outcome = rejected(
                "invalid_fork_operation_id",
                "fork operation id must be 1-128 safe ASCII characters",
            );
            return CachedDispatch {
                events: vec![command_ack(&meta, None, outcome.clone(), &*self.clock)],
                outcome,
                cacheable: true,
            };
        }
        let Ok(payload) = serde_json::to_vec(&(&session_id, &at_turn)) else {
            let outcome = rejected(
                "fork_payload_serialization",
                "fork operation payload could not serialize",
            );
            return CachedDispatch {
                events: vec![command_ack(&meta, None, outcome.clone(), &*self.clock)],
                outcome,
                cacheable: true,
            };
        };
        let payload_hash = blake3::hash(&payload).to_hex().to_string();
        let key = ForkOperationKey {
            operation_id,
            client_id: meta.client_id.clone(),
            request_id: meta.request_id.clone(),
            payload_hash,
        };
        let result = async {
            let mut lifecycle_guard = None;
            let operation = match self.factory.load_fork_operation(&key).await? {
                ForkOperationState::Completed(completed) => return Ok(completed),
                ForkOperationState::Pending(operation) => operation,
                ForkOperationState::Missing => {
                    let parent = self.ready_session(&session_id).await?;
                    lifecycle_guard = Some(Arc::clone(&parent.lifecycle).lock_owned().await);
                    let snapshot = parent.handle().snapshot().await?;
                    if snapshot.driver_client_id.as_ref() != Some(&meta.client_id) {
                        return Err(HostError::Protocol(
                            "only the current driver may fork a session".to_owned(),
                        ));
                    }
                    if snapshot.running || snapshot.active_shell.is_some() {
                        return Err(HostError::Protocol(
                            "forking requires an idle session".to_owned(),
                        ));
                    }
                    let explicit_turn = at_turn.is_some();
                    let resolved_turn = if let Some(turn) = &at_turn {
                        let turn = turn.0.parse::<u64>().map_err(|_| {
                            HostError::Protocol("fork turn must be an unsigned decimal".to_owned())
                        })?;
                        if turn == 0 || turn > snapshot.completed_turns {
                            return Err(HostError::Protocol(
                                "fork turn is not a completed parent boundary".to_owned(),
                            ));
                        }
                        turn
                    } else {
                        snapshot.completed_turns
                    };
                    let through_sequence = if explicit_turn {
                        None
                    } else {
                        let tail = parent.handle().last_sequence().await?;
                        let verified = parent.handle().snapshot().await?;
                        let verified_tail = parent.handle().last_sequence().await?;
                        if verified.running
                            || verified.active_shell.is_some()
                            || verified.completed_turns != snapshot.completed_turns
                            || verified.driver_client_id != snapshot.driver_client_id
                            || verified_tail != tail
                        {
                            return Err(HostError::Protocol(
                                "parent changed while the fork boundary was captured; retry"
                                    .to_owned(),
                            ));
                        }
                        tail
                    };
                    self.factory
                        .prepare_fork_operation(PreparedForkOperation {
                            key: key.clone(),
                            request: ForkSessionRequest {
                                operation_key: key.clone(),
                                parent: parent.descriptor(),
                                child_session_id: self.factory.allocate_session_id()?,
                                at_turn: TurnId(resolved_turn.to_string()),
                                through_sequence,
                                include_idle_tail: !explicit_turn,
                                driver_client_id: meta.client_id.clone(),
                            },
                        })
                        .await?
                }
            };
            let child_session_id = operation.request.child_session_id.clone();
            let child = match self.fork_session(operation.request.clone()).await {
                Ok(child) => child,
                Err(error @ (HostError::SessionCapacity | HostError::ShuttingDown)) => {
                    self.factory.abandon_prepared_fork_operation(&key).await?;
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            let attach = if operation.request.driver_client_id == meta.client_id {
                ClientCommand::AttachSession {
                    meta: meta.clone(),
                    session_id: child_session_id,
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                }
            } else {
                ClientCommand::TakeDriver {
                    meta: meta.clone(),
                    session_id: child_session_id,
                }
            };
            let outcome = child.handle().dispatch(attach).await?;
            if outcome != CommandOutcome::Accepted {
                return Err(HostError::Persistence(
                    "fork child could not attach its authorized driver".to_owned(),
                ));
            }
            child.set_driver(Some(meta.client_id.clone()));
            let completed = CompletedForkOperation {
                protocol_version: rw_types::PROTOCOL_VERSION,
                command_ack_emitted_at: self.clock.emitted_at(),
                fork_event_emitted_at: self.clock.emitted_at(),
                acknowledged_session_id: session_id.clone(),
                outcome,
                parent_session_id: session_id,
                child: child.descriptor(),
                at_turn: operation.request.at_turn,
            };
            let completed = self
                .factory
                .complete_fork_operation(&key, &completed)
                .await?;
            drop(lifecycle_guard);
            Ok(completed)
        }
        .await;
        match result {
            Ok(completed) => completed_fork_dispatch(&key, completed),
            Err(error) => {
                let outcome = rejected(host_error_code(&error), &error.to_string());
                CachedDispatch {
                    events: vec![command_ack(&meta, None, outcome.clone(), &*self.clock)],
                    outcome,
                    // A durable operation may already exist. Never strand it behind
                    // a process-local cached failure; the same request may retry.
                    cacheable: false,
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_inner(
        &self,
        command: ClientCommand,
    ) -> Result<(CommandOutcome, Option<SessionId>, Vec<EngineEvent>), HostError> {
        if self.shutting_down.load(Ordering::Acquire)
            && !matches!(command, ClientCommand::ShutdownHost { .. })
        {
            return Err(HostError::ShuttingDown);
        }
        match command {
            ClientCommand::CreateSession { meta, cwd, model } => {
                let session_id = self.factory.allocate_session_id()?;
                let session = self
                    .create_session(CreateSessionRequest {
                        session_id: session_id.clone(),
                        workspace: cwd,
                        model,
                    })
                    .await?;
                let outcome = session
                    .handle()
                    .dispatch(ClientCommand::AttachSession {
                        meta: meta.clone(),
                        session_id: session_id.clone(),
                        last_seen_sequence: None,
                        role: ClientRole::Driver,
                    })
                    .await?;
                if outcome == CommandOutcome::Accepted {
                    session.set_driver(Some(meta.client_id.clone()));
                }
                Ok((
                    outcome,
                    Some(session_id),
                    vec![EngineEvent::SessionsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        sessions: vec![session.descriptor()],
                    }],
                ))
            }
            ClientCommand::ResumeSession {
                meta,
                session_id,
                last_seen_sequence,
                role,
            } => {
                let session = self.resume_session(&session_id).await?;
                let _lifecycle_guard = match role {
                    ClientRole::Driver => Some(Arc::clone(&session.lifecycle).lock_owned().await),
                    ClientRole::Observer => None,
                };
                let outcome = session
                    .handle()
                    .dispatch(ClientCommand::AttachSession {
                        meta: meta.clone(),
                        session_id: session_id.clone(),
                        last_seen_sequence,
                        role: role.clone(),
                    })
                    .await?;
                if outcome == CommandOutcome::Accepted && role == ClientRole::Driver {
                    session.set_driver(Some(meta.client_id.clone()));
                }
                Ok((
                    outcome,
                    Some(session_id),
                    vec![EngineEvent::SessionsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        sessions: vec![session.descriptor()],
                    }],
                ))
            }
            ClientCommand::ListSessions { meta } => {
                let mut sessions = self.factory.persisted_sessions().await?;
                let registry = self.registry.lock().await;
                for slot in registry.sessions.values() {
                    if let SessionSlot::Ready(session) = slot {
                        let descriptor = session.descriptor();
                        sessions.retain(|existing| existing.session_id != descriptor.session_id);
                        sessions.push(descriptor);
                    }
                }
                sessions.sort_by(|left, right| left.session_id.0.cmp(&right.session_id.0));
                Ok((
                    CommandOutcome::Accepted,
                    None,
                    vec![EngineEvent::SessionsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        sessions,
                    }],
                ))
            }
            ClientCommand::SearchSessions { meta, query, limit } => {
                if query.trim().is_empty() || query.len() > 512 || !(1..=1_000).contains(&limit) {
                    return Err(HostError::Protocol(
                        "session search query or limit is invalid".to_owned(),
                    ));
                }
                let (sessions, truncated) = self
                    .factory
                    .search_persisted_sessions(&query, limit)
                    .await?;
                Ok((
                    CommandOutcome::Accepted,
                    None,
                    vec![EngineEvent::SessionsSearchReady {
                        meta: ack_meta(&meta, &*self.clock),
                        query,
                        sessions,
                        truncated,
                    }],
                ))
            }
            ClientCommand::ListCommands { meta } => Ok((
                CommandOutcome::Accepted,
                None,
                vec![EngineEvent::CommandDescriptorsListed {
                    meta: ack_meta(&meta, &*self.clock),
                    commands: self.queries.command_descriptors().await?,
                }],
            )),
            ClientCommand::ListModels { meta } => Ok((
                CommandOutcome::Accepted,
                None,
                vec![EngineEvent::ModelsListed {
                    meta: ack_meta(&meta, &*self.clock),
                    models: self.queries.model_descriptors().await?,
                }],
            )),
            ClientCommand::SearchWorkspaceFiles {
                meta,
                session_id,
                query,
                limit,
            } => {
                let session = self.ready_session(&session_id).await?;
                let (matches, truncated) = self
                    .queries
                    .search_workspace_files(&session.descriptor(), &query, limit)
                    .await?;
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::WorkspaceFilesFound {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        matches,
                        truncated,
                    }],
                ))
            }
            ClientCommand::PreviewWorkspaceFile {
                meta,
                session_id,
                path,
                max_bytes,
            } => {
                let session = self.ready_session(&session_id).await?;
                let preview = self
                    .queries
                    .preview_workspace_file(&session.descriptor(), &path, max_bytes)
                    .await?;
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::WorkspaceFilePreviewReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        preview,
                    }],
                ))
            }
            ClientCommand::GetWorkspaceStatus { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let status = self.queries.workspace_status(&session.descriptor()).await?;
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::WorkspaceStatusReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        status,
                    }],
                ))
            }
            ClientCommand::ShutdownHost { meta } => {
                let opening_waiters = {
                    let mut registry = self.registry.lock().await;
                    // The shutdown flag and registry transition share this
                    // lock with every post-factory insertion check. This is
                    // the host's shutdown linearization point: an opener
                    // either inserts before it and is drained, or observes
                    // shutdown and can never insert.
                    self.shutting_down.store(true, Ordering::Release);
                    registry
                        .sessions
                        .drain()
                        .filter_map(|(_, slot)| match slot {
                            SessionSlot::Opening(completed) => Some(completed),
                            SessionSlot::Ready(_) => None,
                        })
                        .collect::<Vec<_>>()
                };
                // Clearing an Opening reservation without completing its
                // signal strands waiters forever. Cancel every opening before awaiting
                // factory shutdown; each rechecks `shutting_down` and exits.
                for completed in opening_waiters {
                    completed.send_replace(true);
                }
                self.factory.shutdown().await?;
                Ok((
                    CommandOutcome::Accepted,
                    None,
                    vec![EngineEvent::HostShutdown {
                        meta: ack_meta(&meta, &*self.clock),
                    }],
                ))
            }
            command => {
                let session_id = command_session_id(&command)
                    .ok_or_else(|| HostError::Protocol("command has no session id".to_owned()))?;
                let session = self.ready_session(&session_id).await?;
                let driver = match &command {
                    ClientCommand::TakeDriver { meta, .. } => Some(meta.client_id.clone()),
                    _ => None,
                };
                let lifecycle = matches!(command, ClientCommand::TakeDriver { .. })
                    .then(|| Arc::clone(&session.lifecycle));
                let _lifecycle = match lifecycle {
                    Some(lifecycle) => Some(lifecycle.lock_owned().await),
                    None => None,
                };
                let outcome = session.handle().dispatch(command).await?;
                if outcome == CommandOutcome::Accepted {
                    // TakeDriver persists its lease before returning Accepted.
                    // Model and shell commands acknowledge before their
                    // durable event; those descriptor fields are therefore
                    // updated only by `project_durable_descriptor`.
                    if let Some(driver) = driver {
                        session.set_driver(Some(driver));
                    }
                }
                Ok((outcome, Some(session_id), Vec::new()))
            }
        }
    }

    async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<Arc<HostedSession>, HostError> {
        {
            let mut registry = self.registry.lock().await;
            if self.shutting_down.load(Ordering::Acquire) {
                return Err(HostError::ShuttingDown);
            }
            if registry
                .sessions
                .len()
                .saturating_add(registry.anonymous_openings)
                >= self.config.max_sessions
            {
                return Err(HostError::SessionCapacity);
            }
            if registry.sessions.contains_key(&request.session_id) {
                return Err(HostError::Protocol(
                    "allocated session id already exists".to_owned(),
                ));
            }
            registry.anonymous_openings = registry.anonymous_openings.saturating_add(1);
        }
        let created = match self.factory.create(request.clone()).await {
            Ok(session)
                if session.descriptor().session_id == request.session_id
                    && session.handle().session_id() == &request.session_id =>
            {
                let session = Arc::new(session);
                match session.project_durable_descriptor().await {
                    Ok(()) => Ok(session),
                    Err(error) => Err(error),
                }
            }
            Ok(_) => Err(HostError::SessionIdentityMismatch),
            Err(error) => Err(error),
        };
        let mut registry = self.registry.lock().await;
        registry.anonymous_openings = registry.anonymous_openings.saturating_sub(1);
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(HostError::ShuttingDown);
        }
        let session = created?;
        registry
            .sessions
            .insert(request.session_id, SessionSlot::Ready(Arc::clone(&session)));
        Ok(session)
    }

    async fn fork_session(
        &self,
        request: ForkSessionRequest,
    ) -> Result<Arc<HostedSession>, HostError> {
        loop {
            let wait = {
                let mut registry = self.registry.lock().await;
                if self.shutting_down.load(Ordering::Acquire) {
                    return Err(HostError::ShuttingDown);
                }
                match registry.sessions.get(&request.child_session_id) {
                    Some(SessionSlot::Ready(session)) => return Ok(Arc::clone(session)),
                    Some(SessionSlot::Opening(completed)) => Some(completed.subscribe()),
                    None => {
                        if registry
                            .sessions
                            .len()
                            .saturating_add(registry.anonymous_openings)
                            >= self.config.max_sessions
                        {
                            return Err(HostError::SessionCapacity);
                        }
                        let (completed, _) = watch::channel(false);
                        registry.sessions.insert(
                            request.child_session_id.clone(),
                            SessionSlot::Opening(completed),
                        );
                        None
                    }
                }
            };
            let Some(mut wait) = wait else { break };
            wait.changed()
                .await
                .map_err(|_| HostError::SessionNotLoaded(request.child_session_id.0.clone()))?;
        }
        let forked = match self.factory.fork(request.clone()).await {
            Ok(session)
                if session.descriptor().session_id == request.child_session_id
                    && session.handle().session_id() == &request.child_session_id =>
            {
                let session = Arc::new(session);
                session.project_durable_descriptor().await.map(|()| session)
            }
            Ok(_) => Err(HostError::SessionIdentityMismatch),
            Err(error) => Err(error),
        };
        let mut registry = self.registry.lock().await;
        let completed = match registry.sessions.remove(&request.child_session_id) {
            Some(SessionSlot::Opening(completed)) => Some(completed),
            Some(SessionSlot::Ready(_)) | None => None,
        };
        if self.shutting_down.load(Ordering::Acquire) {
            if let Some(completed) = completed {
                completed.send_replace(true);
            }
            return Err(HostError::ShuttingDown);
        }
        let session = match forked {
            Ok(session) => session,
            Err(error) => {
                if let Some(completed) = completed {
                    completed.send_replace(true);
                }
                return Err(error);
            }
        };
        registry.sessions.insert(
            request.child_session_id,
            SessionSlot::Ready(Arc::clone(&session)),
        );
        if let Some(completed) = completed {
            completed.send_replace(true);
        }
        Ok(session)
    }

    async fn resume_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<HostedSession>, HostError> {
        loop {
            let wait = {
                let mut registry = self.registry.lock().await;
                if self.shutting_down.load(Ordering::Acquire) {
                    return Err(HostError::ShuttingDown);
                }
                match registry.sessions.get(session_id) {
                    Some(SessionSlot::Ready(session)) => return Ok(Arc::clone(session)),
                    Some(SessionSlot::Opening(completed)) => Some(completed.subscribe()),
                    None => {
                        if registry
                            .sessions
                            .len()
                            .saturating_add(registry.anonymous_openings)
                            >= self.config.max_sessions
                        {
                            return Err(HostError::SessionCapacity);
                        }
                        let (completed, receiver) = watch::channel(false);
                        drop(receiver);
                        registry
                            .sessions
                            .insert(session_id.clone(), SessionSlot::Opening(completed));
                        None
                    }
                }
            };
            if let Some(mut completed) = wait {
                if !*completed.borrow_and_update() {
                    let _ = completed.changed().await;
                }
                continue;
            }

            let opened = match self.factory.resume(session_id).await {
                Ok(session)
                    if session.descriptor().session_id == *session_id
                        && session.handle().session_id() == session_id =>
                {
                    let session = Arc::new(session);
                    match session.project_durable_descriptor().await {
                        Ok(()) => Ok(session),
                        Err(error) => Err(error),
                    }
                }
                Ok(_) => Err(HostError::SessionIdentityMismatch),
                Err(error) => Err(error),
            };
            let mut registry = self.registry.lock().await;
            let completed = match registry.sessions.remove(session_id) {
                Some(SessionSlot::Opening(completed)) => Some(completed),
                Some(SessionSlot::Ready(session)) => {
                    registry
                        .sessions
                        .insert(session_id.clone(), SessionSlot::Ready(session));
                    None
                }
                None => None,
            };
            let result = if self.shutting_down.load(Ordering::Acquire) {
                Err(HostError::ShuttingDown)
            } else {
                match opened {
                    Ok(session) => {
                        registry
                            .sessions
                            .insert(session_id.clone(), SessionSlot::Ready(Arc::clone(&session)));
                        Ok(session)
                    }
                    Err(error) => Err(error),
                }
            };
            drop(registry);
            if let Some(completed) = completed {
                completed.send_replace(true);
            }
            return result;
        }
    }

    async fn ready_session(&self, session_id: &SessionId) -> Result<Arc<HostedSession>, HostError> {
        self.session(session_id)
            .await
            .ok_or_else(|| HostError::SessionNotLoaded(session_id.0.clone()))
    }

    /// Subscribes to connection-scoped host results and, optionally, one
    /// session's durable replay/live stream. A replay-complete marker is emitted
    /// after the captured durable tail, never before it.
    /// Subscribes one authenticated client to host results and an optional
    /// durable session replay/live stream.
    ///
    /// # Errors
    ///
    /// Returns a typed host error when the session is unavailable or the
    /// requested replay cursor is invalid.
    pub async fn subscribe(
        &self,
        bound: BoundClient,
        session_id: Option<SessionId>,
        last_seen: Option<SequenceId>,
    ) -> Result<mpsc::Receiver<Result<EngineEvent, HostError>>, HostError> {
        let sender = self.client_sender(&bound.client_id);
        let mut host_events = sender.subscribe();
        let (send, receive) = mpsc::channel(HOST_EVENT_CAPACITY);
        let session = if let Some(session_id) = &session_id {
            Some(self.ready_session(session_id).await?)
        } else {
            None
        };
        let clock = Arc::clone(&self.clock);
        tokio::spawn(async move {
            if let Some(session) = session {
                let captured_tail = match session.handle().last_sequence().await {
                    Ok(tail) => tail,
                    Err(error) => {
                        let _ = send.send(Err(HostError::from(error))).await;
                        return;
                    }
                };
                let cursor_ahead = match (last_seen, captured_tail) {
                    (Some(_), None) => true,
                    (Some(seen), Some(tail)) => seen > tail,
                    (None, _) => false,
                };
                if cursor_ahead {
                    let _ = send
                        .send(Err(HostError::Protocol(
                            "last seen sequence is ahead of the durable log".to_owned(),
                        )))
                        .await;
                    return;
                }
                let mut session_events = session
                    .handle()
                    .subscribe_client(bound.client_id.clone(), last_seen);
                let mut replay_complete = last_seen == captured_tail;
                if replay_complete {
                    let _ = send
                        .send(Ok(replay_completed(
                            &bound.client_id,
                            &session.descriptor().session_id,
                            captured_tail,
                            &*clock,
                        )))
                        .await;
                }
                loop {
                    tokio::select! {
                        host = host_events.recv() => match host {
                            Ok(event) => if send.send(Ok(event)).await.is_err() { return; },
                            Err(broadcast::error::RecvError::Lagged(_)) => {},
                            Err(broadcast::error::RecvError::Closed) => return,
                        },
                        event = session_events.recv() => match event {
                            Ok(event) => {
                                if !matches!(event, EngineEvent::CommandAcknowledged { .. })
                                    && send.send(Ok(event.clone())).await.is_err()
                                {
                                    return;
                                }
                                if !replay_complete
                                    && event.meta().map(|meta| meta.sequence_id) == captured_tail
                                {
                                    replay_complete = true;
                                    if send.send(Ok(replay_completed(
                                        &bound.client_id,
                                        &session.descriptor().session_id,
                                        captured_tail,
                                        &*clock,
                                    ))).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(error) => {
                                let _ = send.send(Err(HostError::from(error))).await;
                                return;
                            }
                        }
                    }
                }
            } else {
                loop {
                    match host_events.recv().await {
                        Ok(event) => {
                            if send.send(Ok(event)).await.is_err() {
                                return;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        });
        Ok(receive)
    }

    fn client_sender(&self, client_id: &ClientId) -> broadcast::Sender<EngineEvent> {
        let mut clients = self
            .client_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clients
            .entry(client_id.clone())
            .or_insert_with(|| broadcast::channel(HOST_EVENT_CAPACITY).0)
            .clone()
    }

    fn emit_one(&self, client_id: &ClientId, event: EngineEvent) {
        let _ = self.client_sender(client_id).send(event);
    }

    fn emit_many(&self, client_id: &ClientId, events: &[EngineEvent]) {
        let sender = self.client_sender(client_id);
        for event in events {
            let _ = sender.send(event.clone());
        }
    }
}

fn ack_meta(meta: &CommandMeta, clock: &dyn EventClock) -> CommandAckMeta {
    CommandAckMeta {
        protocol_version: rw_types::PROTOCOL_VERSION,
        client_id: meta.client_id.clone(),
        request_id: meta.request_id.clone(),
        emitted_at: clock.emitted_at(),
    }
}

fn command_ack(
    meta: &CommandMeta,
    session_id: Option<SessionId>,
    outcome: CommandOutcome,
    clock: &dyn EventClock,
) -> EngineEvent {
    EngineEvent::CommandAcknowledged {
        meta: ack_meta(meta, clock),
        session_id,
        outcome,
    }
}

fn completed_fork_dispatch(
    key: &ForkOperationKey,
    completed: CompletedForkOperation,
) -> CachedDispatch {
    let ack_meta = CommandAckMeta {
        protocol_version: completed.protocol_version,
        client_id: key.client_id.clone(),
        request_id: key.request_id.clone(),
        emitted_at: completed.command_ack_emitted_at,
    };
    let fork_meta = CommandAckMeta {
        emitted_at: completed.fork_event_emitted_at,
        ..ack_meta.clone()
    };
    CachedDispatch {
        outcome: completed.outcome.clone(),
        events: vec![
            EngineEvent::CommandAcknowledged {
                meta: ack_meta,
                session_id: Some(completed.acknowledged_session_id),
                outcome: completed.outcome,
            },
            EngineEvent::SessionForked {
                meta: fork_meta,
                parent_session_id: completed.parent_session_id,
                child: completed.child,
                at_turn: completed.at_turn,
            },
        ],
        cacheable: true,
    }
}

fn replay_completed(
    client_id: &ClientId,
    session_id: &SessionId,
    through_sequence: Option<SequenceId>,
    clock: &dyn EventClock,
) -> EngineEvent {
    EngineEvent::SessionReplayCompleted {
        meta: CommandAckMeta {
            protocol_version: rw_types::PROTOCOL_VERSION,
            client_id: client_id.clone(),
            request_id: RequestId("session-replay".to_owned()),
            emitted_at: clock.emitted_at(),
        },
        session_id: session_id.clone(),
        through_sequence,
    }
}

fn rejected(code: &str, message: &str) -> CommandOutcome {
    CommandOutcome::Rejected {
        error: EngineError {
            category: EngineErrorCategory::Protocol,
            code: code.to_owned(),
            message: message.to_owned(),
            retryable: false,
            details: None,
        },
    }
}

fn host_error_code(error: &HostError) -> &'static str {
    match error {
        HostError::ShuttingDown => "host_shutting_down",
        HostError::SessionCapacity => "session_capacity",
        HostError::SessionNotLoaded(_) => "session_not_loaded",
        HostError::SessionIdentityMismatch => "session_identity_mismatch",
        HostError::RequestConflict => "request_id_conflict",
        HostError::Persistence(_) => "host_persistence_failure",
        HostError::Query(_) => "host_query_failure",
        HostError::Protocol(_) => "host_protocol_failure",
    }
}

fn command_session_id(command: &ClientCommand) -> Option<SessionId> {
    match command {
        ClientCommand::ResumeSession { session_id, .. }
        | ClientCommand::AttachSession { session_id, .. }
        | ClientCommand::SendMessage { session_id, .. }
        | ClientCommand::Interrupt { session_id, .. }
        | ClientCommand::ApproveTool { session_id, .. }
        | ClientCommand::ApprovePlan { session_id, .. }
        | ClientCommand::AnswerQuestion { session_id, .. }
        | ClientCommand::SwitchMode { session_id, .. }
        | ClientCommand::SwitchModel { session_id, .. }
        | ClientCommand::Compact { session_id, .. }
        | ClientCommand::Fork { session_id, .. }
        | ClientCommand::Rewind { session_id, .. }
        | ClientCommand::TakeDriver { session_id, .. }
        | ClientCommand::UserShellStarted { session_id, .. }
        | ClientCommand::UserShellEnded { session_id, .. }
        | ClientCommand::PinContext { session_id, .. }
        | ClientCommand::EvictContext { session_id, .. }
        | ClientCommand::GetContext { session_id, .. }
        | ClientCommand::GetCost { session_id, .. }
        | ClientCommand::DumpPrompt { session_id, .. }
        | ClientCommand::GetSessionReview { session_id, .. }
        | ClientCommand::ReviewFile { session_id, .. }
        | ClientCommand::SearchWorkspaceFiles { session_id, .. }
        | ClientCommand::PreviewWorkspaceFile { session_id, .. }
        | ClientCommand::GetWorkspaceStatus { session_id, .. } => Some(session_id.clone()),
        ClientCommand::CreateSession { .. }
        | ClientCommand::ListSessions { .. }
        | ClientCommand::SearchSessions { .. }
        | ClientCommand::ListCommands { .. }
        | ClientCommand::ListModels { .. }
        | ClientCommand::ShutdownHost { .. } => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{
        sync::atomic::{AtomicU8, AtomicUsize},
        time::Duration,
    };

    use futures_util::stream;
    use rw_types::{
        AttachmentData, CommandMeta, ModelCacheBehavior, ModelCapabilities, PROTOCOL_VERSION,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::{
        ModelDriver, NoopFolderTrustController, NoopMutationCheckpointCoordinator,
        NoopSecretRedactor, NoopSessionEventSink, PermissionGate, SessionActor, SessionActorConfig,
        SessionEventSink, SessionRecoveredState, builtin_command_registry, builtin_hook_dispatcher,
        runtime_support::{
            BoxEventStream, PermissionDecision, ProviderRequest, ThinkingLevel, ToolRegistry,
        },
    };

    struct IdleModel;

    impl ModelDriver for IdleModel {
        fn stream(
            &self,
            _alias: &str,
            _request: ProviderRequest,
        ) -> Result<BoxEventStream, AgentLoopError> {
            Ok(Box::pin(stream::empty()))
        }

        fn has_model_alias(&self, alias: &str) -> bool {
            matches!(alias, "fast" | "big")
        }
    }

    struct StubFactory {
        root: TempDir,
        next: AtomicUsize,
        resumes: AtomicUsize,
        fail_resume_once: AtomicBool,
        block_create: AtomicBool,
        block_resume: AtomicBool,
        block_fork: AtomicBool,
        create_started: Notify,
        create_release: Notify,
        resume_started: Notify,
        resume_release: Notify,
        fork_started: Notify,
        fork_release: Notify,
        fork_turns: Mutex<Vec<TurnId>>,
        shutdowns: AtomicUsize,
        event_sink: Option<Arc<dyn SessionEventSink>>,
    }

    impl StubFactory {
        fn new() -> Self {
            Self {
                root: TempDir::new().expect("host test root"),
                next: AtomicUsize::new(1),
                resumes: AtomicUsize::new(0),
                fail_resume_once: AtomicBool::new(false),
                block_create: AtomicBool::new(false),
                block_resume: AtomicBool::new(false),
                block_fork: AtomicBool::new(false),
                create_started: Notify::new(),
                create_release: Notify::new(),
                resume_started: Notify::new(),
                resume_release: Notify::new(),
                fork_started: Notify::new(),
                fork_release: Notify::new(),
                fork_turns: Mutex::new(Vec::new()),
                shutdowns: AtomicUsize::new(0),
                event_sink: None,
            }
        }

        fn with_event_sink(event_sink: Arc<dyn SessionEventSink>) -> Self {
            Self {
                event_sink: Some(event_sink),
                ..Self::new()
            }
        }

        fn session(&self, session_id: &SessionId) -> HostedSession {
            let workspace = self.root.path().join(&session_id.0);
            std::fs::create_dir_all(&workspace).expect("session workspace");
            let handle = SessionActor::spawn(SessionActorConfig {
                session_id: session_id.clone(),
                workspace_root: workspace,
                additional_workspace_roots: Vec::new(),
                workspace_generation: 0,
                initial_session_context: Vec::new(),
                model_alias: "fast".to_owned(),
                model: Arc::new(IdleModel),
                tools: Arc::new(ToolRegistry::new()),
                permissions: Arc::new(PermissionGate::new(PermissionDecision::Allow)),
                hooks: Arc::new(builtin_hook_dispatcher().expect("hooks")),
                commands: Arc::new(builtin_command_registry().expect("commands")),
                event_sink: self
                    .event_sink
                    .clone()
                    .unwrap_or_else(|| Arc::new(NoopSessionEventSink::default())),
                event_clock: Arc::new(SystemEventClock),
                secret_redactor: Arc::new(NoopSecretRedactor),
                checkpoints: Arc::new(NoopMutationCheckpointCoordinator),
                folder_trust: Arc::new(NoopFolderTrustController),
                workspace_roots: Arc::new(crate::NoopWorkspaceRootController),
                recovered: SessionRecoveredState::default(),
                max_turns: 2,
                identical_tool_failure_limit: 2,
                max_output_tokens: 128,
                thinking: ThinkingLevel::Off,
                event_capacity: 64,
            })
            .expect("session actor");
            HostedSession::new(
                SessionDescriptor {
                    session_id: session_id.clone(),
                    workspace_name: session_id.0.clone(),
                    model: ModelAlias("fast".to_owned()),
                    driver_client_id: None,
                    shell_active: false,
                },
                handle,
            )
        }
    }

    #[async_trait]
    impl SessionFactory for StubFactory {
        fn allocate_session_id(&self) -> Result<SessionId, HostError> {
            Ok(SessionId(format!(
                "created-{}",
                self.next.fetch_add(1, Ordering::Relaxed)
            )))
        }

        async fn create(&self, request: CreateSessionRequest) -> Result<HostedSession, HostError> {
            if self.block_create.load(Ordering::Acquire) {
                self.create_started.notify_one();
                self.create_release.notified().await;
            }
            Ok(self.session(&request.session_id))
        }

        async fn resume(&self, session_id: &SessionId) -> Result<HostedSession, HostError> {
            self.resumes.fetch_add(1, Ordering::Relaxed);
            if self.block_resume.load(Ordering::Acquire) {
                self.resume_started.notify_one();
                self.resume_release.notified().await;
            } else {
                tokio::task::yield_now().await;
            }
            if self.fail_resume_once.swap(false, Ordering::AcqRel) {
                return Err(HostError::Persistence("injected resume failure".to_owned()));
            }
            Ok(self.session(session_id))
        }

        async fn fork(&self, request: ForkSessionRequest) -> Result<HostedSession, HostError> {
            self.fork_turns
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.at_turn.clone());
            if self.block_fork.load(Ordering::Acquire) {
                self.fork_started.notify_one();
                self.fork_release.notified().await;
            }
            Ok(self.session(&request.child_session_id))
        }

        async fn shutdown(&self) -> Result<(), HostError> {
            self.shutdowns.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    const BLOCK_MODEL: u8 = 1;
    const BLOCK_SHELL_ACTIVE: u8 = 2;
    const BLOCK_SHELL_INACTIVE: u8 = 3;

    #[derive(Default)]
    struct BlockingDescriptorSink {
        inner: NoopSessionEventSink,
        block: AtomicU8,
        append_started: Notify,
        append_release: Notify,
    }

    impl BlockingDescriptorSink {
        fn block(&self, target: u8) {
            self.block.store(target, Ordering::Release);
        }

        fn release(&self) {
            self.append_release.notify_one();
        }

        fn event_target(event: &EngineEvent) -> u8 {
            match event {
                EngineEvent::ModelChanged { .. } => BLOCK_MODEL,
                EngineEvent::UserShellStateChanged { active: true, .. } => BLOCK_SHELL_ACTIVE,
                EngineEvent::UserShellStateChanged { active: false, .. } => BLOCK_SHELL_INACTIVE,
                _ => 0,
            }
        }
    }

    #[async_trait]
    impl SessionEventSink for BlockingDescriptorSink {
        async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
            let target = Self::event_target(&event);
            if target != 0 && target == self.block.load(Ordering::Acquire) {
                self.append_started.notify_one();
                self.append_release.notified().await;
            }
            self.inner.append(event).await
        }

        async fn read_after(
            &self,
            last_seen: Option<SequenceId>,
        ) -> Result<Vec<EngineEvent>, AgentLoopError> {
            self.inner.read_after(last_seen).await
        }
    }

    #[derive(Default)]
    struct StubQueries;

    #[async_trait]
    impl HostQueryService for StubQueries {
        async fn command_descriptors(&self) -> Result<Vec<CommandDescriptor>, HostError> {
            Ok(vec![CommandDescriptor {
                name: "help".to_owned(),
                description: "Show help".to_owned(),
                usage: "/help".to_owned(),
            }])
        }

        async fn model_descriptors(&self) -> Result<Vec<ModelDescriptor>, HostError> {
            Ok(vec![ModelDescriptor {
                alias: ModelAlias("fast".to_owned()),
                capabilities: ModelCapabilities {
                    tool_calling: true,
                    vision: false,
                    thinking: false,
                    cache_behavior: ModelCacheBehavior::None,
                },
            }])
        }

        async fn search_workspace_files(
            &self,
            _session: &SessionDescriptor,
            _query: &str,
            _limit: u32,
        ) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
            Ok((Vec::new(), false))
        }

        async fn preview_workspace_file(
            &self,
            _session: &SessionDescriptor,
            path: &str,
            _max_bytes: u32,
        ) -> Result<WorkspaceFilePreview, HostError> {
            Ok(WorkspaceFilePreview {
                path: path.to_owned(),
                media_type: "text/plain".to_owned(),
                data: AttachmentData::Text {
                    content: String::new(),
                },
                total_bytes: 0,
                truncated: false,
            })
        }

        async fn workspace_status(
            &self,
            session: &SessionDescriptor,
        ) -> Result<WorkspaceStatus, HostError> {
            Ok(WorkspaceStatus {
                workspace_name: session.workspace_name.clone(),
                branch: None,
                changed_paths: Vec::new(),
                truncated: false,
            })
        }
    }

    fn meta(client: &str, request: &str) -> CommandMeta {
        CommandMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId(client.to_owned()),
            request_id: RequestId(request.to_owned()),
        }
    }

    fn host(max_sessions: usize) -> (EngineHost, Arc<StubFactory>) {
        let factory = Arc::new(StubFactory::new());
        let host = EngineHost::new(
            EngineHostConfig {
                max_sessions,
                max_deduplicated_requests: 32,
            },
            factory.clone(),
            Arc::new(StubQueries),
        )
        .expect("host");
        (host, factory)
    }

    #[tokio::test]
    async fn concurrent_resume_opens_one_actor_and_capacity_is_atomic() {
        let (host, factory) = host(1);
        let session = SessionId("shared".to_owned());
        let left = host.dispatch(
            BoundClient {
                client_id: ClientId("left".to_owned()),
            },
            ClientCommand::ResumeSession {
                meta: meta("spoofed", "left-resume"),
                session_id: session.clone(),
                last_seen_sequence: None,
                role: ClientRole::Observer,
            },
        );
        let right = host.dispatch(
            BoundClient {
                client_id: ClientId("right".to_owned()),
            },
            ClientCommand::ResumeSession {
                meta: meta("spoofed", "right-resume"),
                session_id: session.clone(),
                last_seen_sequence: None,
                role: ClientRole::Observer,
            },
        );
        let (left, right) = tokio::join!(left, right);
        assert_eq!(left, CommandOutcome::Accepted);
        assert_eq!(right, CommandOutcome::Accepted);
        assert_eq!(factory.resumes.load(Ordering::Relaxed), 1);

        let rejected = host
            .dispatch(
                BoundClient {
                    client_id: ClientId("third".to_owned()),
                },
                ClientCommand::ResumeSession {
                    meta: meta("third", "capacity"),
                    session_id: SessionId("second".to_owned()),
                    last_seen_sequence: None,
                    role: ClientRole::Observer,
                },
            )
            .await;
        assert!(matches!(
            rejected,
            CommandOutcome::Rejected { error } if error.code == "session_capacity"
        ));
    }

    #[tokio::test]
    async fn fork_requires_idle_driver_and_returns_typed_child_descriptor() {
        let (host, _factory) = host(3);
        let parent = SessionId("fork-parent".to_owned());
        let driver = BoundClient {
            client_id: ClientId("fork-driver".to_owned()),
        };
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::ResumeSession {
                    meta: meta("spoofed", "fork-resume"),
                    session_id: parent.clone(),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let mut events = host
            .subscribe(driver.clone(), None, None)
            .await
            .expect("host events");
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::Fork {
                    meta: meta("spoofed", "fork-now"),
                    session_id: parent.clone(),
                    at_turn: None,
                    operation_id: Some("fork-now-operation".to_owned()),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let child = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let EngineEvent::SessionForked {
                    parent_session_id,
                    child,
                    at_turn,
                    ..
                } = events
                    .recv()
                    .await
                    .expect("fork event")
                    .expect("fork result")
                {
                    break (parent_session_id, child, at_turn);
                }
            }
        })
        .await
        .expect("typed fork result");
        assert_eq!(child.0, parent);
        assert_eq!(child.1.session_id, SessionId("created-1".to_owned()));
        assert_eq!(child.1.driver_client_id, Some(driver.client_id.clone()));
        assert_eq!(child.2, TurnId("0".to_owned()));
        assert!(host.session(&parent).await.is_some());
        assert!(host.session(&child.1.session_id).await.is_some());

        let rejected = host
            .dispatch(
                driver,
                ClientCommand::Fork {
                    meta: meta("spoofed", "fork-invalid-turn"),
                    session_id: parent,
                    at_turn: Some(TurnId("1".to_owned())),
                    operation_id: Some("fork-invalid-operation".to_owned()),
                },
            )
            .await;
        assert!(matches!(
            rejected,
            CommandOutcome::Rejected { error }
                if error.message.contains("not a completed parent boundary")
        ));
    }

    #[tokio::test]
    async fn concurrent_fork_waits_for_takeover_and_revalidates_actor_driver() {
        let (host, factory) = host(4);
        let parent = SessionId("fork-race-parent".to_owned());
        let first = BoundClient {
            client_id: ClientId("first-driver".to_owned()),
        };
        let second = BoundClient {
            client_id: ClientId("second-driver".to_owned()),
        };
        assert_eq!(
            host.dispatch(
                first.clone(),
                ClientCommand::ResumeSession {
                    meta: meta("spoofed", "race-resume"),
                    session_id: parent.clone(),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        factory.block_fork.store(true, Ordering::Release);
        let first_fork = tokio::spawn({
            let host = host.clone();
            let first = first.clone();
            let parent = parent.clone();
            async move {
                host.dispatch(
                    first,
                    ClientCommand::Fork {
                        meta: meta("spoofed", "first-fork"),
                        session_id: parent,
                        at_turn: None,
                        operation_id: Some("first-fork-operation".to_owned()),
                    },
                )
                .await
            }
        });
        factory.fork_started.notified().await;
        let takeover = tokio::spawn({
            let host = host.clone();
            let second = second.clone();
            let parent = parent.clone();
            async move {
                host.dispatch(
                    second,
                    ClientCommand::TakeDriver {
                        meta: meta("spoofed", "takeover"),
                        session_id: parent,
                    },
                )
                .await
            }
        });
        tokio::task::yield_now().await;
        assert!(!takeover.is_finished());
        factory.block_fork.store(false, Ordering::Release);
        factory.fork_release.notify_one();
        assert_eq!(
            first_fork.await.expect("first fork task"),
            CommandOutcome::Accepted
        );
        assert_eq!(
            takeover.await.expect("takeover task"),
            CommandOutcome::Accepted
        );
        assert_eq!(
            host.dispatch(
                second,
                ClientCommand::Fork {
                    meta: meta("spoofed", "second-fork"),
                    session_id: parent,
                    at_turn: None,
                    operation_id: Some("second-fork-operation".to_owned()),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        assert_eq!(
            factory
                .fork_turns
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn empty_log_rejects_sequence_zero_cursor_and_null_cursor_completes_promptly() {
        let (host, _factory) = host(1);
        let session = SessionId("empty-log-cursor".to_owned());
        let bound = BoundClient {
            client_id: ClientId("empty-log-observer".to_owned()),
        };
        assert_eq!(
            host.dispatch(
                bound.clone(),
                ClientCommand::ResumeSession {
                    meta: meta("spoofed", "resume-empty-log"),
                    session_id: session.clone(),
                    last_seen_sequence: None,
                    role: ClientRole::Observer,
                },
            )
            .await,
            CommandOutcome::Accepted
        );

        let mut invalid = host
            .subscribe(bound.clone(), Some(session.clone()), Some(SequenceId(0)))
            .await
            .expect("subscription channel");
        let error = tokio::time::timeout(Duration::from_millis(250), invalid.recv())
            .await
            .expect("ahead cursor must not hang")
            .expect("protocol error item")
            .expect_err("sequence zero is ahead of an empty log");
        assert!(matches!(
            error,
            HostError::Protocol(message)
                if message == "last seen sequence is ahead of the durable log"
        ));

        let mut valid = host
            .subscribe(bound, Some(session.clone()), None)
            .await
            .expect("null cursor subscription");
        let completed = tokio::time::timeout(Duration::from_millis(250), valid.recv())
            .await
            .expect("null cursor replay must complete")
            .expect("replay completion item")
            .expect("valid replay completion");
        assert!(matches!(
            &completed,
            EngineEvent::SessionReplayCompleted {
                session_id,
                through_sequence: None,
                ..
            } if session_id == &session
        ));
        let wire = serde_json::to_value(completed).expect("schema-safe replay completion");
        assert_eq!(wire["through_sequence"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn failed_resume_removes_reservation_and_retry_succeeds() {
        let (host, factory) = host(1);
        factory.fail_resume_once.store(true, Ordering::Release);
        let command = |request: &str| ClientCommand::ResumeSession {
            meta: meta("client", request),
            session_id: SessionId("retry".to_owned()),
            last_seen_sequence: None,
            role: ClientRole::Observer,
        };
        assert!(matches!(
            host.dispatch(
                BoundClient {
                    client_id: ClientId("client".to_owned())
                },
                command("first")
            )
            .await,
            CommandOutcome::Rejected { .. }
        ));
        assert_eq!(
            host.dispatch(
                BoundClient {
                    client_id: ClientId("client".to_owned())
                },
                command("second")
            )
            .await,
            CommandOutcome::Accepted
        );
        assert_eq!(factory.resumes.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn bound_identity_and_request_deduplication_fail_closed() {
        let (host, _factory) = host(2);
        let bound = BoundClient {
            client_id: ClientId("bound".to_owned()),
        };
        let command = ClientCommand::CreateSession {
            meta: meta("spoofed-driver", "create-once"),
            cwd: "/workspace".to_owned(),
            model: None,
        };
        assert_eq!(
            host.dispatch(bound.clone(), command.clone()).await,
            CommandOutcome::Accepted
        );
        assert_eq!(
            host.dispatch(bound.clone(), command).await,
            CommandOutcome::Accepted
        );
        let sessions = host.factory.persisted_sessions().await.expect("sessions");
        assert!(sessions.is_empty());
        let registry = host.registry.lock().await;
        assert_eq!(registry.sessions.len(), 1);
        let descriptor = match registry.sessions.values().next() {
            Some(SessionSlot::Ready(session)) => session.descriptor(),
            Some(SessionSlot::Opening(_)) | None => panic!("ready session"),
        };
        assert_eq!(descriptor.driver_client_id, Some(bound.client_id.clone()));
        drop(registry);

        let conflict = host
            .dispatch(
                bound,
                ClientCommand::ListModels {
                    meta: meta("spoofed-driver", "create-once"),
                },
            )
            .await;
        assert!(matches!(
            conflict,
            CommandOutcome::Rejected { error } if error.code == "request_id_conflict"
        ));
    }

    #[tokio::test]
    async fn shutdown_wakes_opening_waiters_and_never_inserts_late_resume() {
        let (host, factory) = host(2);
        factory.block_resume.store(true, Ordering::Release);
        let session_id = SessionId("shutdown-resume-race".to_owned());
        let owner = tokio::spawn({
            let host = host.clone();
            let session_id = session_id.clone();
            async move { host.resume_session(&session_id).await }
        });
        tokio::time::timeout(Duration::from_secs(1), factory.resume_started.notified())
            .await
            .expect("resume entered factory");

        let opening = {
            let registry = host.registry.lock().await;
            match registry.sessions.get(&session_id) {
                Some(SessionSlot::Opening(completed)) => completed.clone(),
                Some(SessionSlot::Ready(_)) | None => panic!("opening reservation"),
            }
        };
        let waiter = tokio::spawn({
            let host = host.clone();
            let session_id = session_id.clone();
            async move { host.resume_session(&session_id).await }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while opening.receiver_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second resume registered as an opening waiter");

        assert_eq!(
            host.dispatch(
                BoundClient {
                    client_id: ClientId("shutdown-client".to_owned()),
                },
                ClientCommand::ShutdownHost {
                    meta: meta("spoofed", "shutdown-resume"),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("opening waiter woke")
                .expect("waiter task"),
            Err(HostError::ShuttingDown)
        ));

        factory.resume_release.notify_one();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), owner)
                .await
                .expect("resume owner finished")
                .expect("owner task"),
            Err(HostError::ShuttingDown)
        ));
        assert!(host.session(&session_id).await.is_none());
        assert!(host.registry.lock().await.sessions.is_empty());
        assert_eq!(factory.shutdowns.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn shutdown_never_inserts_a_session_created_after_shutdown_started() {
        let (host, factory) = host(1);
        factory.block_create.store(true, Ordering::Release);
        let session_id = SessionId("shutdown-create-race".to_owned());
        let create = tokio::spawn({
            let host = host.clone();
            let session_id = session_id.clone();
            async move {
                host.create_session(CreateSessionRequest {
                    session_id,
                    workspace: "workspace".to_owned(),
                    model: None,
                })
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), factory.create_started.notified())
            .await
            .expect("create entered factory");
        assert_eq!(
            host.dispatch(
                BoundClient {
                    client_id: ClientId("shutdown-client".to_owned()),
                },
                ClientCommand::ShutdownHost {
                    meta: meta("spoofed", "shutdown-create"),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        factory.create_release.notify_one();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), create)
                .await
                .expect("create finished")
                .expect("create task"),
            Err(HostError::ShuttingDown)
        ));
        assert!(host.session(&session_id).await.is_none());
        let registry = host.registry.lock().await;
        assert!(registry.sessions.is_empty());
        assert_eq!(registry.anonymous_openings, 0);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn descriptors_follow_only_durable_driver_model_and_shell_state() {
        let sink = Arc::new(BlockingDescriptorSink::default());
        let factory = Arc::new(StubFactory::with_event_sink(sink.clone()));
        let host = EngineHost::new(
            EngineHostConfig {
                max_sessions: 1,
                max_deduplicated_requests: 32,
            },
            factory,
            Arc::new(StubQueries),
        )
        .expect("host");
        let session_id = SessionId("descriptor-state".to_owned());
        host.prepare_session(
            CreateSessionRequest {
                session_id: session_id.clone(),
                workspace: "workspace".to_owned(),
                model: None,
            },
            false,
        )
        .await
        .expect("prepared session");
        let driver = BoundClient {
            client_id: ClientId("driver".to_owned()),
        };
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::TakeDriver {
                    meta: meta("spoofed", "take-driver"),
                    session_id: session_id.clone(),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let session = host.session(&session_id).await.expect("ready session");
        assert_eq!(
            session.descriptor().driver_client_id,
            Some(driver.client_id.clone())
        );

        sink.block(BLOCK_MODEL);
        let switch = tokio::spawn({
            let host = host.clone();
            let driver = driver.clone();
            let session_id = session_id.clone();
            async move {
                host.dispatch(
                    driver,
                    ClientCommand::SwitchModel {
                        meta: meta("spoofed", "switch-model"),
                        session_id,
                        model: ModelAlias("big".to_owned()),
                    },
                )
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), sink.append_started.notified())
            .await
            .expect("model append blocked");
        assert_eq!(switch.await.expect("switch task"), CommandOutcome::Accepted);
        assert_eq!(session.descriptor().model, ModelAlias("fast".to_owned()));
        sink.release();
        tokio::time::timeout(Duration::from_secs(1), async {
            while session.descriptor().model != ModelAlias("big".to_owned()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("durable model projected");

        let tail = session.handle().last_sequence().await.expect("tail");
        let mut shell_events = session
            .handle()
            .subscribe_client(ClientId("shell-test".to_owned()), tail);
        sink.block(BLOCK_SHELL_ACTIVE);
        let start = tokio::spawn({
            let host = host.clone();
            let driver = driver.clone();
            let session_id = session_id.clone();
            async move {
                host.dispatch(
                    driver,
                    ClientCommand::UserShellStarted {
                        meta: meta("spoofed", "shell-start-one"),
                        session_id,
                        command: "python --version".to_owned(),
                    },
                )
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), sink.append_started.notified())
            .await
            .expect("shell-active append blocked");
        assert_eq!(start.await.expect("start task"), CommandOutcome::Accepted);
        assert!(!session.descriptor().shell_active);
        sink.release();
        let shell_id = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let EngineEvent::UserShellStateChanged {
                    shell_id,
                    active: true,
                    ..
                } = shell_events.recv().await.expect("shell event")
                {
                    break shell_id;
                }
            }
        })
        .await
        .expect("active shell event");
        tokio::time::timeout(Duration::from_secs(1), async {
            while !session.descriptor().shell_active {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("durable active shell projected");

        sink.block(BLOCK_SHELL_INACTIVE);
        let end = tokio::spawn({
            let host = host.clone();
            let driver = driver.clone();
            let session_id = session_id.clone();
            let shell_id = shell_id.clone();
            async move {
                host.dispatch(
                    driver,
                    ClientCommand::UserShellEnded {
                        meta: meta("spoofed", "shell-end-one"),
                        session_id,
                        shell_id,
                        status: 0,
                        captured_output: None,
                    },
                )
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), sink.append_started.notified())
            .await
            .expect("shell-inactive append blocked");
        assert_eq!(end.await.expect("end task"), CommandOutcome::Accepted);
        assert!(session.descriptor().shell_active);
        sink.release();
        tokio::time::timeout(Duration::from_secs(1), async {
            while session.descriptor().shell_active {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("durable inactive shell projected");

        let tail = session.handle().last_sequence().await.expect("tail");
        let mut broker_events = session
            .handle()
            .subscribe_client(ClientId("broker-test".to_owned()), tail);
        sink.block(0);
        assert_eq!(
            host.dispatch(
                driver,
                ClientCommand::UserShellStarted {
                    meta: meta("spoofed", "shell-start-two"),
                    session_id: session_id.clone(),
                    command: "python --version".to_owned(),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let broker_shell_id = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let EngineEvent::UserShellStateChanged {
                    shell_id,
                    active: true,
                    ..
                } = broker_events.recv().await.expect("broker shell event")
                {
                    break shell_id;
                }
            }
        })
        .await
        .expect("broker active shell event");
        tokio::time::timeout(Duration::from_secs(1), async {
            while !session.descriptor().shell_active {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second active shell projected");

        sink.block(BLOCK_SHELL_INACTIVE);
        let completion = tokio::spawn({
            let host = host.clone();
            let session_id = session_id.clone();
            async move {
                host.complete_user_shell(&session_id, broker_shell_id, 0, None)
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), sink.append_started.notified())
            .await
            .expect("trusted completion append blocked");
        assert!(session.descriptor().shell_active);
        assert!(!completion.is_finished());
        sink.release();
        completion
            .await
            .expect("completion task")
            .expect("trusted completion");
        assert!(!session.descriptor().shell_active);
    }
}
