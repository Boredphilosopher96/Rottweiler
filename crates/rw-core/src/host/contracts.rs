use super::{
    AgentLoopError, Arc, BuiltinProviderProfile, CachedModelCatalog, ClientId, CommandDescriptor,
    CommandOutcome, EngineEvent, Error, Future, McpApprovalReview, McpEnvironmentEntry,
    McpServerDescriptor, ModelAlias, ModelCatalogSnapshot, Pin, ProviderApiKey,
    ProviderAuthChallenge, RequestId, RuntimeServiceDescriptor, RwLock, SequenceId,
    SessionDescriptor, SessionHandle, SessionId, SubagentDescriptor, SubagentId, TranscriptFormat,
    TurnId, WorkspaceDiff, WorkspaceFileMatch, WorkspaceFilePreview, WorkspaceStatus, async_trait,
    fmt,
};

/// Transport-authenticated client identity. The host overwrites every
/// untrusted wire `CommandMeta.client_id` with this value before authorization.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BoundClient {
    pub client_id: ClientId,
}

/// Sanitized outcome of the non-protocol provider credential channel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderApiKeySubmission {
    pub stored: bool,
    pub activated: bool,
    pub warnings: Vec<String>,
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
    pub(super) descriptor: Arc<RwLock<SessionDescriptor>>,
    handle: SessionHandle,
    pub(super) lifecycle: Arc<tokio::sync::Mutex<()>>,
    model_catalog: Option<Arc<CachedModelCatalog>>,
    mcp: Option<Arc<dyn HostMcpService>>,
    runtime_services: Option<Arc<dyn HostRuntimeService>>,
    subagents: Option<Arc<dyn HostSubagentService>>,
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
            model_catalog: None,
            mcp: None,
            runtime_services: None,
            subagents: None,
        }
    }

    /// Attaches the exact provider catalog assembled for this session.
    #[must_use]
    pub fn with_model_catalog(mut self, model_catalog: Arc<CachedModelCatalog>) -> Self {
        self.model_catalog = Some(model_catalog);
        self
    }

    #[must_use]
    pub fn model_catalog(&self) -> Option<Arc<CachedModelCatalog>> {
        self.model_catalog.clone()
    }

    /// Attaches live MCP control for this exact actor session.
    #[must_use]
    pub fn with_mcp(mut self, mcp: Arc<dyn HostMcpService>) -> Self {
        self.mcp = Some(mcp);
        self
    }

    #[must_use]
    pub fn mcp(&self) -> Option<Arc<dyn HostMcpService>> {
        self.mcp.clone()
    }

    /// Attaches a credential-free view of processes currently serving this
    /// exact actor session.
    #[must_use]
    pub fn with_runtime_services(mut self, services: Arc<dyn HostRuntimeService>) -> Self {
        self.runtime_services = Some(services);
        self
    }

    #[must_use]
    pub fn runtime_services(&self) -> Option<Arc<dyn HostRuntimeService>> {
        self.runtime_services.clone()
    }

    /// Attaches parent-owned child-agent control for this exact session.
    #[must_use]
    pub fn with_subagents(mut self, subagents: Arc<dyn HostSubagentService>) -> Self {
        self.subagents = Some(subagents);
        self
    }

    #[must_use]
    pub fn subagents(&self) -> Option<Arc<dyn HostSubagentService>> {
        self.subagents.clone()
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

    pub(super) fn set_driver(&self, client_id: Option<ClientId>) {
        self.descriptor
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .driver_client_id = client_id;
    }

    pub(super) fn set_shell_active(&self, active: bool) {
        self.descriptor
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shell_active = active;
    }

    /// Starts a projection of the actor's durable state changes into the
    /// lightweight host descriptor. The subscription is created before the
    /// task is spawned, so events committed between registration and the
    /// first poll are either replayed from the sink or retained by broadcast.
    pub(super) async fn project_durable_descriptor(&self) -> Result<(), HostError> {
        let descriptor = Arc::clone(&self.descriptor);
        // The factory-provided descriptor already represents recovered state.
        // Start at the current durable tail so it is never rolled backward by
        // replaying historical state transitions. The session is not visible
        // in the host registry until this subscription has been installed.
        let tail = self.handle.last_sequence().await.map_err(HostError::from)?;
        let mut events = self
            .handle
            .subscribe_client(ClientId("host-descriptor-projector".to_owned()), tail)?;
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                {
                    let mut descriptor = descriptor
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match event {
                        EngineEvent::SessionCreated {
                            driver_client_id, ..
                        }
                        | EngineEvent::DriverChanged {
                            driver_client_id, ..
                        } => {
                            descriptor.driver_client_id = Some(driver_client_id);
                        }
                        EngineEvent::ModelChanged { model, .. } => {
                            descriptor.model = model;
                        }
                        EngineEvent::SessionTitleUpdated { title, .. } => {
                            descriptor.title = title;
                        }
                        EngineEvent::UserShellStateChanged { active, .. } => {
                            descriptor.shell_active = active;
                        }
                        _ => {}
                    }
                }
            }
        });
        Ok(())
    }
}

