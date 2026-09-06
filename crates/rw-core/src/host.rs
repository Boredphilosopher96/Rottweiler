mod contracts;
pub use contracts::*;
mod commands;
mod control;
mod control_admission;
mod control_completion;
mod control_owner;
mod event_frame;
mod events;
pub use event_frame::{HostEvent, HostEventBudget};
mod lifecycle;
mod operation_receipts;
mod provider_completion;
mod retained_control;
use retained_control::RetainedDispatch;
mod read;
pub use read::{HostReadChannel, HostReadResult, HostReply};

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    future::Future,
    path::Path,
    pin::Pin,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use futures_util::future::join_all;
use rw_types::{
    ClientCommand, ClientId, ClientRole, CommandAckMeta, CommandDescriptor, CommandMeta,
    CommandOutcome, EngineError, EngineErrorCategory, EngineEvent, McpApprovalReview,
    McpEnvironmentEntry, McpServerDescriptor, ModeDescriptor, ModelAlias, ModelCatalogSnapshot,
    ProviderAuthAttemptId, ProviderAuthChallenge, RequestId, RuntimeServiceDescriptor, SequenceId,
    SessionDescriptor, SessionId, ShellId, SubagentDescriptor, SubagentId, TranscriptFormat,
    TurnId, WorkspaceDiff, WorkspaceFileMatch, WorkspaceFilePreview, WorkspaceStatus,
};
use thiserror::Error;
use tokio::sync::{mpsc, watch};

use crate::{
    AgentLoopError, BuiltinProviderId, BuiltinProviderProfile, CachedModelCatalog, EventClock,
    ProviderApiKey, SessionHandle, SystemEventClock, store_provider_api_key,
};

const HOST_EVENT_CAPACITY: usize = 256;
const HOST_EVENT_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
const MAX_WIRE_COMMANDS: usize = 512;
const MAX_WIRE_COMMAND_CATALOG_BYTES: usize = 48 * 1024;
const MAX_WIRE_MODES: usize = 128;
const MAX_WIRE_MODE_CATALOG_BYTES: usize = 64 * 1024;
const PROVIDER_AUTH_BEGIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
const PROVIDER_AUTH_COMPLETE_DEADLINE: std::time::Duration = std::time::Duration::from_mins(10);
const MAX_PROVIDER_AUTH_URL_BYTES: usize = 4_096;
const MAX_PROVIDER_AUTH_CODE_BYTES: usize = 256;
const MAX_PROVIDER_AUTH_WARNINGS: usize = 16;
const MAX_PROVIDER_AUTH_WARNING_BYTES: usize = 512;
const PROVIDER_AUTH_WARNINGS_OMITTED: &str =
    "provider credential was saved; some credential warnings were omitted";
const MAX_PROVIDER_AUTH_MESSAGE_BYTES: usize = 1_024;

#[derive(Debug)]
enum SessionSlot {
    Opening(watch::Sender<bool>),
    Ready(Arc<HostedSession>),
}

#[derive(Debug, Default)]
struct HostRegistry {
    sessions: HashMap<SessionId, SessionSlot>,
    shutdown_failure: Option<Arc<str>>,
}

#[derive(Clone, Debug)]
struct CachedDispatch {
    outcome: CommandOutcome,
    events: Vec<EngineEvent>,
    cacheable: bool,
}

