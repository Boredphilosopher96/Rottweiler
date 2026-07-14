use std::{
    collections::{HashMap, VecDeque},
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use rw_types::{
    ClientCommand, ClientId, ClientRole, CommandAckMeta, CommandDescriptor, CommandMeta,
    CommandOutcome, EngineError, EngineErrorCategory, EngineEvent, McpApprovalReview,
    McpServerDescriptor, ModelAlias, ModelCatalogSnapshot, ProviderAuthAttemptId,
    ProviderAuthChallenge, RequestId, RuntimeServiceDescriptor, SequenceId, SessionDescriptor,
    SessionId, ShellId, SubagentDescriptor, SubagentId, SubagentReplayItem, TurnId, WorkspaceDiff,
    WorkspaceFileMatch, WorkspaceFilePreview, WorkspaceStatus,
};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Notify, broadcast, mpsc, watch};

use crate::{
    AgentLoopError, CachedModelCatalog, EventClock, ProviderApiKey, SessionHandle,
    SystemEventClock, store_provider_api_key,
};

const HOST_EVENT_CAPACITY: usize = 256;
const SUBAGENT_REPLAY_BATCH_EVENTS: usize = 128;
const SUBAGENT_REPLAY_BATCH_BYTES: usize = 128 * 1024;
const MAX_WIRE_COMMANDS: usize = 512;
const MAX_WIRE_COMMAND_CATALOG_BYTES: usize = 48 * 1024;
const PROVIDER_AUTH_BEGIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
const PROVIDER_AUTH_COMPLETE_DEADLINE: std::time::Duration = std::time::Duration::from_mins(10);
const MAX_PROVIDER_AUTH_URL_BYTES: usize = 4_096;
const MAX_PROVIDER_AUTH_CODE_BYTES: usize = 256;
const MAX_PROVIDER_AUTH_WARNINGS: usize = 16;
const MAX_PROVIDER_AUTH_WARNING_BYTES: usize = 512;
const PROVIDER_AUTH_WARNINGS_OMITTED: &str =
    "provider credential was saved; some credential warnings were omitted";
const MAX_PROVIDER_AUTH_MESSAGE_BYTES: usize = 1_024;

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
    descriptor: Arc<RwLock<SessionDescriptor>>,
    handle: SessionHandle,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
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
    async fn configure_builtin_provider(&self, _provider: &str) -> Result<(), HostError> {
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

/// One bounded, validated child-log replay owned by a parent session.
#[derive(Clone, Debug, PartialEq)]
pub struct SubagentReplay {
    pub child_session_id: SessionId,
    pub events: Vec<(SequenceId, Value)>,
    pub through_sequence: Option<SequenceId>,
    pub next_cursor: Option<SequenceId>,
    pub tail_sequence: Option<SequenceId>,
    pub has_more: bool,
    pub events_before_page: u64,
    pub truncated: bool,
}

/// Session-scoped child-agent control. Implementations must enforce exact
/// parent ownership and must never expose a child as a generic hosted session.
#[async_trait]
pub trait HostSubagentService: Send + Sync + 'static {
    async fn list(
        &self,
        parent_session_id: &SessionId,
    ) -> Result<Vec<SubagentDescriptor>, HostError>;

    async fn replay(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &SubagentId,
        after_sequence: Option<SequenceId>,
    ) -> Result<SubagentReplay, HostError>;

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

type ProviderAuthPersistence = Box<dyn FnOnce() -> Result<Vec<String>, HostError> + Send + 'static>;
type ProviderApiKeyStore =
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

    fn take_persistence(&mut self) -> Option<ProviderAuthPersistence> {
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

    fn challenge(&self) -> &ProviderAuthChallenge {
        &self.challenge
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn cancellation(&self) -> Arc<dyn Fn() + Send + Sync + 'static> {
        Arc::clone(&self.cancellation)
    }

    async fn complete(self) -> Result<ProviderAuthCompletion, HostError> {
        self.completion.await
    }

    fn cancel(self) {
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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ProviderAuthOwner {
    client_id: ClientId,
    session_id: SessionId,
    provider: String,
}

enum PendingProviderAuth {
    Opening {
        attempt_id: ProviderAuthAttemptId,
    },
    Ready {
        attempt_id: ProviderAuthAttemptId,
        attempt: ProviderAuthAttempt,
    },
    Completing {
        attempt_id: ProviderAuthAttemptId,
        cancellation: Arc<dyn Fn() + Send + Sync + 'static>,
        cancelled: watch::Sender<bool>,
    },
    Finalizing {
        attempt_id: ProviderAuthAttemptId,
    },
}

#[derive(Default)]
struct PendingProviderAuths {
    entries: Mutex<HashMap<ProviderAuthOwner, PendingProviderAuth>>,
}

struct ProviderAuthOpeningGuard {
    pending: Arc<PendingProviderAuths>,
    owner: ProviderAuthOwner,
    attempt_id: ProviderAuthAttemptId,
    armed: bool,
}

struct ProviderAuthCompletionGuard {
    pending: Arc<PendingProviderAuths>,
    owner: ProviderAuthOwner,
    attempt_id: ProviderAuthAttemptId,
}

impl Drop for ProviderAuthCompletionGuard {
    fn drop(&mut self) {
        remove_provider_auth_reservation(&self.pending, &self.owner, &self.attempt_id);
    }
}

impl ProviderAuthOpeningGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProviderAuthOpeningGuard {
    fn drop(&mut self) {
        if self.armed {
            remove_provider_auth_reservation(&self.pending, &self.owner, &self.attempt_id);
        }
    }
}

struct ProviderAuthSubscriptionGuard {
    client_id: ClientId,
    receiver: broadcast::Receiver<EngineEvent>,
    sender: broadcast::Sender<EngineEvent>,
    pending: Arc<PendingProviderAuths>,
}

impl Drop for ProviderAuthSubscriptionGuard {
    fn drop(&mut self) {
        // This receiver is still counted during Drop. Cancel only when it is
        // the client's final authenticated event subscription.
        if self.sender.receiver_count() <= 1 {
            self.pending.cancel_client(&self.client_id);
        }
    }
}

impl PendingProviderAuths {
    fn cancel_session_client(&self, client_id: &ClientId, session_id: &SessionId) {
        let attempts = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let owners = entries
                .keys()
                .filter(|owner| {
                    &owner.client_id == client_id
                        && &owner.session_id == session_id
                        && entries.get(*owner).is_some_and(provider_auth_can_cancel)
                })
                .cloned()
                .collect::<Vec<_>>();
            owners
                .into_iter()
                .filter_map(|owner| entries.remove(&owner))
                .collect::<Vec<_>>()
        };
        cancel_provider_auth_attempts(attempts);
    }

    fn cancel_client(&self, client_id: &ClientId) {
        let attempts = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let owners = entries
                .keys()
                .filter(|owner| {
                    &owner.client_id == client_id
                        && entries.get(*owner).is_some_and(provider_auth_can_cancel)
                })
                .cloned()
                .collect::<Vec<_>>();
            owners
                .into_iter()
                .filter_map(|owner| entries.remove(&owner))
                .collect::<Vec<_>>()
        };
        cancel_provider_auth_attempts(attempts);
    }

    fn cancel_all(&self) {
        let attempts = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries
                .drain()
                .map(|(_, pending)| pending)
                .collect::<Vec<_>>()
        };
        cancel_provider_auth_attempts(attempts);
    }
}