/// CLI composition boundary for opening durable session actors.
#[async_trait]
pub trait SessionFactory: Send + Sync + 'static {
    /// Authorizes the command's workspace and durably reserves its stable request identity.
    async fn admit_command_receipt(
        &self,
        command: &rw_types::ClientCommand,
        fingerprint: &str,
    ) -> Result<rw_types::command_receipt::ReceiptAdmission, HostError>;

    /// Durably stores completion after accepted effects settle.
    async fn complete_command_receipt(
        &self,
        operation: &RequestId,
        fingerprint: &str,
        receipt: rw_types::command_receipt::CommandReceipt,
    ) -> Result<rw_types::command_receipt::CommandReceipt, HostError>;

    /// Allocates a storage-safe id before an asynchronous create reserves
    /// capacity.
    ///
    /// # Errors
    ///
    /// Returns a sanitized factory error when an id cannot be allocated.
    fn allocate_session_id(&self) -> Result<SessionId, HostError>;

    async fn create(&self, request: CreateSessionRequest) -> Result<HostedSession, HostError>;

    async fn resume(&self, session_id: &SessionId) -> Result<HostedSession, HostError>;

    async fn fork(&self, request: ForkSessionRequest) -> Result<HostedSession, HostError>;
    async fn load_fork_operation(
        &self,
        key: &ForkOperationKey,
    ) -> Result<ForkOperationState, HostError>;
    async fn prepare_fork_operation(
        &self,
        operation: PreparedForkOperation,
    ) -> Result<PreparedForkOperation, HostError>;
    async fn complete_fork_operation(
        &self,
        key: &ForkOperationKey,
        result: &CompletedForkOperation,
    ) -> Result<CompletedForkOperation, HostError>;
    async fn abandon_prepared_fork_operation(
        &self,
        key: &ForkOperationKey,
    ) -> Result<(), HostError>;

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

    /// Settle every factory-owned operation before reporting success.
    async fn shutdown(&self) -> Result<(), HostError>;
}