#[derive(Debug)]
enum DedupeState {
    Read {
        payload_hash: String,
        active: usize,
    },
    Running {
        payload_hash: String,
        completion: watch::Sender<Option<Arc<RetainedDispatch>>>,
    },
    Complete {
        payload_hash: String,
        dispatch: Arc<RetainedDispatch>,
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ClientEventSubscriptionId(u64);

#[derive(Default)]
struct ClientEventSubscribers {
    next_id: u64,
    senders: HashMap<ClientEventSubscriptionId, mpsc::Sender<HostEvent>>,
}

struct ClientEventChannel {
    delivery: tokio::sync::Mutex<()>,
    subscribers: Mutex<ClientEventSubscribers>,
    slots: Arc<tokio::sync::Semaphore>,
}
impl Default for ClientEventChannel {
    fn default() -> Self {
        Self {
            delivery: tokio::sync::Mutex::default(),
            subscribers: Mutex::default(),
            slots: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }
}

struct ClientEventRegistry {
    clients: HashMap<ClientId, Arc<ClientEventChannel>>,
    slots: Arc<tokio::sync::Semaphore>,
}
impl Default for ClientEventRegistry {
    fn default() -> Self {
        Self {
            clients: HashMap::new(),
            slots: Arc::new(tokio::sync::Semaphore::new(64)),
        }
    }
}
struct ClientSubscriptionLease {
    _global: tokio::sync::OwnedSemaphorePermit,
    _client: tokio::sync::OwnedSemaphorePermit,
}

impl ClientEventChannel {
    fn subscribe(&self) -> (ClientEventSubscriptionId, mpsc::Receiver<HostEvent>) {
        let (sender, receiver) = mpsc::channel(HOST_EVENT_CAPACITY);
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = loop {
            let id = ClientEventSubscriptionId(subscribers.next_id);
            subscribers.next_id = subscribers.next_id.wrapping_add(1);
            if !subscribers.senders.contains_key(&id) {
                break id;
            }
        };
        subscribers.senders.insert(id, sender);
        (id, receiver)
    }

    fn unsubscribe(&self, id: ClientEventSubscriptionId) -> bool {
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        subscribers.senders.remove(&id);
        subscribers.senders.is_empty()
    }

    fn senders(&self) -> Vec<(ClientEventSubscriptionId, mpsc::Sender<HostEvent>)> {
        self.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .senders
            .iter()
            .map(|(id, sender)| (*id, sender.clone()))
            .collect()
    }
}

impl ClientEventRegistry {
    fn subscribe(
        &mut self,
        client_id: &ClientId,
    ) -> Result<
        (
            Arc<ClientEventChannel>,
            ClientEventSubscriptionId,
            mpsc::Receiver<HostEvent>,
            ClientSubscriptionLease,
        ),
        HostError,
    > {
        let global = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| HostError::Protocol("host subscription admission exhausted".into()))?;
        let channel = Arc::clone(
            self.clients
                .entry(client_id.clone())
                .or_insert_with(|| Arc::new(ClientEventChannel::default())),
        );
        let client =
            channel.slots.clone().try_acquire_owned().map_err(|_| {
                HostError::Protocol("client subscription admission exhausted".into())
            })?;
        let (id, receiver) = channel.subscribe();
        Ok((
            channel,
            id,
            receiver,
            ClientSubscriptionLease {
                _global: global,
                _client: client,
            },
        ))
    }

    fn unsubscribe(
        &mut self,
        client_id: &ClientId,
        channel: &Arc<ClientEventChannel>,
        id: ClientEventSubscriptionId,
    ) -> bool {
        let Some(registered) = self.clients.get(client_id) else {
            return false;
        };
        if !Arc::ptr_eq(registered, channel) || !channel.unsubscribe(id) {
            return false;
        }
        self.clients.remove(client_id);
        true
    }
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
    subscription_id: ClientEventSubscriptionId,
    receiver: mpsc::Receiver<HostEvent>,
    _lease: Arc<ClientSubscriptionLease>,
    channel: Arc<ClientEventChannel>,
    registry: Arc<Mutex<ClientEventRegistry>>,
    pending: Arc<PendingProviderAuths>,
}

impl Drop for ProviderAuthSubscriptionGuard {
    fn drop(&mut self) {
        let final_subscription = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unsubscribe(&self.client_id, &self.channel, self.subscription_id);
        if final_subscription {
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
    read_channel: HostReadChannel,
    control_admission: Arc<control_admission::ControlAdmission>,
    control_owner: Arc<control_owner::ControlOwner>,
    completion_budget: Arc<retained_control::CompletionBudget>,
    client_events: Arc<Mutex<ClientEventRegistry>>,
    event_budget: HostEventBudget,
    provider_auth: Arc<PendingProviderAuths>,
    provider_mutation: Arc<tokio::sync::Mutex<()>>,
    provider_api_key_store: Arc<ProviderApiKeyStore>,
    shutting_down: Arc<AtomicBool>,
    closure: Arc<lifecycle::HostClosure>,
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
        let dedupe = Arc::new(Mutex::new(DedupeRegistry::default()));
        let read_channel =
            HostReadChannel::shared(Arc::clone(&dedupe), config.max_deduplicated_requests);
        Ok(Self {
            config,
            factory,
            queries,
            clock: Arc::new(SystemEventClock),
            registry: Arc::new(tokio::sync::Mutex::new(HostRegistry::default())),
            dedupe,
            read_channel,
            control_admission: Arc::new(control_admission::ControlAdmission::default()),
            control_owner: Arc::default(),
            completion_budget: Arc::default(),
            client_events: Arc::new(Mutex::new(ClientEventRegistry::default())),
            event_budget: HostEventBudget::default(),
            provider_auth: Arc::new(PendingProviderAuths::default()),
            provider_mutation: Arc::new(tokio::sync::Mutex::new(())),
            provider_api_key_store: Arc::new(|provider, api_key| {
                store_provider_api_key(&provider, api_key)
                    .map_err(|_| HostError::Query("provider credential storage failed".to_owned()))
            }),
            shutting_down: Arc::new(AtomicBool::new(false)),
            closure: Arc::new(lifecycle::HostClosure::default()),
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
        HostError::ReplayCursorAhead => "replay_cursor_ahead",
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
        HostError::Protocol(_) | HostError::ReplayCursorAhead => {
            "provider authentication request was invalid"
        }
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
            source: descriptor.source(),
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

fn wire_mode_catalog(
    active: ModeDescriptor,
    descriptors: impl IntoIterator<Item = ModeDescriptor>,
) -> (Vec<ModeDescriptor>, bool) {
    debug_assert!(active.current);
    let mut modes = vec![active];
    let mut truncated = false;
    let mut serialized_bytes =
        serde_json::to_vec(&modes).map_or(MAX_WIRE_MODE_CATALOG_BYTES, |encoded| encoded.len());
    for mode in descriptors {
        if modes.len() >= MAX_WIRE_MODES {
            truncated = true;
            break;
        }
        let Ok(encoded) = serde_json::to_vec(&mode) else {
            truncated = true;
            break;
        };
        let Some(next_size) = serialized_bytes
            .checked_add(1)
            .and_then(|size| size.checked_add(encoded.len()))
        else {
            truncated = true;
            break;
        };
        if next_size > MAX_WIRE_MODE_CATALOG_BYTES {
            truncated = true;
            break;
        }
        serialized_bytes = next_size;
        modes.push(mode);
    }
    (modes, truncated)
}

#[cfg(test)]
mod tests;

fn trim_dedupe(ledger: &mut DedupeRegistry, limit: usize) {
    let mut scanned = 0;
    while ledger.entries.len() > limit && scanned < ledger.order.len() {
        let Some(key) = ledger.order.pop_front() else {
            break;
        };
        if matches!(ledger.entries.get(&key), Some(DedupeState::Running { .. }))
            || matches!(ledger.entries.get(&key), Some(DedupeState::Read { active, .. }) if *active > 0)
        {
            ledger.order.push_back(key);
            scanned += 1;
        } else {
            ledger.entries.remove(&key);
        }
    }
}