fn cancel_provider_auth_attempts(attempts: Vec<PendingProviderAuth>) {
    for pending in attempts {
        match pending {
            PendingProviderAuth::Ready { attempt, .. } => attempt.cancel(),
            PendingProviderAuth::Completing {
                cancellation,
                cancelled,
                ..
            } => {
                cancellation();
                let _ = cancelled.send(true);
            }
            PendingProviderAuth::Opening { .. } | PendingProviderAuth::Finalizing { .. } => {}
        }
    }
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
    provider_auth: Arc<PendingProviderAuths>,
    provider_mutation: Arc<tokio::sync::Mutex<()>>,
    provider_api_key_store: Arc<ProviderApiKeyStore>,
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
            provider_auth: Arc::new(PendingProviderAuths::default()),
            provider_mutation: Arc::new(tokio::sync::Mutex::new(())),
            provider_api_key_store: Arc::new(|provider, api_key| {
                store_provider_api_key(&provider, api_key)
                    .map_err(|_| HostError::Query("provider credential storage failed".to_owned()))
            }),
            shutting_down: Arc::new(AtomicBool::new(false)),
        })
    }

    #[cfg(test)]
    fn with_provider_api_key_store(mut self, store: Arc<ProviderApiKeyStore>) -> Self {
        self.provider_api_key_store = store;
        self
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

    /// Accepts an API key from the transport's separate, non-replayable secret
    /// channel. The authenticated client must own the session driver lease;
    /// key material is consumed directly by the credential store and never
    /// enters a command, event, snapshot, or diagnostic value.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for an invalid provider/session, a client
    /// without the driver lease, credential-storage failure, or activation
    /// failure.
    pub async fn submit_provider_api_key(
        &self,
        bound: BoundClient,
        session_id: &SessionId,
        provider: &str,
        api_key: ProviderApiKey,
    ) -> Result<ProviderApiKeySubmission, HostError> {
        validate_provider_auth_name(provider)?;
        let session = self.ready_session(session_id).await?;
        let provider = provider.to_owned();
        let provider_mutation = Arc::clone(&self.provider_mutation);
        let provider_api_key_store = Arc::clone(&self.provider_api_key_store);
        // The host-owned task shields the irreversible vault write. Dropping
        // the HTTP request cannot drop the lifecycle guard while its blocking
        // writer continues detached; takeover waits through activation.
        tokio::spawn(async move {
            let provider_mutation_guard = provider_mutation.lock_owned().await;
            let lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
            let snapshot = session.handle().snapshot().await?;
            if snapshot.driver_client_id.as_ref() != Some(&bound.client_id) {
                return Err(HostError::Protocol(
                    "only the current driver may store provider credentials".to_owned(),
                ));
            }
            let provider_for_store = provider.clone();
            let warnings = tokio::task::spawn_blocking(move || {
                provider_api_key_store(provider_for_store, api_key)
            })
            .await
            .map_err(|_| HostError::Query("provider credential storage failed".to_owned()))??;
            let warnings = bounded_provider_auth_warnings(&warnings)?;
            let connection_ready = session
                .handle()
                .activate_provider(&provider, Some(&snapshot.model_alias))
                .await
                .is_ok();
            drop(lifecycle_guard);
            drop(provider_mutation_guard);
            let catalog_ready = match session.model_catalog() {
                Some(catalog) => catalog
                    .refresh_provider(&provider)
                    .await
                    .is_ok_and(|catalog| provider_catalog_is_ready(&catalog, &provider)),
                None => false,
            };
            Ok(ProviderApiKeySubmission {
                stored: true,
                activated: connection_ready && catalog_ready,
                warnings,
            })
        })
        .await
        .map_err(|_| HostError::Query("provider credential task failed".to_owned()))?
    }

    /// Retries activation for an already-stored provider credential without
    /// asking the client to submit the secret again.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for an invalid provider/session, a client
    /// without the driver lease, or provider activation failure.
    pub async fn activate_provider_for_client(
        &self,
        bound: BoundClient,
        session_id: &SessionId,
        provider: &str,
    ) -> Result<(), HostError> {
        validate_provider_auth_name(provider)?;
        let session = self.ready_session(session_id).await?;
        let provider_mutation_guard = Arc::clone(&self.provider_mutation).lock_owned().await;
        let lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
        let snapshot = session.handle().snapshot().await?;
        if snapshot.driver_client_id.as_ref() != Some(&bound.client_id) {
            return Err(HostError::Protocol(
                "only the current driver may activate providers".to_owned(),
            ));
        }
        session
            .handle()
            .activate_provider(provider, Some(&snapshot.model_alias))
            .await
            .map_err(|_| HostError::Query("provider activation failed".to_owned()))?;
        drop(lifecycle_guard);
        drop(provider_mutation_guard);
        let catalog = session
            .model_catalog()
            .ok_or_else(|| HostError::Query("provider model catalog is unavailable".to_owned()))?;
        let catalog = catalog
            .refresh_provider(provider)
            .await
            .map_err(|_| HostError::Query("provider model catalog refresh failed".to_owned()))?;
        if !provider_catalog_is_ready(&catalog, provider) {
            return Err(HostError::Query(
                "provider is not reachable or returned no models".to_owned(),
            ));
        }
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
        self.prepare_session_after_reservation(request, resume, || {})
            .await
    }

    /// Opens the supervisor-selected initial session and invokes `on_reserved`
    /// after its exact identity is present in the host registry but before
    /// potentially blocking factory work begins. Supervisors use this boundary
    /// to publish authenticated readiness without allowing an initial client
    /// resume to race a second open of the same durable session.
    ///
    /// # Errors
    ///
    /// Returns a typed host error when capacity, persistence, recovery, or
    /// session identity validation fails.
    pub async fn prepare_session_after_reservation<F>(
        &self,
        request: CreateSessionRequest,
        resume: bool,
        on_reserved: F,
    ) -> Result<SessionDescriptor, HostError>
    where
        F: FnOnce(),
    {
        let session = if resume {
            self.resume_session_after_reservation(&request.session_id, Some(on_reserved))
                .await?
        } else {
            self.prepare_fresh_session_after_reservation(request, Some(on_reserved))
                .await?
        };
        Ok(session.descriptor())
    }

    /// Reserves the supervisor-selected fresh identity before asynchronous
    /// factory work begins. Clients that connect after authenticated health is
    /// ready but before provider composition finishes join the same opening.
    async fn prepare_fresh_session_after_reservation<F>(
        &self,
        request: CreateSessionRequest,
        mut on_reserved: Option<F>,
    ) -> Result<Arc<HostedSession>, HostError>
    where
        F: FnOnce(),
    {
        loop {
            let (ready, wait, owns_opening) = {
                let mut registry = self.registry.lock().await;
                if self.shutting_down.load(Ordering::Acquire) {
                    return Err(HostError::ShuttingDown);
                }
                match registry.sessions.get(&request.session_id) {
                    Some(SessionSlot::Ready(session)) => (Some(Arc::clone(session)), None, false),
                    Some(SessionSlot::Opening(completed)) => {
                        (None, Some(completed.subscribe()), false)
                    }
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
                            .insert(request.session_id.clone(), SessionSlot::Opening(completed));
                        (None, None, true)
                    }
                }
            };
            if let Some(on_reserved) = on_reserved.take() {
                on_reserved();
            }
            if let Some(session) = ready {
                return Ok(session);
            }
            if let Some(mut completed) = wait {
                if !*completed.borrow_and_update() {
                    let _ = completed.changed().await;
                }
                continue;
            }
            if owns_opening {
                break;
            }
        }

        let created = match self.factory.create(request.clone()).await {
            Ok(session)
                if session.descriptor().session_id == request.session_id
                    && session.handle().session_id() == &request.session_id =>
            {
                let session = Arc::new(session);
                session.project_durable_descriptor().await.map(|()| session)
            }
            Ok(_) => Err(HostError::SessionIdentityMismatch),
            Err(error) => Err(error),
        };
        let mut registry = self.registry.lock().await;
        let completed = match registry.sessions.remove(&request.session_id) {
            Some(SessionSlot::Opening(completed)) => Some(completed),
            Some(SessionSlot::Ready(session)) => {
                registry
                    .sessions
                    .insert(request.session_id.clone(), SessionSlot::Ready(session));
                None
            }
            None => None,
        };
        let result = if self.shutting_down.load(Ordering::Acquire) {
            Err(HostError::ShuttingDown)
        } else {
            match created {
                Ok(session) => {
                    registry
                        .sessions
                        .insert(request.session_id, SessionSlot::Ready(Arc::clone(&session)));
                    Ok(session)
                }
                Err(error) => Err(error),
            }
        };
        drop(registry);
        if let Some(completed) = completed {
            completed.send_replace(true);
        }
        result
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
                    if snapshot.running
                        || snapshot.active_shell.is_some()
                        || snapshot.active_background
                    {
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
                            || verified.active_background
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
            ClientCommand::ListCommands { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let descriptors = session.handle().command_descriptors();
                let (commands, truncated) = wire_command_catalog(descriptors.iter().cloned());
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::CommandDescriptorsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        commands,
                        truncated,
                    }],
                ))
            }
            ClientCommand::ListModels {
                meta,
                session_id,
                refresh,
            } => {
                let (session_catalog, selected, resolved) = if let Some(session_id) = &session_id {
                    let session = self.ready_session(session_id).await?;
                    let snapshot = session.handle().snapshot().await.map_err(HostError::from)?;
                    let resolved = snapshot.conversation.iter().rev().find_map(|turn| {
                        turn.meta
                            .model
                            .as_ref()
                            .filter(|model| model.contains('/'))
                            .cloned()
                    });
                    (
                        session.model_catalog(),
                        Some(snapshot.model_alias),
                        resolved,
                    )
                } else {
                    (None, None, None)
                };
                let mut catalog = if let Some(session_catalog) = session_catalog {
                    session_catalog
                        .get(refresh)
                        .await
                        .map_err(|error| HostError::Query(error.to_string()))?
                } else {
                    self.queries.model_catalog(refresh, None, None).await?
                };
                overlay_model_catalog_current(
                    &mut catalog,
                    selected.as_deref(),
                    resolved.as_deref(),
                );
                Ok((
                    CommandOutcome::Accepted,
                    None,
                    vec![EngineEvent::ModelsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        models: catalog.models,
                        aliases: catalog.aliases,
                        providers: catalog.providers,
                        cached: catalog.cached,
                        truncated: catalog.truncated,
                    }],
                ))
            }
            ClientCommand::ListSettings { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let settings = self.queries.user_settings(&session.descriptor()).await?;
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::SettingsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        settings,
                    }],
                ))
            }
            ClientCommand::SetSetting {
                meta,
                session_id,
                key,
                value,
            } => {
                let session = self.ready_session(&session_id).await?;
                let queries = Arc::clone(&self.queries);
                let actor = meta.client_id.clone();
                let settings = tokio::spawn(async move {
                    let _lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                    let snapshot = session.handle().snapshot().await?;
                    if snapshot.driver_client_id.as_ref() != Some(&actor) {
                        return Err(HostError::Protocol(
                            "only the current driver may persist user settings".to_owned(),
                        ));
                    }
                    queries
                        .set_user_setting(&session.descriptor(), &key, &value)
                        .await
                })
                .await
                .map_err(|_| HostError::Query("user setting task failed".to_owned()))??;
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::SettingsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        settings,
                    }],
                ))
            }
            ClientCommand::ListMcpServers { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let mcp = session.mcp().ok_or_else(|| {
                    HostError::Query(
                        "live MCP management is unavailable for this session".to_owned(),
                    )
                })?;
                let servers = mcp.list().await?;
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::McpServersListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        servers,
                    }],
                ))
            }
            ClientCommand::ListRuntimeServices { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let services = match session.runtime_services() {
                    Some(services) => services.list().await?,
                    None => Vec::new(),
                };
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::RuntimeServicesListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        services,
                    }],
                ))
            }
            ClientCommand::AddMcpHttpServer {
                meta,
                session_id,
                name,
                endpoint,
            } => {
                let session = self.ready_session(&session_id).await?;
                let mcp = session.mcp().ok_or_else(|| {
                    HostError::Query(
                        "live MCP management is unavailable for this session".to_owned(),
                    )
                })?;
                let actor = meta.client_id.clone();
                let servers = tokio::spawn(async move {
                    let _lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                    let snapshot = session.handle().snapshot().await?;
                    if snapshot.driver_client_id.as_ref() != Some(&actor) {
                        return Err(HostError::Protocol(
                            "only the current driver may add MCP servers".to_owned(),
                        ));
                    }
                    mcp.add_http(&name, &endpoint).await
                })
                .await
                .map_err(|_| HostError::Query("MCP add task failed".to_owned()))??;
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::McpServersListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        servers,
                    }],
                ))
            }
            ClientCommand::ReviewMcpServer {
                meta,
                session_id,
                name,
            } => {
                let session = self.ready_session(&session_id).await?;
                let snapshot = session.handle().snapshot().await?;
                if snapshot.driver_client_id.as_ref() != Some(&meta.client_id) {
                    return Err(HostError::Protocol(
                        "only the current driver may review MCP configuration".to_owned(),
                    ));
                }
                let review = session
                    .mcp()
                    .ok_or_else(|| {
                        HostError::Query(
                            "live MCP management is unavailable for this session".to_owned(),
                        )
                    })?
                    .review(&name)
                    .await?;
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::McpServerApprovalReviewed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        review,
                    }],
                ))
            }
            ClientCommand::ApproveMcpServer {
                meta,
                session_id,
                name,
                fingerprint,
            } => {
                let session = self.ready_session(&session_id).await?;
                let mcp = session.mcp().ok_or_else(|| {
                    HostError::Query(
                        "live MCP management is unavailable for this session".to_owned(),
                    )
                })?;
                let actor = meta.client_id.clone();
                let servers = tokio::spawn(async move {
                    let _lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                    let snapshot = session.handle().snapshot().await?;
                    if snapshot.driver_client_id.as_ref() != Some(&actor) {
                        return Err(HostError::Protocol(
                            "only the current driver may approve MCP servers".to_owned(),
                        ));
                    }
                    mcp.approve(&name, &fingerprint).await
                })
                .await
                .map_err(|_| HostError::Query("MCP approval task failed".to_owned()))??;
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::McpServersListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        servers,
                    }],
                ))
            }
            ClientCommand::SetMcpServerEnabled {
                meta,
                session_id,
                name,
                enabled,
            } => {
                let session = self.ready_session(&session_id).await?;
                let mcp = session.mcp().ok_or_else(|| {
                    HostError::Query(
                        "live MCP management is unavailable for this session".to_owned(),
                    )
                })?;
                let actor = meta.client_id.clone();
                let servers = tokio::spawn(async move {
                    let _lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                    let snapshot = session.handle().snapshot().await?;
                    if snapshot.driver_client_id.as_ref() != Some(&actor) {
                        return Err(HostError::Protocol(
                            "only the current driver may enable or disable MCP servers".to_owned(),
                        ));
                    }
                    mcp.set_enabled(&name, enabled).await
                })
                .await
                .map_err(|_| HostError::Query("MCP enablement task failed".to_owned()))??;
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::McpServersListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        servers,
                    }],
                ))
            }
            ClientCommand::BeginProviderAuth {
                meta,
                session_id,
                provider,
            } => {
                validate_provider_auth_name(&provider)?;
                let session = self.ready_session(&session_id).await?;
                let lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                let snapshot = session.handle().snapshot().await?;
                if snapshot.driver_client_id.as_ref() != Some(&meta.client_id) {
                    return Err(HostError::Protocol(
                        "only the current driver may authenticate providers".to_owned(),
                    ));
                }
                let owner = ProviderAuthOwner {
                    client_id: meta.client_id.clone(),
                    session_id: session_id.clone(),
                    provider: provider.clone(),
                };
                let attempt_id = provider_auth_attempt_id(&meta, &session_id, &provider);
                {
                    let mut pending = self
                        .provider_auth
                        .entries
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if pending.keys().any(|active| active.provider == provider) {
                        return Err(HostError::Protocol(
                            "provider authentication is already in progress".to_owned(),
                        ));
                    }
                    pending.insert(
                        owner.clone(),
                        PendingProviderAuth::Opening {
                            attempt_id: attempt_id.clone(),
                        },
                    );
                }
                let mut opening_guard = ProviderAuthOpeningGuard {
                    pending: Arc::clone(&self.provider_auth),
                    owner: owner.clone(),
                    attempt_id: attempt_id.clone(),
                    armed: true,
                };
                drop(lifecycle_guard);
                let attempt = match tokio::time::timeout(
                    PROVIDER_AUTH_BEGIN_DEADLINE,
                    self.queries.begin_provider_auth(&provider),
                )
                .await
                {
                    Ok(Ok(attempt)) => attempt,
                    Ok(Err(error)) => {
                        remove_provider_auth_reservation(&self.provider_auth, &owner, &attempt_id);
                        return Err(HostError::Query(sanitized_provider_auth_error(&error)));
                    }
                    Err(_) => {
                        remove_provider_auth_reservation(&self.provider_auth, &owner, &attempt_id);
                        return Err(HostError::Query(
                            "provider authentication setup deadline exceeded".to_owned(),
                        ));
                    }
                };
                let (challenge, warnings) = bounded_provider_auth_prompt(&attempt)?;
                let lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                let driver_unchanged = session.handle().snapshot().await?.driver_client_id.as_ref()
                    == Some(&meta.client_id);
                let mut attempt = Some(attempt);
                let retained = {
                    let mut pending = self
                        .provider_auth
                        .entries
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if driver_unchanged
                        && matches!(
                            pending.get(&owner),
                            Some(PendingProviderAuth::Opening { attempt_id: current }) if current == &attempt_id
                        )
                    {
                        if let Some(retained_attempt) = attempt.take() {
                            pending.insert(
                                owner,
                                PendingProviderAuth::Ready {
                                    attempt_id: attempt_id.clone(),
                                    attempt: retained_attempt,
                                },
                            );
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };
                drop(lifecycle_guard);
                if !retained {
                    if let Some(attempt) = attempt {
                        attempt.cancel();
                    }
                    return Err(HostError::Protocol(
                        "provider authentication was cancelled during setup".to_owned(),
                    ));
                }
                opening_guard.disarm();
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::ProviderAuthStarted {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        attempt_id,
                        provider,
                        challenge,
                        warnings,
                    }],
                ))
            }
            ClientCommand::ConfigureBuiltinProvider {
                meta,
                session_id,
                provider,
            } => {
                validate_provider_auth_name(&provider)?;
                let session = self.ready_session(&session_id).await?;
                let auth_kind = builtin_provider_auth_kind(&provider)?;
                let queries = Arc::clone(&self.queries);
                let provider_mutation = Arc::clone(&self.provider_mutation);
                let actor = meta.client_id.clone();
                let provider_for_task = provider.clone();
                tokio::spawn(async move {
                    let _provider_mutation = provider_mutation.lock_owned().await;
                    let _lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                    let snapshot = session.handle().snapshot().await?;
                    if snapshot.driver_client_id.as_ref() != Some(&actor) {
                        return Err(HostError::Protocol(
                            "only the current driver may configure built-in providers".to_owned(),
                        ));
                    }
                    queries.configure_builtin_provider(&provider_for_task).await
                })
                .await
                .map_err(|_| HostError::Query("provider configuration task failed".to_owned()))??;
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::ProviderConfigured {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        provider,
                        auth_kind,
                    }],
                ))
            }
            ClientCommand::CompleteProviderAuth {
                meta,
                session_id,
                provider,
                attempt_id,
            } => {
                validate_provider_auth_name(&provider)?;
                let session = self.ready_session(&session_id).await?;
                let lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                let snapshot = session.handle().snapshot().await?;
                if snapshot.driver_client_id.as_ref() != Some(&meta.client_id) {
                    return Err(HostError::Protocol(
                        "only the current driver may complete provider authentication".to_owned(),
                    ));
                }
                let owner = ProviderAuthOwner {
                    client_id: meta.client_id.clone(),
                    session_id: session_id.clone(),
                    provider: provider.clone(),
                };
                let pending = {
                    let mut entries = self
                        .provider_auth
                        .entries
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    entries.remove(&owner)
                };
                let attempt = match pending {
                    Some(PendingProviderAuth::Ready {
                        attempt_id: current,
                        attempt,
                    }) if current == attempt_id => attempt,
                    Some(pending @ PendingProviderAuth::Completing { .. })
                        if pending_provider_auth_id(&pending) == &attempt_id =>
                    {
                        self.provider_auth
                            .entries
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(owner, pending);
                        // ProviderAuthStarted is durable and can be replayed after a
                        // transport reconnect. Treat the corresponding completion
                        // command as an idempotent subscription to the already-running
                        // poll/callback instead of turning a healthy login into a
                        // protocol error.
                        return Ok((CommandOutcome::Accepted, Some(session_id), Vec::new()));
                    }
                    Some(other) => {
                        self.provider_auth
                            .entries
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(owner, other);
                        return Err(HostError::Protocol(
                            "provider authentication attempt is not ready or does not match"
                                .to_owned(),
                        ));
                    }
                    None => {
                        return Err(HostError::Protocol(
                            "provider authentication attempt is no longer active".to_owned(),
                        ));
                    }
                };
                let cancellation = attempt.cancellation();
                let (cancelled, cancel_signal) = watch::channel(false);
                self.provider_auth
                    .entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        owner.clone(),
                        PendingProviderAuth::Completing {
                            attempt_id: attempt_id.clone(),
                            cancellation: Arc::clone(&cancellation),
                            cancelled,
                        },
                    );
                drop(lifecycle_guard);
                let host = self.clone();
                tokio::spawn(async move {
                    host.complete_provider_auth_task(
                        owner,
                        attempt_id,
                        attempt,
                        cancel_signal,
                        meta,
                    )
                    .await;
                });
                Ok((CommandOutcome::Accepted, Some(session_id), Vec::new()))
            }
            ClientCommand::CancelProviderAuth {
                meta,
                session_id,
                provider,
                attempt_id,
            } => {
                validate_provider_auth_name(&provider)?;
                let session = self.ready_session(&session_id).await?;
                let _lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                let snapshot = session.handle().snapshot().await?;
                if snapshot.driver_client_id.as_ref() != Some(&meta.client_id) {
                    return Err(HostError::Protocol(
                        "only the current driver may cancel provider authentication".to_owned(),
                    ));
                }
                let owner = ProviderAuthOwner {
                    client_id: meta.client_id.clone(),
                    session_id: session_id.clone(),
                    provider: provider.clone(),
                };
                let pending = {
                    let mut entries = self
                        .provider_auth
                        .entries
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let pending = entries.remove(&owner);
                    match pending {
                        Some(pending) if pending_provider_auth_id(&pending) == &attempt_id => {
                            pending
                        }
                        Some(pending) => {
                            entries.insert(owner, pending);
                            return Err(HostError::Protocol(
                                "provider authentication attempt does not match".to_owned(),
                            ));
                        }
                        None => {
                            return Err(HostError::Protocol(
                                "provider authentication attempt is no longer active".to_owned(),
                            ));
                        }
                    }
                };
                cancel_provider_auth_attempts(vec![pending]);
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::ProviderAuthFinished {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        attempt_id,
                        provider,
                        success: false,
                        message: "provider authentication cancelled".to_owned(),
                        warnings: Vec::new(),
                    }],
                ))
            }
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
            ClientCommand::GetWorkspaceDiff {
                meta,
                session_id,
                path,
                max_bytes,
            } => {
                let session = self.ready_session(&session_id).await?;
                let diff = self
                    .queries
                    .workspace_diff(&session.descriptor(), &path, max_bytes)
                    .await?;
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::WorkspaceDiffReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        diff,
                    }],
                ))
            }
            ClientCommand::ListSubagents { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let _lifecycle = Arc::clone(&session.lifecycle).lock_owned().await;
                ensure_session_driver(&session, &meta.client_id).await?;
                let subagents = session
                    .subagents()
                    .ok_or_else(|| {
                        HostError::Query(
                            "child-agent control is unavailable for this session".to_owned(),
                        )
                    })?
                    .list(&session_id)
                    .await?;
                Ok((
                    CommandOutcome::Accepted,
                    Some(session_id.clone()),
                    vec![EngineEvent::SubagentsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        subagents,
                    }],
                ))
            }
            ClientCommand::ReplaySubagent {
                meta,
                session_id,
                subagent_id,
                after_sequence,
            } => {
                let session = self.ready_session(&session_id).await?;
                let _lifecycle = Arc::clone(&session.lifecycle).lock_owned().await;
                ensure_session_driver(&session, &meta.client_id).await?;
                let replay = session
                    .subagents()
                    .ok_or_else(|| {
                        HostError::Query(
                            "child-agent control is unavailable for this session".to_owned(),
                        )
                    })?
                    .replay(&session_id, &subagent_id, after_sequence)
                    .await?;
                let completion = subagent_replay_completed(
                    &meta,
                    &session_id,
                    &subagent_id,
                    &replay,
                    &*self.clock,
                );
                let mut events = subagent_replay_batches(
                    &meta,
                    &session_id,
                    &subagent_id,
                    &replay.child_session_id,
                    replay.events,
                    &*self.clock,
                );
                events.push(completion);
                Ok((CommandOutcome::Accepted, Some(session_id), events))
            }
            ClientCommand::ContinueSubagent {
                meta,
                session_id,
                subagent_id,
                content,
            } => {
                let session = self.ready_session(&session_id).await?;
                let _lifecycle = Arc::clone(&session.lifecycle).lock_owned().await;
                ensure_session_driver(&session, &meta.client_id).await?;
                session
                    .subagents()
                    .ok_or_else(|| {
                        HostError::Query(
                            "child-agent control is unavailable for this session".to_owned(),
                        )
                    })?
                    .continue_child(&session_id, &subagent_id, content)
                    .await?;
                Ok((CommandOutcome::Accepted, Some(session_id), Vec::new()))
            }
            ClientCommand::InterruptSubagent {
                meta,
                session_id,
                subagent_id,
            } => {
                let session = self.ready_session(&session_id).await?;
                let _lifecycle = Arc::clone(&session.lifecycle).lock_owned().await;
                ensure_session_driver(&session, &meta.client_id).await?;
                session
                    .subagents()
                    .ok_or_else(|| {
                        HostError::Query(
                            "child-agent control is unavailable for this session".to_owned(),
                        )
                    })?
                    .interrupt(&session_id, &subagent_id)
                    .await?;
                Ok((CommandOutcome::Accepted, Some(session_id), Vec::new()))
            }
            ClientCommand::CloseSubagent {
                meta,
                session_id,
                subagent_id,
            } => {
                let session = self.ready_session(&session_id).await?;
                let _lifecycle = Arc::clone(&session.lifecycle).lock_owned().await;
                ensure_session_driver(&session, &meta.client_id).await?;
                session
                    .subagents()
                    .ok_or_else(|| {
                        HostError::Query(
                            "child-agent control is unavailable for this session".to_owned(),
                        )
                    })?
                    .close(&session_id, &subagent_id)
                    .await?;
                Ok((CommandOutcome::Accepted, Some(session_id), Vec::new()))
            }
            ClientCommand::ShutdownHost { meta } => {
                self.provider_auth.cancel_all();
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
                let persists_model = matches!(
                    command,
                    ClientCommand::SwitchModel { .. } | ClientCommand::AnswerQuestion { .. }
                );
                let lifecycle = (matches!(command, ClientCommand::TakeDriver { .. })
                    || persists_model)
                    .then(|| Arc::clone(&session.lifecycle));
                let _lifecycle = match lifecycle {
                    Some(lifecycle) => Some(lifecycle.lock_owned().await),
                    None => None,
                };
                let previous_driver = if driver.is_some() {
                    session.handle().snapshot().await?.driver_client_id
                } else {
                    None
                };
                let previous_model = if persists_model {
                    Some(session.handle().snapshot().await?.model_alias)
                } else {
                    None
                };
                let outcome = if persists_model {
                    session.handle().dispatch_durably(command).await?
                } else {
                    session.handle().dispatch(command).await?
                };
                if outcome == CommandOutcome::Accepted {
                    // TakeDriver persists its lease before returning Accepted.
                    // Shell commands acknowledge before their durable event,
                    // so that descriptor field is updated by
                    // `project_durable_descriptor`. Model commands use the
                    // awaited durable path below so project preference
                    // persistence cannot be detached or silently ignored.
                    if let Some(driver) = driver {
                        if let Some(previous) =
                            previous_driver.filter(|previous| previous != &driver)
                        {
                            self.provider_auth
                                .cancel_session_client(&previous, &session_id);
                        }
                        session.set_driver(Some(driver));
                    }
                    if let Some(previous_model) = previous_model {
                        let committed_model = session.handle().snapshot().await?.model_alias;
                        if committed_model != previous_model {
                            let model = ModelAlias(committed_model);
                            let descriptor = {
                                let mut descriptor = session
                                    .descriptor
                                    .write()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                descriptor.model = model.clone();
                                descriptor.clone()
                            };
                            self.queries
                                .persist_project_model_selection(&descriptor, &model)
                                .await?;
                        }
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
        self.resume_session_after_reservation::<fn()>(session_id, None)
            .await
    }

    async fn resume_session_after_reservation<F>(
        &self,
        session_id: &SessionId,
        mut on_reserved: Option<F>,
    ) -> Result<Arc<HostedSession>, HostError>
    where
        F: FnOnce(),
    {
        loop {
            let (ready, wait, owns_opening) = {
                let mut registry = self.registry.lock().await;
                if self.shutting_down.load(Ordering::Acquire) {
                    return Err(HostError::ShuttingDown);
                }
                match registry.sessions.get(session_id) {
                    Some(SessionSlot::Ready(session)) => (Some(Arc::clone(session)), None, false),
                    Some(SessionSlot::Opening(completed)) => {
                        (None, Some(completed.subscribe()), false)
                    }
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
                        (None, None, true)
                    }
                }
            };
            if let Some(on_reserved) = on_reserved.take() {
                on_reserved();
            }
            if let Some(session) = ready {
                return Ok(session);
            }
            if let Some(mut completed) = wait {
                if !*completed.borrow_and_update() {
                    let _ = completed.changed().await;
                }
                continue;
            }

            if !owns_opening {
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
    #[allow(clippy::too_many_lines)]
    pub async fn subscribe(
        &self,
        bound: BoundClient,
        session_id: Option<SessionId>,
        last_seen: Option<SequenceId>,
    ) -> Result<mpsc::Receiver<Result<EngineEvent, HostError>>, HostError> {
        let sender = self.client_sender(&bound.client_id);
        let host_events = sender.subscribe();
        let (send, receive) = mpsc::channel(HOST_EVENT_CAPACITY);
        let session = if let Some(session_id) = &session_id {
            Some(self.ready_session(session_id).await?)
        } else {
            None
        };
        let clock = Arc::clone(&self.clock);
        let provider_auth = Arc::clone(&self.provider_auth);
        tokio::spawn(async move {
            let mut subscription = ProviderAuthSubscriptionGuard {
                client_id: bound.client_id.clone(),
                receiver: host_events,
                sender,
                pending: provider_auth,
            };
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
                        host = subscription.receiver.recv() => match host {
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
                    match subscription.receiver.recv().await {
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

    #[allow(clippy::too_many_lines)]
    async fn complete_provider_auth_task(
        self,
        owner: ProviderAuthOwner,
        attempt_id: ProviderAuthAttemptId,
        attempt: ProviderAuthAttempt,
        mut cancel_signal: watch::Receiver<bool>,
        meta: CommandMeta,
    ) {
        let _reservation_guard = ProviderAuthCompletionGuard {
            pending: Arc::clone(&self.provider_auth),
            owner: owner.clone(),
            attempt_id: attempt_id.clone(),
        };
        let cancellation = attempt.cancellation();
        let completion = tokio::select! {
            result = tokio::time::timeout(PROVIDER_AUTH_COMPLETE_DEADLINE, attempt.complete()) => {
                result.unwrap_or_else(|_| {
                    cancellation();
                    Err(HostError::Query("provider authentication deadline exceeded".to_owned()))
                })
            }
            changed = cancel_signal.changed() => {
                let _ = changed;
                Err(HostError::Query("provider authentication was cancelled".to_owned()))
            }
        };
        let mut completion = match completion
            .and_then(|completion| validate_provider_auth_completion(&owner.provider, completion))
        {
            Ok(completion) => completion,
            Err(error) => {
                self.emit_provider_auth_finished(
                    &owner,
                    attempt_id,
                    &meta,
                    false,
                    sanitized_provider_auth_error(&error),
                    Vec::new(),
                );
                return;
            }
        };
        let provider_mutation = Arc::clone(&self.provider_mutation).lock_owned().await;
        let session = match self.ready_session(&owner.session_id).await {
            Ok(session) => session,
            Err(error) => {
                drop(provider_mutation);
                self.emit_provider_auth_finished(
                    &owner,
                    attempt_id,
                    &meta,
                    false,
                    sanitized_provider_auth_error(&error),
                    Vec::new(),
                );
                return;
            }
        };
        let lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
        let snapshot = match session.handle().snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                drop(lifecycle_guard);
                drop(provider_mutation);
                self.emit_provider_auth_finished(
                    &owner,
                    attempt_id,
                    &meta,
                    false,
                    sanitized_provider_auth_error(&HostError::from(error)),
                    Vec::new(),
                );
                return;
            }
        };
        if snapshot.driver_client_id.as_ref() != Some(&owner.client_id)
            || !transition_provider_auth_to_finalizing(&self.provider_auth, &owner, &attempt_id)
        {
            drop(lifecycle_guard);
            drop(provider_mutation);
            return;
        }

        // This transition is the irreversible boundary. The host-owned task
        // now holds both the global mutation lock and the session lifecycle;
        // disconnect and takeover may no longer cancel or interleave the write.
        let persisted = if let Some(persistence) = completion.take_persistence() {
            tokio::task::spawn_blocking(persistence)
                .await
                .map_err(|_| {
                    HostError::Persistence("provider credential storage failed".to_owned())
                })
                .and_then(std::convert::identity)
        } else {
            Ok(Vec::new())
        };
        let (message, warnings) = match persisted {
            Ok(mut persisted_warnings) => {
                completion.warnings.append(&mut persisted_warnings);
                (
                    completion.message,
                    sanitized_persisted_provider_auth_warnings(completion.warnings),
                )
            }
            Err(error) => {
                drop(lifecycle_guard);
                drop(provider_mutation);
                self.emit_provider_auth_finished(
                    &owner,
                    attempt_id,
                    &meta,
                    false,
                    sanitized_provider_auth_error(&error),
                    Vec::new(),
                );
                return;
            }
        };
        // Credential persistence is the authentication result. Report it
        // immediately; activation and catalog discovery are independent
        // readiness work and must not relabel or delay a successful login.
        self.emit_provider_auth_finished(&owner, attempt_id, &meta, true, message, warnings);
        let activated = session
            .handle()
            .activate_provider(&owner.provider, Some(&snapshot.model_alias))
            .await
            .is_ok();
        drop(lifecycle_guard);
        drop(provider_mutation);
        let catalog_ready = self
            .emit_refreshed_provider_catalog(&owner, &meta, snapshot)
            .await;
        let (ready, readiness_message) = match (activated, catalog_ready) {
            (true, Some(true)) => (
                true,
                "Provider connected. Choose a model from /models.".to_owned(),
            ),
            (false, _) => (
                false,
                "Signed in, but the provider connection is not ready. Retry from /providers."
                    .to_owned(),
            ),
            (true, None) => (
                false,
                "Signed in, but the model catalog could not be refreshed. Retry from /providers."
                    .to_owned(),
            ),
            (true, Some(false)) => (
                false,
                "Signed in, but this provider is not reachable or returned no models. Retry from /providers."
                    .to_owned(),
            ),
        };
        self.emit_provider_activation_finished(&owner, &meta, ready, readiness_message);
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_provider_auth_finished(
        &self,
        owner: &ProviderAuthOwner,
        attempt_id: ProviderAuthAttemptId,
        meta: &CommandMeta,
        success: bool,
        message: String,
        warnings: Vec<String>,
    ) {
        self.emit_many(
            &owner.client_id,
            &[EngineEvent::ProviderAuthFinished {
                meta: ack_meta(meta, &*self.clock),
                session_id: owner.session_id.clone(),
                attempt_id,
                provider: owner.provider.clone(),
                success,
                message,
                warnings,
            }],
        );
    }

    fn emit_provider_activation_finished(
        &self,
        owner: &ProviderAuthOwner,
        meta: &CommandMeta,
        success: bool,
        message: String,
    ) {
        self.emit_many(
            &owner.client_id,
            &[EngineEvent::ProviderActivationFinished {
                meta: ack_meta(meta, &*self.clock),
                session_id: owner.session_id.clone(),
                provider: owner.provider.clone(),
                success,
                message,
            }],
        );
    }

    async fn emit_refreshed_provider_catalog(
        &self,
        owner: &ProviderAuthOwner,
        meta: &CommandMeta,
        snapshot: crate::SessionSnapshot,
    ) -> Option<bool> {
        let selected = Some(snapshot.model_alias.as_str());
        let resolved = snapshot.conversation.iter().rev().find_map(|turn| {
            turn.meta
                .model
                .as_deref()
                .filter(|model| model.contains('/'))
        });
        // Authentication is a provider-scoped action. Refreshing the global
        // catalog here would resolve unrelated credentials (and can trigger
        // unrelated credential loading), so readiness must use the live
        // session's provider-aware catalog boundary exclusively.
        let session = self.ready_session(&owner.session_id).await.ok()?;
        let provider_catalog = session.model_catalog()?;
        let mut catalog = provider_catalog
            .refresh_provider(&owner.provider)
            .await
            .ok()?;
        let provider_ready = provider_catalog_is_ready(&catalog, &owner.provider);
        overlay_model_catalog_current(&mut catalog, selected, resolved);
        self.emit_many(
            &owner.client_id,
            &[EngineEvent::ModelsListed {
                meta: ack_meta(meta, &*self.clock),
                session_id: Some(owner.session_id.clone()),
                models: catalog.models,
                aliases: catalog.aliases,
                providers: catalog.providers,
                cached: catalog.cached,
                truncated: catalog.truncated,
            }],
        );
        Some(provider_ready)
    }
}

fn subagent_replay_batches(
    meta: &CommandMeta,
    session_id: &SessionId,
    subagent_id: &SubagentId,
    child_session_id: &SessionId,
    replay: Vec<(SequenceId, Value)>,
    clock: &dyn EventClock,
) -> Vec<EngineEvent> {
    let ack = ack_meta(meta, clock);
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut current_bytes = 0usize;
    for (child_sequence, event) in replay {
        let event_bytes =
            serde_json::to_vec(&event).map_or(SUBAGENT_REPLAY_BATCH_BYTES, |v| v.len());
        if !current.is_empty()
            && (current.len() >= SUBAGENT_REPLAY_BATCH_EVENTS
                || current_bytes.saturating_add(event_bytes) > SUBAGENT_REPLAY_BATCH_BYTES)
        {
            batches.push(EngineEvent::SubagentReplayBatch {
                meta: ack.clone(),
                session_id: session_id.clone(),
                subagent_id: subagent_id.clone(),
                child_session_id: child_session_id.clone(),
                events: std::mem::take(&mut current),
            });
            current_bytes = 0;
        }
        current_bytes = current_bytes.saturating_add(event_bytes);
        current.push(SubagentReplayItem {
            child_sequence,
            event,
        });
    }
    if !current.is_empty() {
        batches.push(EngineEvent::SubagentReplayBatch {
            meta: ack,
            session_id: session_id.clone(),
            subagent_id: subagent_id.clone(),
            child_session_id: child_session_id.clone(),
            events: current,
        });
    }
    debug_assert!(batches.len().saturating_add(2) <= HOST_EVENT_CAPACITY);
    batches
}

fn subagent_replay_completed(
    meta: &CommandMeta,
    session_id: &SessionId,
    subagent_id: &SubagentId,
    replay: &SubagentReplay,
    clock: &dyn EventClock,
) -> EngineEvent {
    EngineEvent::SubagentReplayCompleted {
        meta: ack_meta(meta, clock),
        session_id: session_id.clone(),
        subagent_id: subagent_id.clone(),
        through_sequence: replay.through_sequence,
        next_cursor: replay.next_cursor,
        tail_sequence: replay.tail_sequence,
        has_more: replay.has_more,
        events_before_page: replay.events_before_page,
        truncated: replay.truncated,
    }
}

fn provider_catalog_is_ready(catalog: &ModelCatalogSnapshot, provider_name: &str) -> bool {
    catalog.providers.iter().any(|provider| {
        provider.name == provider_name && provider.reachable && provider.model_count > 0
    })
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

async fn ensure_session_driver(
    session: &HostedSession,
    client_id: &ClientId,
) -> Result<(), HostError> {
    let snapshot = session.handle().snapshot().await?;
    if snapshot.driver_client_id.as_ref() == Some(client_id) {
        Ok(())
    } else {
        Err(HostError::Protocol(
            "only the current driver may control child agents".to_owned(),
        ))
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
        | ClientCommand::GetWorkspaceStatus { session_id, .. }
        | ClientCommand::GetWorkspaceDiff { session_id, .. }
        | ClientCommand::ListCommands { session_id, .. }
        | ClientCommand::ListSettings { session_id, .. }
        | ClientCommand::SetSetting { session_id, .. }
        | ClientCommand::ListMcpServers { session_id, .. }
        | ClientCommand::ListRuntimeServices { session_id, .. }
        | ClientCommand::AddMcpHttpServer { session_id, .. }
        | ClientCommand::ReviewMcpServer { session_id, .. }
        | ClientCommand::ApproveMcpServer { session_id, .. }
        | ClientCommand::SetMcpServerEnabled { session_id, .. }
        | ClientCommand::ListPermissions { session_id, .. }
        | ClientCommand::AddSessionPermissionRule { session_id, .. }
        | ClientCommand::RemoveSessionPermissionRule { session_id, .. }
        | ClientCommand::RevokePermissionApproval { session_id, .. }
        | ClientCommand::BeginProviderAuth { session_id, .. }
        | ClientCommand::ConfigureBuiltinProvider { session_id, .. }
        | ClientCommand::CompleteProviderAuth { session_id, .. }
        | ClientCommand::CancelProviderAuth { session_id, .. }
        | ClientCommand::ListSubagents { session_id, .. }
        | ClientCommand::ReplaySubagent { session_id, .. }
        | ClientCommand::ContinueSubagent { session_id, .. }
        | ClientCommand::InterruptSubagent { session_id, .. }
        | ClientCommand::CloseSubagent { session_id, .. } => Some(session_id.clone()),
        ClientCommand::CreateSession { .. }
        | ClientCommand::ListSessions { .. }
        | ClientCommand::SearchSessions { .. }
        | ClientCommand::ListModels { .. }
        | ClientCommand::ShutdownHost { .. } => None,
    }
}

fn validate_provider_auth_name(provider: &str) -> Result<(), HostError> {
    if provider.is_empty()
        || provider.len() > 128
        || !provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(HostError::Protocol(
            "provider authentication name is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn builtin_provider_auth_kind(provider: &str) -> Result<rw_types::ProviderAuthKind, HostError> {
    match provider {
        "openai_codex" => Ok(rw_types::ProviderAuthKind::Oauth),
        "github_copilot" => Ok(rw_types::ProviderAuthKind::DeviceFlow),
        "openai" | "anthropic" => Ok(rw_types::ProviderAuthKind::ApiKey),
        _ => Err(HostError::Protocol(
            "provider is not in the fixed built-in setup allowlist".to_owned(),
        )),
    }
}

fn provider_auth_attempt_id(
    meta: &CommandMeta,
    session_id: &SessionId,
    provider: &str,
) -> ProviderAuthAttemptId {
    let digest = blake3::hash(
        format!(
            "{}\0{}\0{}\0{}",
            meta.client_id.0, meta.request_id.0, session_id.0, provider
        )
        .as_bytes(),
    );
    ProviderAuthAttemptId(digest.to_hex()[..24].to_owned())
}

fn pending_provider_auth_id(pending: &PendingProviderAuth) -> &ProviderAuthAttemptId {
    match pending {
        PendingProviderAuth::Opening { attempt_id }
        | PendingProviderAuth::Ready { attempt_id, .. }
        | PendingProviderAuth::Completing { attempt_id, .. }
        | PendingProviderAuth::Finalizing { attempt_id } => attempt_id,
    }
}

const fn provider_auth_can_cancel(pending: &PendingProviderAuth) -> bool {
    !matches!(pending, PendingProviderAuth::Finalizing { .. })
}

fn remove_provider_auth_reservation(
    pending: &PendingProviderAuths,
    owner: &ProviderAuthOwner,
    attempt_id: &ProviderAuthAttemptId,
) {
    let mut entries = pending
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if entries
        .get(owner)
        .is_some_and(|attempt| pending_provider_auth_id(attempt) == attempt_id)
    {
        entries.remove(owner);
    }
}

fn transition_provider_auth_to_finalizing(
    pending: &PendingProviderAuths,
    owner: &ProviderAuthOwner,
    attempt_id: &ProviderAuthAttemptId,
) -> bool {
    let mut entries = pending
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if matches!(
        entries.get(owner),
        Some(PendingProviderAuth::Completing { attempt_id: current, .. }) if current == attempt_id
    ) {
        entries.insert(
            owner.clone(),
            PendingProviderAuth::Finalizing {
                attempt_id: attempt_id.clone(),
            },
        );
        true
    } else {
        false
    }
}

fn bounded_provider_auth_prompt(
    attempt: &ProviderAuthAttempt,
) -> Result<(ProviderAuthChallenge, Vec<String>), HostError> {
    let challenge = attempt.challenge();
    let lengths_valid = match challenge {
        ProviderAuthChallenge::Oauth {
            authorization_url,
            redirect_uri,
        } => {
            !authorization_url.is_empty()
                && authorization_url.len() <= MAX_PROVIDER_AUTH_URL_BYTES
                && !redirect_uri.is_empty()
                && redirect_uri.len() <= MAX_PROVIDER_AUTH_URL_BYTES
        }
        ProviderAuthChallenge::DeviceFlow {
            verification_uri,
            user_code,
        } => {
            !verification_uri.is_empty()
                && verification_uri.len() <= MAX_PROVIDER_AUTH_URL_BYTES
                && !user_code.is_empty()
                && user_code.len() <= MAX_PROVIDER_AUTH_CODE_BYTES
        }
    };
    if !lengths_valid {
        return Err(HostError::Query(
            "provider authentication prompt exceeded its safety limit".to_owned(),
        ));
    }
    Ok((
        challenge.clone(),
        bounded_provider_auth_warnings(attempt.warnings())?,
    ))
}

fn bounded_provider_auth_warnings(warnings: &[String]) -> Result<Vec<String>, HostError> {
    if warnings.len() > MAX_PROVIDER_AUTH_WARNINGS
        || warnings
            .iter()
            .any(|warning| warning.len() > MAX_PROVIDER_AUTH_WARNING_BYTES)
    {
        return Err(HostError::Query(
            "provider authentication warnings exceeded their safety limit".to_owned(),
        ));
    }
    Ok(warnings.to_vec())
}

fn sanitized_persisted_provider_auth_warnings(warnings: Vec<String>) -> Vec<String> {
    if bounded_provider_auth_warnings(&warnings).is_ok() {
        return warnings;
    }
    vec![PROVIDER_AUTH_WARNINGS_OMITTED.to_owned()]
}

fn validate_provider_auth_completion(
    expected_provider: &str,
    completion: ProviderAuthCompletion,
) -> Result<ProviderAuthCompletion, HostError> {
    if completion.provider != expected_provider
        || completion.message.is_empty()
        || completion.message.len() > MAX_PROVIDER_AUTH_MESSAGE_BYTES
    {
        return Err(HostError::Query(
            "provider authentication result was invalid".to_owned(),
        ));
    }
    bounded_provider_auth_warnings(&completion.warnings)?;
    Ok(completion)
}

fn sanitized_provider_auth_error(error: &HostError) -> String {
    match error {
        HostError::ShuttingDown => "provider authentication stopped during host shutdown",
        HostError::SessionNotLoaded(_) => "provider authentication session is unavailable",
        HostError::SessionCapacity => "provider authentication capacity is exhausted",
        HostError::Persistence(_) => "provider credential storage failed",
        HostError::Protocol(_) => "provider authentication request was invalid",
        HostError::Query(message) if message.contains("no GitHub OAuth client id") => {
            "GitHub Copilot sign-in is unavailable in this build because it has no compatible OAuth client identity"
        }
        HostError::Query(message) if message.contains("device authorization expired") => {
            "GitHub sign-in expired; start a new sign-in attempt"
        }
        HostError::Query(message) if message.contains("device authorization was denied") => {
            "GitHub sign-in was denied; start a new sign-in attempt to try again"
        }
        HostError::Query(_) | HostError::SessionIdentityMismatch | HostError::RequestConflict => {
            "provider authentication failed"
        }
    }
    .to_owned()
}

fn overlay_model_catalog_current(
    catalog: &mut ModelCatalogSnapshot,
    selected_model: Option<&str>,
    resolved_model: Option<&str>,
) {
    let current = selected_model
        .filter(|selected| selected.contains('/'))
        .or(resolved_model)
        .or(selected_model);
    if let Some(current) = current {
        for model in &mut catalog.models {
            model.current = model.id == current
                || catalog.aliases.iter().any(|alias| {
                    alias.alias.0 == current && alias.candidates.first() == Some(&model.id)
                });
        }
    }
    if let Some(selected) = selected_model {
        for alias in &mut catalog.aliases {
            alias.current = alias.alias.0 == selected;
        }
    }
}

fn wire_command_catalog(
    descriptors: impl IntoIterator<Item = rw_ext::CommandDescriptor>,
) -> (Vec<CommandDescriptor>, bool) {
    let mut commands = Vec::new();
    let mut truncated = false;
    // JSON arrays need two brackets plus one comma between adjacent entries.
    // Serialize each candidate once so catalog projection remains linear and
    // stop examining input after the wire count bound has been proven exceeded.
    let mut serialized_bytes = 2_usize;
    for (index, descriptor) in descriptors.into_iter().enumerate() {
        if index >= MAX_WIRE_COMMANDS {
            truncated = true;
            break;
        }
        let command = CommandDescriptor {
            name: descriptor.name().to_owned(),
            description: descriptor.description().to_owned(),
            usage: descriptor.argument_hint().unwrap_or_default().to_owned(),
            source: match descriptor.source() {
                rw_ext::CommandSource::Builtin => rw_types::CommandSource::Builtin,
                rw_ext::CommandSource::Project => rw_types::CommandSource::Project,
                rw_ext::CommandSource::User => rw_types::CommandSource::User,
                rw_ext::CommandSource::Plugin => rw_types::CommandSource::Plugin,
                rw_ext::CommandSource::Skill => rw_types::CommandSource::Skill,
                rw_ext::CommandSource::Workflow => rw_types::CommandSource::Workflow,
                rw_ext::CommandSource::Mcp => rw_types::CommandSource::Mcp,
            },
        };
        let Ok(encoded) = serde_json::to_vec(&command) else {
            truncated = true;
            break;
        };
        let separator = usize::from(!commands.is_empty());
        let Some(next_size) = serialized_bytes
            .checked_add(separator)
            .and_then(|size| size.checked_add(encoded.len()))
        else {
            truncated = true;
            break;
        };
        if next_size > MAX_WIRE_COMMAND_CATALOG_BYTES {
            truncated = true;
            break;
        }
        serialized_bytes = next_size;
        commands.push(command);
    }
    (commands, truncated)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{
        sync::{
            Condvar,
            atomic::{AtomicU8, AtomicUsize},
        },
        time::Duration,
    };

    use futures_util::stream;
    use rw_ext::{
        CommandDescriptor as ExtensionCommandDescriptor, CommandExecutionError, CommandHandler,
        CommandInvocation,
    };
    use rw_types::{AttachmentData, CommandMeta, PROTOCOL_VERSION};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        ModelDriver, NoopFolderTrustController, NoopMutationCheckpointCoordinator,
        NoopSecretRedactor, NoopSessionEventSink, PermissionGate, SessionActor, SessionActorConfig,
        SessionCommandAction, SessionCommandContext, SessionCommandOutput, SessionEventSink,
        SessionRecoveredState, builtin_command_registry, builtin_hook_dispatcher,
        runtime_support::{
            BoxEventStream, PermissionDecision, ProviderRequest, ThinkingLevel, ToolRegistry,
        },
    };

    #[test]
    fn subagent_replay_batches_are_lossless_and_fit_the_broadcast_window() {
        let replay = (1..=1_024)
            .map(|sequence| {
                (
                    SequenceId(sequence),
                    serde_json::json!({"type": "text_delta", "text": sequence.to_string()}),
                )
            })
            .collect();
        let batches = subagent_replay_batches(
            &meta("driver", "replay"),
            &SessionId("parent".to_owned()),
            &SubagentId("child".to_owned()),
            &SessionId("child-session".to_owned()),
            replay,
            &SystemEventClock,
        );

        assert!(batches.len().saturating_add(2) <= HOST_EVENT_CAPACITY);
        let sequences = batches
            .iter()
            .flat_map(|event| match event {
                EngineEvent::SubagentReplayBatch { events, .. } => events
                    .iter()
                    .map(|item| item.child_sequence.0)
                    .collect::<Vec<_>>(),
                _ => panic!("unexpected replay event"),
            })
            .collect::<Vec<_>>();
        assert_eq!(sequences, (1..=1_024).collect::<Vec<_>>());
    }

    #[test]
    fn empty_subagent_replay_completion_keeps_cursor_and_request_correlation() {
        let command = meta("driver", "tail-replay");
        let replay = SubagentReplay {
            child_session_id: SessionId("child-session".to_owned()),
            events: Vec::new(),
            through_sequence: None,
            next_cursor: Some(SequenceId(9)),
            tail_sequence: Some(SequenceId(9)),
            has_more: false,
            events_before_page: 10,
            truncated: false,
        };
        let completion = subagent_replay_completed(
            &command,
            &SessionId("parent".to_owned()),
            &SubagentId("child".to_owned()),
            &replay,
            &SystemEventClock,
        );
        let EngineEvent::SubagentReplayCompleted {
            meta,
            through_sequence,
            next_cursor,
            tail_sequence,
            has_more,
            events_before_page,
            truncated,
            ..
        } = completion
        else {
            panic!("expected replay completion");
        };
        assert_eq!(meta.client_id, command.client_id);
        assert_eq!(meta.request_id, command.request_id);
        assert_eq!(through_sequence, None);
        assert_eq!(next_cursor, Some(SequenceId(9)));
        assert_eq!(tail_sequence, Some(SequenceId(9)));
        assert!(!has_more);
        assert_eq!(events_before_page, 10);
        assert!(!truncated);
    }

    struct IdleModel;

    struct ActivatableModel;

    struct SummaryModel;

    struct MarkerCommand;

    #[async_trait]
    impl CommandHandler<SessionCommandContext, SessionCommandOutput> for MarkerCommand {
        async fn execute(
            &self,
            _context: &mut SessionCommandContext,
            _invocation: CommandInvocation,
        ) -> Result<SessionCommandOutput, CommandExecutionError> {
            Ok(SessionCommandOutput {
                message: "marker".to_owned(),
                action: SessionCommandAction::None,
            })
        }
    }

    impl ModelDriver for IdleModel {
        fn stream(
            &self,
            _alias: &str,
            _request: ProviderRequest,
        ) -> Result<BoxEventStream, AgentLoopError> {
            Ok(Box::pin(stream::empty()))
        }

        fn has_model_alias(&self, alias: &str) -> bool {
            matches!(alias, "fast" | "big") || alias.contains('/')
        }
    }

    impl ModelDriver for SummaryModel {
        fn stream(
            &self,
            _alias: &str,
            _request: ProviderRequest,
        ) -> Result<BoxEventStream, AgentLoopError> {
            Ok(Box::pin(stream::iter([
                Ok(rw_providers::ProviderEvent::TextDelta {
                    text: "durable model handoff".to_owned(),
                }),
                Ok(rw_providers::ProviderEvent::Finished {
                    reason: rw_providers::FinishReason::Stop,
                }),
            ])))
        }

        fn has_model_alias(&self, alias: &str) -> bool {
            matches!(alias, "fast" | "big") || alias.contains('/')
        }
    }

    #[async_trait]
    impl ModelDriver for ActivatableModel {
        fn stream(
            &self,
            _alias: &str,
            _request: ProviderRequest,
        ) -> Result<BoxEventStream, AgentLoopError> {
            Ok(Box::pin(stream::empty()))
        }

        fn has_model_alias(&self, alias: &str) -> bool {
            matches!(alias, "fast" | "big") || alias.contains('/')
        }

        async fn activate_provider(
            &self,
            _provider: &str,
            _selected_model: Option<&str>,
        ) -> Result<(), AgentLoopError> {
            Ok(())
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
        model: Arc<dyn ModelDriver>,
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
                model: Arc::new(IdleModel),
            }
        }

        fn with_event_sink(event_sink: Arc<dyn SessionEventSink>) -> Self {
            Self {
                event_sink: Some(event_sink),
                ..Self::new()
            }
        }

        fn with_model(model: Arc<dyn ModelDriver>) -> Self {
            Self {
                model,
                ..Self::new()
            }
        }

        fn session(&self, session_id: &SessionId) -> HostedSession {
            let workspace = self.root.path().join(&session_id.0);
            std::fs::create_dir_all(&workspace).expect("session workspace");
            let mut commands = builtin_command_registry().expect("commands");
            commands
                .register(
                    ExtensionCommandDescriptor::new(
                        format!("only.{}", session_id.0),
                        "session-specific command",
                    ),
                    MarkerCommand,
                )
                .expect("session marker command");
            let handle = SessionActor::spawn(SessionActorConfig {
                session_id: session_id.clone(),
                workspace_root: workspace,
                additional_workspace_roots: Vec::new(),
                workspace_generation: 0,
                initial_session_context: Vec::new(),
                startup_notifications: Vec::new(),
                model_alias: "fast".to_owned(),
                model: Arc::clone(&self.model),
                tools: Arc::new(ToolRegistry::new()),
                permissions: Arc::new(PermissionGate::new(PermissionDecision::Allow)),
                hooks: Arc::new(builtin_hook_dispatcher().expect("hooks")),
                commands: Arc::new(commands),
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
                    title: "New session".to_owned(),
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
    struct StubQueries {
        auth: Option<Arc<AuthFixture>>,
        persisted_models: std::sync::Mutex<Vec<String>>,
        fail_model_catalog: bool,
        fail_model_persistence: bool,
    }

    struct AuthFixture {
        completion: watch::Sender<bool>,
        cancelled: Arc<AtomicBool>,
        persistence: Option<Arc<BlockingCredentialMutation>>,
    }

    impl AuthFixture {
        fn pending() -> Arc<Self> {
            let (completion, _) = watch::channel(false);
            Arc::new(Self {
                completion,
                cancelled: Arc::new(AtomicBool::new(false)),
                persistence: None,
            })
        }

        fn with_persistence(persistence: Arc<BlockingCredentialMutation>) -> Arc<Self> {
            let (completion, _) = watch::channel(false);
            Arc::new(Self {
                completion,
                cancelled: Arc::new(AtomicBool::new(false)),
                persistence: Some(persistence),
            })
        }
    }

    struct BlockingCredentialMutation {
        started: Notify,
        persisted: AtomicBool,
        gate: (Mutex<bool>, Condvar),
    }

    impl BlockingCredentialMutation {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                started: Notify::new(),
                persisted: AtomicBool::new(false),
                gate: (Mutex::new(false), Condvar::new()),
            })
        }

        #[allow(clippy::unnecessary_wraps)]
        fn run(&self) -> Result<Vec<String>, HostError> {
            self.started.notify_one();
            let (gate, release) = &self.gate;
            let mut open = gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*open {
                open = release
                    .wait(open)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            self.persisted.store(true, Ordering::Release);
            Ok(Vec::new())
        }

        fn release(&self) {
            let (gate, release) = &self.gate;
            *gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            release.notify_all();
        }
    }

    #[async_trait]
    impl HostQueryService for StubQueries {
        async fn command_descriptors(&self) -> Result<Vec<CommandDescriptor>, HostError> {
            Ok(vec![CommandDescriptor {
                name: "help".to_owned(),
                description: "Show help".to_owned(),
                usage: "/help".to_owned(),
                source: rw_types::CommandSource::default(),
            }])
        }

        async fn model_catalog(
            &self,
            _refresh: bool,
            _selected_model: Option<&str>,
            _resolved_model: Option<&str>,
        ) -> Result<ModelCatalogSnapshot, HostError> {
            if self.fail_model_catalog {
                return Err(HostError::Query(
                    "injected provider catalog refresh failure".to_owned(),
                ));
            }
            Ok(ModelCatalogSnapshot {
                aliases: Vec::new(),
                models: Vec::new(),
                providers: Vec::new(),
                cached: false,
                truncated: false,
            })
        }

        async fn persist_project_model_selection(
            &self,
            _session: &SessionDescriptor,
            model: &ModelAlias,
        ) -> Result<(), HostError> {
            if self.fail_model_persistence {
                return Err(HostError::Query(
                    "injected project model persistence failure".to_owned(),
                ));
            }
            self.persisted_models
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(model.0.clone());
            Ok(())
        }

        async fn begin_provider_auth(
            &self,
            provider: &str,
        ) -> Result<ProviderAuthAttempt, HostError> {
            let fixture = self.auth.clone().ok_or_else(|| {
                HostError::Query("provider authentication is unavailable".to_owned())
            })?;
            let mut completion = fixture.completion.subscribe();
            let completion_provider = provider.to_owned();
            let persistence = fixture.persistence.clone();
            let future = Box::pin(async move {
                while !*completion.borrow_and_update() {
                    completion.changed().await.map_err(|_| {
                        HostError::Query("provider authentication cancelled".to_owned())
                    })?;
                }
                let completion = ProviderAuthCompletion::new(
                    completion_provider,
                    "provider authentication completed".to_owned(),
                    Vec::new(),
                );
                Ok(if let Some(persistence) = persistence {
                    completion.with_persistence(move || persistence.run())
                } else {
                    completion
                })
            });
            let cancellation = Arc::clone(&fixture.cancelled);
            let cancel_signal = fixture.completion.clone();
            Ok(ProviderAuthAttempt::new(
                ProviderAuthChallenge::DeviceFlow {
                    verification_uri: "https://example.test/device".to_owned(),
                    user_code: "ABCD-1234".to_owned(),
                },
                Vec::new(),
                future,
                Arc::new(move || {
                    cancellation.store(true, Ordering::Release);
                    let _ = cancel_signal.send(true);
                }),
            ))
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

        async fn workspace_diff(
            &self,
            _session: &SessionDescriptor,
            path: &str,
            _max_bytes: u32,
        ) -> Result<WorkspaceDiff, HostError> {
            Ok(WorkspaceDiff {
                path: path.to_owned(),
                unified_diff: String::new(),
                truncated: false,
                binary: false,
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
            Arc::new(StubQueries::default()),
        )
        .expect("host");
        (host, factory)
    }

    #[tokio::test]
    async fn accepted_alias_and_concrete_model_switches_persist_in_dispatch_order() {
        let factory = Arc::new(StubFactory::new());
        let queries = Arc::new(StubQueries::default());
        let host = EngineHost::new(
            EngineHostConfig {
                max_sessions: 1,
                max_deduplicated_requests: 32,
            },
            factory,
            queries.clone(),
        )
        .expect("host");
        let session_id = SessionId("ordered-model-switches".to_owned());
        let driver = BoundClient {
            client_id: ClientId("driver".to_owned()),
        };
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::ResumeSession {
                    meta: meta("spoofed", "resume"),
                    session_id: session_id.clone(),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        for (request, model) in [("switch-a", "big"), ("switch-b", "openai/b")] {
            assert_eq!(
                host.dispatch(
                    driver.clone(),
                    ClientCommand::SwitchModel {
                        meta: meta("spoofed", request),
                        session_id: session_id.clone(),
                        model: ModelAlias(model.to_owned()),
                        provider: None,
                    },
                )
                .await,
                CommandOutcome::Accepted
            );
            assert_eq!(
                queries
                    .persisted_models
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .last()
                    .map(String::as_str),
                Some(model),
                "dispatch must not complete before the committed preference is persisted"
            );
        }
        assert_eq!(
            *queries
                .persisted_models
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!["big".to_owned(), "openai/b".to_owned()]
        );
        assert_eq!(
            host.session(&session_id)
                .await
                .expect("session")
                .descriptor()
                .model,
            ModelAlias("openai/b".to_owned())
        );
    }

    #[tokio::test]
    async fn model_switch_persistence_failure_is_visible_after_the_session_commit() {
        let factory = Arc::new(StubFactory::new());
        let queries = Arc::new(StubQueries {
            fail_model_persistence: true,
            ..StubQueries::default()
        });
        let host = EngineHost::new(
            EngineHostConfig {
                max_sessions: 1,
                max_deduplicated_requests: 32,
            },
            factory,
            queries.clone(),
        )
        .expect("host");
        let session_id = SessionId("failed-model-preference".to_owned());
        let driver = BoundClient {
            client_id: ClientId("driver".to_owned()),
        };
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::ResumeSession {
                    meta: meta("spoofed", "resume"),
                    session_id: session_id.clone(),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        assert!(matches!(
            host.dispatch(
                driver,
                ClientCommand::SwitchModel {
                    meta: meta("spoofed", "switch"),
                    session_id: session_id.clone(),
                    model: ModelAlias("big".to_owned()),
                    provider: None,
                },
            )
            .await,
            CommandOutcome::Rejected { error } if error.code == "host_query_failure"
        ));

        let session = host.session(&session_id).await.expect("session");
        assert_eq!(
            session
                .handle()
                .snapshot()
                .await
                .expect("snapshot")
                .model_alias,
            "big",
            "the journaled session switch remains correct when preference caching fails"
        );
        assert_eq!(session.descriptor().model, ModelAlias("big".to_owned()));
        assert!(
            queries
                .persisted_models
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn pending_switch_is_not_persisted_and_each_context_choice_persists_on_commit() {
        #[allow(clippy::too_many_lines)]
        async fn run(strategy: &str, model: Arc<dyn ModelDriver>) {
            let factory = Arc::new(StubFactory::with_model(model));
            let queries = Arc::new(StubQueries::default());
            let host = EngineHost::new(
                EngineHostConfig {
                    max_sessions: 1,
                    max_deduplicated_requests: 32,
                },
                factory,
                queries.clone(),
            )
            .expect("host");
            let session_id = SessionId(format!("model-context-{strategy}"));
            let driver = BoundClient {
                client_id: ClientId("driver".to_owned()),
            };
            assert_eq!(
                host.dispatch(
                    driver.clone(),
                    ClientCommand::ResumeSession {
                        meta: meta("spoofed", "resume"),
                        session_id: session_id.clone(),
                        last_seen_sequence: None,
                        role: ClientRole::Driver,
                    },
                )
                .await,
                CommandOutcome::Accepted
            );
            assert_eq!(
                host.dispatch(
                    driver.clone(),
                    ClientCommand::UserShellStarted {
                        meta: meta("spoofed", "shell-start"),
                        session_id: session_id.clone(),
                        command: "printf durable-context".to_owned(),
                    },
                )
                .await,
                CommandOutcome::Accepted
            );
            let session = host.session(&session_id).await.expect("session");
            let shell_id = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if let Some(shell) = session
                        .handle()
                        .snapshot()
                        .await
                        .expect("shell snapshot")
                        .active_shell
                    {
                        break shell.shell_id;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("shell became active");
            host.complete_user_shell(&session_id, shell_id, 0, Some("durable-context".to_owned()))
                .await
                .expect("shell context committed");

            let tail = session.handle().last_sequence().await.expect("tail");
            let mut events = session
                .handle()
                .subscribe_client(driver.client_id.clone(), tail);
            assert_eq!(
                host.dispatch(
                    driver.clone(),
                    ClientCommand::SwitchModel {
                        meta: meta("spoofed", "switch"),
                        session_id: session_id.clone(),
                        model: ModelAlias("big".to_owned()),
                        provider: None,
                    },
                )
                .await,
                CommandOutcome::Accepted
            );
            let question_id = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if let EngineEvent::QuestionAsked { question_id, .. } =
                        events.recv().await.expect("question event")
                    {
                        break question_id;
                    }
                }
            })
            .await
            .expect("model context question");
            tokio::task::yield_now().await;
            assert!(
                queries
                    .persisted_models
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_empty(),
                "opening a context-transfer question must not persist the target"
            );
            assert_eq!(session.descriptor().model, ModelAlias("fast".to_owned()));

            assert_eq!(
                host.dispatch(
                    driver,
                    ClientCommand::AnswerQuestion {
                        meta: meta("spoofed", "answer"),
                        session_id,
                        question_id: question_id.clone(),
                        answers: vec![rw_types::Answer {
                            question_id,
                            values: vec![strategy.to_owned()],
                        }],
                    },
                )
                .await,
                CommandOutcome::Accepted
            );
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let persisted = queries
                        .persisted_models
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    if persisted == ["big"] {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("committed model persisted");
            assert_eq!(
                *queries
                    .persisted_models
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                vec!["big".to_owned()]
            );
            assert_eq!(session.descriptor().model, ModelAlias("big".to_owned()));
        }

        run("pass_summary", Arc::new(SummaryModel)).await;
        run("pass_full_context", Arc::new(IdleModel)).await;
        run("start_without_context", Arc::new(IdleModel)).await;
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
                    session_id: None,
                    refresh: false,
                },
            )
            .await;
        assert!(matches!(
            conflict,
            CommandOutcome::Rejected { error } if error.code == "request_id_conflict"
        ));
    }

    #[tokio::test]
    async fn list_commands_routes_to_the_explicit_sessions_assembled_registry() {
        let (host, _factory) = host(3);
        let bound = BoundClient {
            client_id: ClientId("palette-driver".to_owned()),
        };
        for name in ["palette-first", "palette-second"] {
            assert_eq!(
                host.dispatch(
                    bound.clone(),
                    ClientCommand::ResumeSession {
                        meta: meta("spoofed", &format!("resume-{name}")),
                        session_id: SessionId(name.to_owned()),
                        last_seen_sequence: None,
                        role: ClientRole::Driver,
                    },
                )
                .await,
                CommandOutcome::Accepted
            );
        }
        let mut events = host
            .subscribe(bound.clone(), None, None)
            .await
            .expect("command catalog events");
        for name in ["palette-first", "palette-second"] {
            assert_eq!(
                host.dispatch(
                    bound.clone(),
                    ClientCommand::ListCommands {
                        meta: meta("spoofed", &format!("list-{name}")),
                        session_id: SessionId(name.to_owned()),
                    },
                )
                .await,
                CommandOutcome::Accepted
            );
            let (session_id, commands, truncated) =
                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        if let EngineEvent::CommandDescriptorsListed {
                            session_id,
                            commands,
                            truncated,
                            ..
                        } = events
                            .recv()
                            .await
                            .expect("command catalog event")
                            .expect("command catalog result")
                            && session_id.0 == name
                        {
                            break (session_id, commands, truncated);
                        }
                    }
                })
                .await
                .expect("command catalog deadline");
            assert_eq!(session_id, SessionId(name.to_owned()));
            assert!(!truncated);
            assert!(
                commands
                    .iter()
                    .any(|command| command.name == format!("only.{name}"))
            );
            let other = if name == "palette-first" {
                "palette-second"
            } else {
                "palette-first"
            };
            assert!(
                commands
                    .iter()
                    .all(|command| command.name != format!("only.{other}"))
            );
            assert!(commands.iter().any(|command| command.name == "permissions"));
            assert!(commands.iter().any(|command| command.name == "add-dir"));
        }
    }

    #[test]
    fn wire_command_catalog_is_bounded_below_the_sse_line_limit() {
        let descriptors = (0..600).map(|index| {
            ExtensionCommandDescriptor::new(
                format!("catalog-{index}"),
                format!("{}-{index}", "description".repeat(80)),
            )
            .with_argument_hint("<value>".repeat(20))
        });
        let (commands, truncated) = wire_command_catalog(descriptors);
        assert!(truncated);
        assert!(commands.len() <= MAX_WIRE_COMMANDS);
        assert!(
            serde_json::to_vec(&commands)
                .expect("bounded command catalog JSON")
                .len()
                <= MAX_WIRE_COMMAND_CATALOG_BYTES
        );
    }

    #[test]
    fn wire_command_catalog_preserves_each_runtime_source() {
        let sources = [
            (
                rw_ext::CommandSource::Builtin,
                rw_types::CommandSource::Builtin,
            ),
            (
                rw_ext::CommandSource::Project,
                rw_types::CommandSource::Project,
            ),
            (rw_ext::CommandSource::User, rw_types::CommandSource::User),
            (
                rw_ext::CommandSource::Plugin,
                rw_types::CommandSource::Plugin,
            ),
            (rw_ext::CommandSource::Skill, rw_types::CommandSource::Skill),
            (
                rw_ext::CommandSource::Workflow,
                rw_types::CommandSource::Workflow,
            ),
            (rw_ext::CommandSource::Mcp, rw_types::CommandSource::Mcp),
        ];
        let descriptors = sources.iter().enumerate().map(|(index, (source, _))| {
            ExtensionCommandDescriptor::new(format!("source-{index}"), "source test")
                .with_source(*source)
        });

        let (commands, truncated) = wire_command_catalog(descriptors);

        assert!(!truncated);
        assert_eq!(commands.len(), sources.len());
        for (command, (_, expected)) in commands.iter().zip(sources) {
            assert_eq!(command.source, expected);
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn provider_auth_completion_is_async_and_stale_cancel_keeps_real_attempt() {
        let fixture = AuthFixture::pending();
        let factory = Arc::new(StubFactory::new());
        let host = EngineHost::new(
            EngineHostConfig {
                max_sessions: 1,
                max_deduplicated_requests: 32,
            },
            factory,
            Arc::new(StubQueries {
                auth: Some(Arc::clone(&fixture)),
                ..StubQueries::default()
            }),
        )
        .expect("host");
        let session_id = SessionId("provider-auth".to_owned());
        host.prepare_session(
            CreateSessionRequest {
                session_id: session_id.clone(),
                workspace: "workspace".to_owned(),
                model: None,
            },
            false,
        )
        .await
        .expect("session");
        let driver = BoundClient {
            client_id: ClientId("auth-driver".to_owned()),
        };
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::TakeDriver {
                    meta: meta("spoofed", "auth-take"),
                    session_id: session_id.clone(),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let mut events = host
            .subscribe(driver.clone(), Some(session_id.clone()), None)
            .await
            .expect("events");
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::BeginProviderAuth {
                    meta: meta("spoofed", "auth-begin"),
                    session_id: session_id.clone(),
                    provider: "github_copilot".to_owned(),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let attempt_id = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let EngineEvent::ProviderAuthStarted { attempt_id, .. } = events
                    .recv()
                    .await
                    .expect("auth event")
                    .expect("auth result")
                {
                    break attempt_id;
                }
            }
        })
        .await
        .expect("auth prompt");
        assert_eq!(
            tokio::time::timeout(
                Duration::from_millis(100),
                host.dispatch(
                    driver.clone(),
                    ClientCommand::CompleteProviderAuth {
                        meta: meta("spoofed", "auth-complete"),
                        session_id: session_id.clone(),
                        provider: "github_copilot".to_owned(),
                        attempt_id: attempt_id.clone(),
                    },
                ),
            )
            .await
            .expect("completion command must not await device polling"),
            CommandOutcome::Accepted
        );
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::CompleteProviderAuth {
                    meta: meta("spoofed", "auth-complete-replayed"),
                    session_id: session_id.clone(),
                    provider: "github_copilot".to_owned(),
                    attempt_id: attempt_id.clone(),
                },
            )
            .await,
            CommandOutcome::Accepted,
            "a replayed durable auth prompt must join the in-flight completion"
        );
        assert!(matches!(
            host.dispatch(
                driver.clone(),
                ClientCommand::CancelProviderAuth {
                    meta: meta("spoofed", "auth-stale-cancel"),
                    session_id: session_id.clone(),
                    provider: "github_copilot".to_owned(),
                    attempt_id: ProviderAuthAttemptId("stale".to_owned()),
                },
            )
            .await,
            CommandOutcome::Rejected { .. }
        ));
        assert!(!fixture.cancelled.load(Ordering::Acquire));
        assert_eq!(
            host.dispatch(
                driver,
                ClientCommand::CancelProviderAuth {
                    meta: meta("spoofed", "auth-cancel"),
                    session_id,
                    provider: "github_copilot".to_owned(),
                    attempt_id,
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        assert!(fixture.cancelled.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn provider_auth_poll_is_cancelled_when_another_driver_takes_over() {
        let fixture = AuthFixture::pending();
        let factory = Arc::new(StubFactory::new());
        let host = EngineHost::new(
            EngineHostConfig {
                max_sessions: 1,
                max_deduplicated_requests: 32,
            },
            factory,
            Arc::new(StubQueries {
                auth: Some(Arc::clone(&fixture)),
                ..StubQueries::default()
            }),
        )
        .expect("host");
        let session_id = SessionId("provider-auth-takeover".to_owned());
        host.prepare_session(
            CreateSessionRequest {
                session_id: session_id.clone(),
                workspace: "workspace".to_owned(),
                model: None,
            },
            false,
        )
        .await
        .expect("session");
        let original = BoundClient {
            client_id: ClientId("original-driver".to_owned()),
        };
        for command in [
            ClientCommand::TakeDriver {
                meta: meta("spoofed", "take-original"),
                session_id: session_id.clone(),
            },
            ClientCommand::BeginProviderAuth {
                meta: meta("spoofed", "begin-original"),
                session_id: session_id.clone(),
                provider: "github_copilot".to_owned(),
            },
        ] {
            assert_eq!(
                host.dispatch(original.clone(), command).await,
                CommandOutcome::Accepted
            );
        }
        let attempt_id = {
            let entries = host
                .provider_auth
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending_provider_auth_id(entries.values().next().expect("pending auth")).clone()
        };
        assert_eq!(
            host.dispatch(
                original,
                ClientCommand::CompleteProviderAuth {
                    meta: meta("spoofed", "complete-original"),
                    session_id: session_id.clone(),
                    provider: "github_copilot".to_owned(),
                    attempt_id,
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        assert_eq!(
            host.dispatch(
                BoundClient {
                    client_id: ClientId("replacement-driver".to_owned()),
                },
                ClientCommand::TakeDriver {
                    meta: meta("spoofed", "take-replacement"),
                    session_id,
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while !fixture.cancelled.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("takeover cancellation");
    }

    #[tokio::test]
    async fn cancelled_begin_future_drops_its_opening_reservation() {
        let pending = Arc::new(PendingProviderAuths::default());
        let owner = ProviderAuthOwner {
            client_id: ClientId("cancelled-begin".to_owned()),
            session_id: SessionId("cancelled-begin-session".to_owned()),
            provider: "github_copilot".to_owned(),
        };
        let attempt_id = ProviderAuthAttemptId("cancelled-opening".to_owned());
        pending
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                owner.clone(),
                PendingProviderAuth::Opening {
                    attempt_id: attempt_id.clone(),
                },
            );
        let task = tokio::spawn({
            let pending = Arc::clone(&pending);
            async move {
                let _guard = ProviderAuthOpeningGuard {
                    pending,
                    owner,
                    attempt_id,
                    armed: true,
                };
                std::future::pending::<()>().await;
            }
        });
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.expect_err("cancelled begin").is_cancelled());
        assert!(
            pending
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn cancelled_api_key_request_cannot_interrupt_store_or_overtake_lifecycle() {
        let mutation = BlockingCredentialMutation::new();
        let store = {
            let mutation = Arc::clone(&mutation);
            Arc::new(move |_provider: String, _api_key: ProviderApiKey| mutation.run())
                as Arc<ProviderApiKeyStore>
        };
        let host = EngineHost::new(
            EngineHostConfig {
                max_sessions: 1,
                max_deduplicated_requests: 32,
            },
            Arc::new(StubFactory::new()),
            Arc::new(StubQueries::default()),
        )
        .expect("host")
        .with_provider_api_key_store(store);
        let session_id = SessionId("api-key-cancellation".to_owned());
        host.prepare_session(
            CreateSessionRequest {
                session_id: session_id.clone(),
                workspace: "workspace".to_owned(),
                model: None,
            },
            false,
        )
        .await
        .expect("session");
        let original = BoundClient {
            client_id: ClientId("api-key-owner".to_owned()),
        };
        assert_eq!(
            host.dispatch(
                original.clone(),
                ClientCommand::TakeDriver {
                    meta: meta("spoofed", "take-api-key-owner"),
                    session_id: session_id.clone(),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let request = tokio::spawn({
            let host = host.clone();
            let session_id = session_id.clone();
            async move {
                host.submit_provider_api_key(
                    original,
                    &session_id,
                    "openai",
                    ProviderApiKey::from_terminal_input("request-only-secret".to_owned())
                        .expect("key"),
                )
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), mutation.started.notified())
            .await
            .expect("store started");
        request.abort();
        assert!(
            request
                .await
                .expect_err("request cancellation")
                .is_cancelled()
        );

        let mut takeover = tokio::spawn({
            let host = host.clone();
            let session_id = session_id.clone();
            async move {
                host.dispatch(
                    BoundClient {
                        client_id: ClientId("api-key-replacement".to_owned()),
                    },
                    ClientCommand::TakeDriver {
                        meta: meta("spoofed", "take-api-key-replacement"),
                        session_id,
                    },
                )
                .await
            }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut takeover)
                .await
                .is_err(),
            "takeover must wait while the irreversible store owns lifecycle"
        );
        mutation.release();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), takeover)
                .await
                .expect("takeover completed")
                .expect("takeover task"),
            CommandOutcome::Accepted
        );
        assert!(mutation.persisted.load(Ordering::Acquire));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn oauth_and_api_key_mutations_share_one_global_store_boundary() {
        let oauth_mutation = BlockingCredentialMutation::new();
        let api_mutation = BlockingCredentialMutation::new();
        let fixture = AuthFixture::with_persistence(Arc::clone(&oauth_mutation));
        let store = {
            let api_mutation = Arc::clone(&api_mutation);
            Arc::new(move |_provider: String, _api_key: ProviderApiKey| api_mutation.run())
                as Arc<ProviderApiKeyStore>
        };
        let host = EngineHost::new(
            EngineHostConfig {
                max_sessions: 2,
                max_deduplicated_requests: 64,
            },
            Arc::new(StubFactory::new()),
            Arc::new(StubQueries {
                auth: Some(Arc::clone(&fixture)),
                ..StubQueries::default()
            }),
        )
        .expect("host")
        .with_provider_api_key_store(store);
        let auth_session = SessionId("oauth-mutation".to_owned());
        let api_session = SessionId("api-mutation".to_owned());
        let driver = BoundClient {
            client_id: ClientId("mutation-driver".to_owned()),
        };
        for session_id in [&auth_session, &api_session] {
            host.prepare_session(
                CreateSessionRequest {
                    session_id: session_id.clone(),
                    workspace: format!("workspace-{}", session_id.0),
                    model: None,
                },
                false,
            )
            .await
            .expect("session");
            assert_eq!(
                host.dispatch(
                    driver.clone(),
                    ClientCommand::TakeDriver {
                        meta: meta("spoofed", &format!("take-{}", session_id.0)),
                        session_id: session_id.clone(),
                    },
                )
                .await,
                CommandOutcome::Accepted
            );
        }
        let mut auth_events = host
            .subscribe(driver.clone(), Some(auth_session.clone()), None)
            .await
            .expect("auth events");
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::BeginProviderAuth {
                    meta: meta("spoofed", "begin-global-auth"),
                    session_id: auth_session.clone(),
                    provider: "github_copilot".to_owned(),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let attempt_id = {
            let entries = host
                .provider_auth
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending_provider_auth_id(entries.values().next().expect("pending auth")).clone()
        };
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::CompleteProviderAuth {
                    meta: meta("spoofed", "complete-global-auth"),
                    session_id: auth_session,
                    provider: "github_copilot".to_owned(),
                    attempt_id,
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        fixture.completion.send_replace(true);
        tokio::time::timeout(Duration::from_secs(1), oauth_mutation.started.notified())
            .await
            .expect("OAuth persistence started");

        let api_request = tokio::spawn({
            let host = host.clone();
            async move {
                host.submit_provider_api_key(
                    driver,
                    &api_session,
                    "openai",
                    ProviderApiKey::from_terminal_input("another-request-secret".to_owned())
                        .expect("key"),
                )
                .await
            }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), api_mutation.started.notified())
                .await
                .is_err(),
            "API-key persistence must wait for OAuth persistence globally"
        );
        oauth_mutation.release();
        tokio::time::timeout(Duration::from_secs(1), api_mutation.started.notified())
            .await
            .expect("API-key persistence started after OAuth release");
        api_mutation.release();
        let submission = tokio::time::timeout(Duration::from_secs(1), api_request)
            .await
            .expect("API-key request completed")
            .expect("API-key task")
            .expect("API-key submission");
        assert!(submission.stored);
        assert!(oauth_mutation.persisted.load(Ordering::Acquire));
        assert!(api_mutation.persisted.load(Ordering::Acquire));
        let (success, message, warnings) = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let EngineEvent::ProviderAuthFinished {
                    success,
                    message,
                    warnings,
                    ..
                } = auth_events
                    .recv()
                    .await
                    .expect("auth event")
                    .expect("auth result")
                {
                    break (success, message, warnings);
                }
            }
        })
        .await
        .expect("auth completion event");
        assert!(success, "stored credentials complete authentication");
        assert_eq!(message, "provider authentication completed");
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn provider_catalog_refresh_failure_does_not_delay_or_relabel_login() {
        let fixture = AuthFixture::pending();
        let host = EngineHost::new(
            EngineHostConfig {
                max_sessions: 1,
                max_deduplicated_requests: 32,
            },
            Arc::new(StubFactory::with_model(Arc::new(ActivatableModel))),
            Arc::new(StubQueries {
                auth: Some(Arc::clone(&fixture)),
                fail_model_catalog: true,
                ..StubQueries::default()
            }),
        )
        .expect("host");
        let session_id = SessionId("provider-catalog-warning".to_owned());
        host.prepare_session(
            CreateSessionRequest {
                session_id: session_id.clone(),
                workspace: "workspace".to_owned(),
                model: None,
            },
            false,
        )
        .await
        .expect("session");
        let driver = BoundClient {
            client_id: ClientId("catalog-warning-driver".to_owned()),
        };
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::TakeDriver {
                    meta: meta("spoofed", "catalog-warning-take"),
                    session_id: session_id.clone(),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let mut events = host
            .subscribe(driver.clone(), Some(session_id.clone()), None)
            .await
            .expect("events");
        assert_eq!(
            host.dispatch(
                driver.clone(),
                ClientCommand::BeginProviderAuth {
                    meta: meta("spoofed", "catalog-warning-begin"),
                    session_id: session_id.clone(),
                    provider: "github_copilot".to_owned(),
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        let attempt_id = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let EngineEvent::ProviderAuthStarted { attempt_id, .. } = events
                    .recv()
                    .await
                    .expect("auth event")
                    .expect("auth result")
                {
                    break attempt_id;
                }
            }
        })
        .await
        .expect("auth prompt");
        assert_eq!(
            host.dispatch(
                driver,
                ClientCommand::CompleteProviderAuth {
                    meta: meta("spoofed", "catalog-warning-complete"),
                    session_id,
                    provider: "github_copilot".to_owned(),
                    attempt_id,
                },
            )
            .await,
            CommandOutcome::Accepted
        );
        fixture.completion.send_replace(true);

        let (success, message, warnings) = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let EngineEvent::ProviderAuthFinished {
                    success,
                    message,
                    warnings,
                    ..
                } = events
                    .recv()
                    .await
                    .expect("auth event")
                    .expect("auth result")
                {
                    break (success, message, warnings);
                }
            }
        })
        .await
        .expect("auth completion event");
        assert!(success, "catalog refresh does not redefine login success");
        assert_eq!(message, "provider authentication completed");
        assert!(warnings.is_empty());
        let (ready, message) = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let EngineEvent::ProviderActivationFinished {
                    success, message, ..
                } = events
                    .recv()
                    .await
                    .expect("readiness event")
                    .expect("readiness result")
                {
                    break (success, message);
                }
            }
        })
        .await
        .expect("provider readiness event");
        assert!(!ready);
        assert!(message.contains("catalog could not be refreshed"));
    }

    #[test]
    fn provider_readiness_requires_the_target_catalog_row_to_be_usable() {
        let descriptor =
            |name: &str, reachable: bool, model_count: u32| rw_types::ProviderDescriptor {
                name: name.to_owned(),
                auth_kind: rw_types::ProviderAuthKind::DeviceFlow,
                next_action: rw_types::ProviderNextAction::SelectModels,
                configured: true,
                authenticated: true,
                reachable,
                model_count,
                status: None,
            };
        let catalog = ModelCatalogSnapshot {
            aliases: Vec::new(),
            models: Vec::new(),
            providers: vec![
                descriptor("openai_codex", true, 3),
                descriptor("github_copilot", false, 0),
            ],
            cached: false,
            truncated: false,
        };

        assert!(!provider_catalog_is_ready(&catalog, "github_copilot"));
        assert!(provider_catalog_is_ready(&catalog, "openai_codex"));
        let empty_target = ModelCatalogSnapshot {
            providers: vec![descriptor("github_copilot", true, 0)],
            ..catalog
        };
        assert!(!provider_catalog_is_ready(&empty_target, "github_copilot"));
    }

    #[test]
    fn provider_auth_prompts_and_connection_events_are_bounded_and_non_durable() {
        let oversized = ProviderAuthAttempt::new(
            ProviderAuthChallenge::Oauth {
                authorization_url: format!("https://example.test/{}", "x".repeat(4_096)),
                redirect_uri: "http://127.0.0.1/callback".to_owned(),
            },
            Vec::new(),
            Box::pin(std::future::pending()),
            Arc::new(|| {}),
        );
        assert!(bounded_provider_auth_prompt(&oversized).is_err());

        let warning_flood = ProviderAuthAttempt::new(
            ProviderAuthChallenge::DeviceFlow {
                verification_uri: "https://example.test/device".to_owned(),
                user_code: "ABCD-1234".to_owned(),
            },
            vec!["warning".to_owned(); MAX_PROVIDER_AUTH_WARNINGS + 1],
            Box::pin(std::future::pending()),
            Arc::new(|| {}),
        );
        assert!(bounded_provider_auth_prompt(&warning_flood).is_err());

        let event = EngineEvent::ProviderAuthStarted {
            meta: CommandAckMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId("client".to_owned()),
                request_id: RequestId("request".to_owned()),
                emitted_at: "now".to_owned(),
            },
            session_id: SessionId("session".to_owned()),
            attempt_id: ProviderAuthAttemptId("attempt".to_owned()),
            provider: "github_copilot".to_owned(),
            challenge: ProviderAuthChallenge::DeviceFlow {
                verification_uri: "https://example.test/device".to_owned(),
                user_code: "ABCD-1234".to_owned(),
            },
            warnings: Vec::new(),
        };
        assert!(event.meta().is_none());
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
    async fn initial_preparation_reserves_identity_before_blocking_factory_work() {
        let (host, factory) = host(2);
        factory.block_create.store(true, Ordering::Release);
        let session_id = SessionId("blocked-authorized-vault".to_owned());
        let readiness_published = Arc::new(AtomicBool::new(false));
        let preparation = tokio::spawn({
            let host = host.clone();
            let session_id = session_id.clone();
            let readiness_published = Arc::clone(&readiness_published);
            async move {
                let inspection_host = host.clone();
                let inspection_session = session_id.clone();
                host.prepare_session_after_reservation(
                    CreateSessionRequest {
                        session_id,
                        workspace: "workspace".to_owned(),
                        model: None,
                    },
                    false,
                    move || {
                        let registry = inspection_host
                            .registry
                            .try_lock()
                            .expect("reservation callback runs outside the registry lock");
                        assert!(matches!(
                            registry.sessions.get(&inspection_session),
                            Some(SessionSlot::Opening(_))
                        ));
                        readiness_published.store(true, Ordering::Release);
                    },
                )
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), factory.create_started.notified())
            .await
            .expect("session preparation entered the blocking credential/composition boundary");
        assert!(
            readiness_published.load(Ordering::Acquire),
            "authenticated readiness must publish after the initial reservation and before factory work"
        );

        let opening = {
            let registry = host.registry.lock().await;
            match registry.sessions.get(&session_id) {
                Some(SessionSlot::Opening(completed)) => completed.clone(),
                Some(SessionSlot::Ready(_)) | None => panic!("exact opening reservation"),
            }
        };
        let reconnect = tokio::spawn({
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
        .expect("early reconnect joined the initial opening");
        assert_eq!(
            factory.resumes.load(Ordering::Acquire),
            0,
            "the reconnect must not start a competing session resume"
        );

        factory.create_release.notify_one();
        preparation
            .await
            .expect("preparation task")
            .expect("prepared session");
        reconnect
            .await
            .expect("reconnect task")
            .expect("reconnect joined prepared session");
        assert!(host.session(&session_id).await.is_some());
        assert_eq!(factory.resumes.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn initial_resume_publishes_readiness_only_after_reserving_reconnect_identity() {
        let (host, factory) = host(2);
        factory.block_resume.store(true, Ordering::Release);
        let session_id = SessionId("blocked-initial-resume".to_owned());
        let readiness_published = Arc::new(AtomicBool::new(false));
        let preparation = tokio::spawn({
            let host = host.clone();
            let session_id = session_id.clone();
            let readiness_published = Arc::clone(&readiness_published);
            async move {
                let inspection_host = host.clone();
                let inspection_session = session_id.clone();
                host.prepare_session_after_reservation(
                    CreateSessionRequest {
                        session_id,
                        workspace: "workspace".to_owned(),
                        model: None,
                    },
                    true,
                    move || {
                        let registry = inspection_host
                            .registry
                            .try_lock()
                            .expect("reservation callback runs outside the registry lock");
                        assert!(matches!(
                            registry.sessions.get(&inspection_session),
                            Some(SessionSlot::Opening(_))
                        ));
                        readiness_published.store(true, Ordering::Release);
                    },
                )
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), factory.resume_started.notified())
            .await
            .expect("initial resume entered the blocking credential/composition boundary");
        assert!(readiness_published.load(Ordering::Acquire));

        let reconnect = tokio::spawn({
            let host = host.clone();
            let session_id = session_id.clone();
            async move { host.resume_session(&session_id).await }
        });
        let opening = {
            let registry = host.registry.lock().await;
            match registry.sessions.get(&session_id) {
                Some(SessionSlot::Opening(completed)) => completed.clone(),
                Some(SessionSlot::Ready(_)) | None => panic!("initial resume reservation"),
            }
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while opening.receiver_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reconnect joined the initial resume reservation");
        assert_eq!(
            factory.resumes.load(Ordering::Acquire),
            1,
            "the reconnect must join the initial resume instead of opening a competitor"
        );

        factory.resume_release.notify_one();
        preparation
            .await
            .expect("preparation task")
            .expect("prepared resumed session");
        reconnect
            .await
            .expect("reconnect task")
            .expect("reconnect joined resumed session");
        assert!(host.session(&session_id).await.is_some());
        assert_eq!(factory.resumes.load(Ordering::Acquire), 1);
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
            Arc::new(StubQueries::default()),
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
                        provider: None,
                    },
                )
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), sink.append_started.notified())
            .await
            .expect("model append blocked");
        assert_eq!(session.descriptor().model, ModelAlias("fast".to_owned()));
        assert!(
            !switch.is_finished(),
            "model-switch acceptance must wait for the durable event and project preference"
        );
        sink.release();
        assert_eq!(switch.await.expect("switch task"), CommandOutcome::Accepted);
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