/// Remote-safe host query boundary implemented by the CLI/storage layer.
#[async_trait]
pub trait HostQueryService: Send + Sync + 'static {
    async fn session_children(
        &self,
        session: &SessionId,
        scope: rw_types::session_read::SessionReadScope,
    ) -> Result<rw_types::session_children::SessionChildrenResult, HostError>;

    async fn todos(
        &self,
        session: &SessionId,
        scope: rw_types::session_read::SessionReadScope,
    ) -> Result<rw_types::todo::TodoReadResult, HostError>;

    async fn read_transcript_tail(
        &self,
        session: &SessionId,
        scope: rw_types::session_read::SessionReadScope,
        read: rw_types::transcript_tail::TranscriptTailRead,
    ) -> Result<rw_types::transcript_tail::TranscriptTailResult, HostError>;

    async fn read_transcript(
        &self,
        _session: &SessionId,
        _scope: rw_types::session_read::SessionReadScope,
        _read: rw_types::transcript::TranscriptRead,
    ) -> Result<rw_types::transcript::TranscriptReadResult, HostError> {
        Err(HostError::Query("transcript history is unavailable".into()))
    }
    async fn read_transcript_content(
        &self,
        _session: &SessionId,
        _scope: rw_types::session_read::SessionReadScope,
        _read: rw_types::transcript::TranscriptContentRead,
    ) -> Result<rw_types::transcript::TranscriptContentPage, HostError> {
        Err(HostError::Query("transcript content is unavailable".into()))
    }

    async fn command_descriptors(&self) -> Result<Vec<CommandDescriptor>, HostError>;
    async fn model_catalog(
        &self,
        refresh: bool,
        selected_model: Option<&str>,
        resolved_model: Option<&str>,
    ) -> Result<ModelCatalogSnapshot, HostError>;
    async fn user_settings(
        &self,
        _session: &SessionDescriptor,
    ) -> Result<Vec<rw_types::UserSettingDescriptor>, HostError> {
        Err(HostError::Query(
            "user settings are unavailable on this host".to_owned(),
        ))
    }
    async fn set_user_setting(
        &self,
        _session: &SessionDescriptor,
        _key: &str,
        _value: &str,
    ) -> Result<Vec<rw_types::UserSettingDescriptor>, HostError> {
        Err(HostError::Query(
            "user settings are unavailable on this host".to_owned(),
        ))
    }
    async fn persist_project_model_selection(
        &self,
        _session: &SessionDescriptor,
        _model: &ModelAlias,
    ) -> Result<(), HostError> {
        Err(HostError::Query(
            "project model persistence is unavailable on this host".to_owned(),
        ))
    }
    async fn begin_provider_auth(&self, _provider: &str) -> Result<ProviderAuthAttempt, HostError> {
        Err(HostError::Query(
            "provider authentication is unavailable on this host".to_owned(),
        ))
    }
    async fn configure_builtin_provider(
        &self,
        _profile: BuiltinProviderProfile,
    ) -> Result<(), HostError> {
        Err(HostError::Query(
            "built-in provider setup is unavailable on this host".to_owned(),
        ))
    }
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
    async fn workspace_diff(
        &self,
        session: &SessionDescriptor,
        path: &str,
        max_bytes: u32,
    ) -> Result<WorkspaceDiff, HostError>;
    async fn export_session(
        &self,
        _session: &SessionDescriptor,
        _format: TranscriptFormat,
        _output_path: &str,
        _force: bool,
    ) -> Result<String, HostError> {
        Err(HostError::Query(
            "session export is unavailable on this host".to_owned(),
        ))
    }
}

/// Session-scoped live MCP operations. Implementations own the transaction
/// between the active manager and user configuration persistence.
#[async_trait]
pub trait HostMcpService: Send + Sync + 'static {
    async fn list(&self) -> Result<Vec<McpServerDescriptor>, HostError>;
    async fn add_http(
        &self,
        name: &str,
        endpoint: &str,
    ) -> Result<Vec<McpServerDescriptor>, HostError>;
    async fn add_stdio(
        &self,
        name: &str,
        executable: &str,
        args: &[String],
        environment: &[McpEnvironmentEntry],
    ) -> Result<Vec<McpServerDescriptor>, HostError>;
    async fn remove(&self, name: &str) -> Result<Vec<McpServerDescriptor>, HostError>;
    async fn review(&self, name: &str) -> Result<McpApprovalReview, HostError>;
    async fn approve(
        &self,
        name: &str,
        fingerprint: &str,
    ) -> Result<Vec<McpServerDescriptor>, HostError>;
    async fn set_enabled(
        &self,
        name: &str,
        enabled: bool,
    ) -> Result<Vec<McpServerDescriptor>, HostError>;
}

/// Remote-safe observation boundary for session-local supporting processes.
/// Implementations must return identities only: no arguments, paths, output,
/// environment, endpoints, or credentials.
#[async_trait]
pub trait HostRuntimeService: Send + Sync + 'static {
    async fn list(&self) -> Result<Vec<RuntimeServiceDescriptor>, HostError>;
}

