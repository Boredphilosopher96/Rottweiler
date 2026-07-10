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
    RequestId, SequenceId, SessionDescriptor, SessionId, WorkspaceFileMatch, WorkspaceFilePreview,
    WorkspaceStatus,
};
use thiserror::Error;
use tokio::sync::{Notify, broadcast, mpsc};

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

/// One actor and its remote-safe host descriptor.
#[derive(Clone)]
pub struct HostedSession {
    descriptor: Arc<RwLock<SessionDescriptor>>,
    handle: SessionHandle,
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

    fn set_model(&self, model: ModelAlias) {
        self.descriptor
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .model = model;
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

    async fn persisted_sessions(&self) -> Result<Vec<SessionDescriptor>, HostError> {
        Ok(Vec::new())
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
    Opening(Arc<Notify>),
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
                            let dispatch = host.execute(operation).await;
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

    async fn execute(&self, command: ClientCommand) -> CachedDispatch {
        let meta = command.meta().clone();
        let result = self.execute_inner(command).await;
        match result {
            Ok((outcome, session_id, mut events)) => {
                events.insert(
                    0,
                    command_ack(&meta, session_id, outcome.clone(), &*self.clock),
                );
                CachedDispatch { outcome, events }
            }
            Err(error) => {
                let outcome = rejected(host_error_code(&error), &error.to_string());
                CachedDispatch {
                    events: vec![command_ack(&meta, None, outcome.clone(), &*self.clock)],
                    outcome,
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
                self.shutting_down.store(true, Ordering::Release);
                self.registry.lock().await.sessions.clear();
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
                let model = match &command {
                    ClientCommand::SwitchModel { model, .. } => Some(model.clone()),
                    _ => None,
                };
                let outcome = session.handle().dispatch(command).await?;
                if outcome == CommandOutcome::Accepted
                    && let Some(model) = model
                {
                    session.set_model(model);
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
        let created = self.factory.create(request.clone()).await;
        let mut registry = self.registry.lock().await;
        registry.anonymous_openings = registry.anonymous_openings.saturating_sub(1);
        let session = Arc::new(created?);
        if session.descriptor().session_id != request.session_id
            || session.handle().session_id() != &request.session_id
        {
            return Err(HostError::SessionIdentityMismatch);
        }
        registry
            .sessions
            .insert(request.session_id, SessionSlot::Ready(Arc::clone(&session)));
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
                    Some(SessionSlot::Opening(notify)) => Some(Arc::clone(notify).notified_owned()),
                    None => {
                        if registry
                            .sessions
                            .len()
                            .saturating_add(registry.anonymous_openings)
                            >= self.config.max_sessions
                        {
                            return Err(HostError::SessionCapacity);
                        }
                        let notify = Arc::new(Notify::new());
                        registry.sessions.insert(
                            session_id.clone(),
                            SessionSlot::Opening(Arc::clone(&notify)),
                        );
                        None
                    }
                }
            };
            if let Some(wait) = wait {
                wait.await;
                continue;
            }

            let opened = self.factory.resume(session_id).await;
            let mut registry = self.registry.lock().await;
            let notify = match registry.sessions.remove(session_id) {
                Some(SessionSlot::Opening(notify)) => Some(notify),
                Some(SessionSlot::Ready(session)) => {
                    registry
                        .sessions
                        .insert(session_id.clone(), SessionSlot::Ready(session));
                    None
                }
                None => None,
            };
            let result = match opened {
                Ok(session)
                    if session.descriptor().session_id == *session_id
                        && session.handle().session_id() == session_id =>
                {
                    let session = Arc::new(session);
                    registry
                        .sessions
                        .insert(session_id.clone(), SessionSlot::Ready(Arc::clone(&session)));
                    Ok(session)
                }
                Ok(_) => Err(HostError::SessionIdentityMismatch),
                Err(error) => Err(error),
            };
            drop(registry);
            if let Some(notify) = notify {
                notify.notify_waiters();
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
                if last_seen
                    .zip(captured_tail)
                    .is_some_and(|(seen, tail)| seen > tail)
                {
                    let _ = send
                        .send(Err(HostError::Protocol(
                            "last seen sequence is ahead of the durable tail".to_owned(),
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
        | ClientCommand::SearchWorkspaceFiles { session_id, .. }
        | ClientCommand::PreviewWorkspaceFile { session_id, .. }
        | ClientCommand::GetWorkspaceStatus { session_id, .. } => Some(session_id.clone()),
        ClientCommand::CreateSession { .. }
        | ClientCommand::ListSessions { .. }
        | ClientCommand::ListCommands { .. }
        | ClientCommand::ListModels { .. }
        | ClientCommand::ShutdownHost { .. } => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use futures_util::stream;
    use rw_types::{
        AttachmentData, CommandMeta, ModelCacheBehavior, ModelCapabilities, PROTOCOL_VERSION,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::{
        ModelDriver, NoopMutationCheckpointCoordinator, NoopSecretRedactor, NoopSessionEventSink,
        PermissionGate, SessionActor, SessionActorConfig, SessionRecoveredState,
        builtin_command_registry, builtin_hook_dispatcher,
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
    }

    impl StubFactory {
        fn new() -> Self {
            Self {
                root: TempDir::new().expect("host test root"),
                next: AtomicUsize::new(1),
                resumes: AtomicUsize::new(0),
                fail_resume_once: AtomicBool::new(false),
            }
        }

        fn session(&self, session_id: &SessionId) -> HostedSession {
            let workspace = self.root.path().join(&session_id.0);
            std::fs::create_dir_all(&workspace).expect("session workspace");
            let handle = SessionActor::spawn(SessionActorConfig {
                session_id: session_id.clone(),
                workspace_root: workspace,
                initial_session_context: Vec::new(),
                model_alias: "fast".to_owned(),
                model: Arc::new(IdleModel),
                tools: Arc::new(ToolRegistry::new()),
                permissions: Arc::new(PermissionGate::new(PermissionDecision::Allow)),
                hooks: Arc::new(builtin_hook_dispatcher().expect("hooks")),
                commands: Arc::new(builtin_command_registry().expect("commands")),
                event_sink: Arc::new(NoopSessionEventSink::default()),
                event_clock: Arc::new(SystemEventClock),
                secret_redactor: Arc::new(NoopSecretRedactor),
                checkpoints: Arc::new(NoopMutationCheckpointCoordinator),
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
            Ok(self.session(&request.session_id))
        }

        async fn resume(&self, session_id: &SessionId) -> Result<HostedSession, HostError> {
            self.resumes.fetch_add(1, Ordering::Relaxed);
            tokio::task::yield_now().await;
            if self.fail_resume_once.swap(false, Ordering::AcqRel) {
                return Err(HostError::Persistence("injected resume failure".to_owned()));
            }
            Ok(self.session(session_id))
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
}