/// Session-scoped child-agent control. Implementations must enforce exact
/// parent ownership and must never expose a child as a generic hosted session.
#[async_trait]
pub trait HostSubagentService: Send + Sync + 'static {
    async fn family_controls(
        &self,
        root: &SessionId,
        after: Option<rw_types::SequenceId>,
    ) -> Result<rw_types::family_controls::FamilyControlsSnapshot, HostError>;
    async fn child_controls(
        &self,
        root: &SessionId,
        target: &rw_types::family_controls::ChildControlTarget,
    ) -> Result<rw_types::family_controls::ChildControlsSnapshot, HostError>;
    async fn respond_control(
        &self,
        root: &SessionId,
        target: &rw_types::family_controls::ChildControlTarget,
        authority: crate::FamilyControlAuthority,
        meta: rw_types::CommandMeta,
        revision: rw_types::SequenceId,
        response: rw_types::family_controls::ChildControlResponse,
    ) -> Result<rw_types::CommandOutcome, HostError>;

    async fn list(
        &self,
        parent_session_id: &SessionId,
    ) -> Result<Vec<SubagentDescriptor>, HostError>;

    async fn continue_child(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &SubagentId,
        content: String,
    ) -> Result<(), HostError>;

    async fn interrupt(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<(), HostError>;

    async fn close(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<(), HostError>;
}

pub(super) type ProviderAuthPersistence =
    Box<dyn FnOnce() -> Result<Vec<String>, HostError> + Send + 'static>;
pub(super) type ProviderApiKeyStore =
    dyn Fn(String, ProviderApiKey) -> Result<Vec<String>, HostError> + Send + Sync + 'static;

/// One sanitized provider-auth result. Credential values never cross this boundary.
pub struct ProviderAuthCompletion {
    pub provider: String,
    pub message: String,
    pub warnings: Vec<String>,
    persistence: Option<ProviderAuthPersistence>,
}

impl fmt::Debug for ProviderAuthCompletion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAuthCompletion")
            .field("provider", &self.provider)
            .field("message", &self.message)
            .field("warnings", &self.warnings)
            .field(
                "persistence",
                &self.persistence.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl ProviderAuthCompletion {
    /// Builds a completion that performs no credential mutation. This is used
    /// by tests and hosts whose authentication backend has no local secret.
    #[must_use]
    pub fn new(provider: String, message: String, warnings: Vec<String>) -> Self {
        Self {
            provider,
            message,
            warnings,
            persistence: None,
        }
    }

    /// Attaches an opaque, non-async persistence closure. The host invokes it
    /// in a blocking worker only after lifecycle ownership is revalidated.
    #[must_use]
    pub fn with_persistence(
        mut self,
        persistence: impl FnOnce() -> Result<Vec<String>, HostError> + Send + 'static,
    ) -> Self {
        self.persistence = Some(Box::new(persistence));
        self
    }

    pub(super) fn take_persistence(&mut self) -> Option<ProviderAuthPersistence> {
        self.persistence.take()
    }
}

/// Opaque, connection-scoped provider authentication owned by the host until
/// completion or cancellation. The inner future owns all provider secrets.
pub struct ProviderAuthAttempt {
    challenge: ProviderAuthChallenge,
    warnings: Vec<String>,
    completion:
        Pin<Box<dyn Future<Output = Result<ProviderAuthCompletion, HostError>> + Send + 'static>>,
    cancellation: Arc<dyn Fn() + Send + Sync + 'static>,
}

impl ProviderAuthAttempt {
    #[must_use]
    pub fn new(
        challenge: ProviderAuthChallenge,
        warnings: Vec<String>,
        completion: Pin<
            Box<dyn Future<Output = Result<ProviderAuthCompletion, HostError>> + Send + 'static>,
        >,
        cancellation: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Self {
        Self {
            challenge,
            warnings,
            completion,
            cancellation,
        }
    }

    pub(super) fn challenge(&self) -> &ProviderAuthChallenge {
        &self.challenge
    }

    pub(super) fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub(super) fn cancellation(&self) -> Arc<dyn Fn() + Send + Sync + 'static> {
        Arc::clone(&self.cancellation)
    }

    pub(super) async fn complete(self) -> Result<ProviderAuthCompletion, HostError> {
        self.completion.await
    }

    pub(super) fn cancel(self) {
        (self.cancellation)();
    }
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
    #[error("last seen sequence is ahead of the durable log")]
    ReplayCursorAhead,
    #[error("host persistence failure: {0}")]
    Persistence(String),
    #[error("host query failure: {0}")]
    Query(String),
    #[error("host protocol failure: {0}")]
    Protocol(String),
}

impl From<AgentLoopError> for HostError {
    fn from(value: AgentLoopError) -> Self {
        match value {
            AgentLoopError::ReplayCursorAhead => Self::ReplayCursorAhead,
            other => Self::Persistence(other.to_string()),
        }
    }
}
