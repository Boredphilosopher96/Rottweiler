//! Read responses own their byte admission through transport buffer release.

use super::{BoundClient, DedupeRegistry, DedupeState, EngineHost, HostError, rejected};
use bytes::Bytes;
use rw_types::{
    ClientCommand, ClientId, CommandExecution, CommandOutcome, CommandReply, EngineEvent,
    EngineEventDelivery, MAX_CLIENT_READS, MAX_COMMAND_REPLY_BYTES, RequestId,
};
use std::{
    collections::HashMap,
    io::{self, Write},
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_ACTIVE_READS: usize = 8;
const REPLY_BYTE_UNIT: usize = 1024;
const MAX_RETAINED_REPLY_UNITS: usize = 32 * 1024;

#[derive(Debug)]
pub(super) struct ReadAdmission {
    global: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    clients: Mutex<HashMap<ClientId, Weak<Semaphore>>>,
}
impl Default for ReadAdmission {
    fn default() -> Self {
        Self {
            global: Arc::new(Semaphore::new(MAX_ACTIVE_READS)),
            bytes: Arc::new(Semaphore::new(MAX_RETAINED_REPLY_UNITS)),
            clients: Mutex::new(HashMap::new()),
        }
    }
}
struct ReadLease {
    _global: OwnedSemaphorePermit,
    bytes: OwnedSemaphorePermit,
    identity: Option<ReadIdentityLease>,
    _client: OwnedSemaphorePermit,
}
struct ReadIdentityLease {
    ledger: Arc<Mutex<DedupeRegistry>>,
    key: (ClientId, RequestId),
    limit: usize,
}
impl Drop for ReadIdentityLease {
    fn drop(&mut self) {
        let mut ledger = self
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(DedupeState::Read { active, .. }) = ledger.entries.get_mut(&self.key) {
            *active -= 1;
        }
        super::trim_dedupe(&mut ledger, self.limit);
    }
}

impl ReadAdmission {
    fn acquire(&self, client: &ClientId) -> Result<ReadLease, HostError> {
        let global = Arc::clone(&self.global)
            .try_acquire_owned()
            .map_err(|_| HostError::Query("host read admission exhausted".into()))?;
        let bytes = Arc::clone(&self.bytes)
            .try_acquire_many_owned(
                u32::try_from(MAX_COMMAND_REPLY_BYTES / REPLY_BYTE_UNIT)
                    .map_err(|_| HostError::Query("read byte reservation is invalid".into()))?,
            )
            .map_err(|_| HostError::Query("host read byte admission exhausted".into()))?;
        let owner = {
            let mut clients = self
                .clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            clients.retain(|_, owner| owner.strong_count() > 0);
            clients
                .entry(client.clone())
                .or_default()
                .upgrade()
                .unwrap_or_else(|| {
                    let owner = Arc::new(Semaphore::new(MAX_CLIENT_READS));
                    clients.insert(client.clone(), Arc::downgrade(&owner));
                    owner
                })
        };
        let client = owner
            .try_acquire_owned()
            .map_err(|_| HostError::Query("client read admission exhausted".into()))?;
        Ok(ReadLease {
            _global: global,
            bytes,
            identity: None,
            _client: client,
        })
    }
}

/// Decoded query payload and the resource owner that admitted its construction.
/// The owner remains alive until serialization transfers the payload to reply-byte admission.
pub struct HostReadResult {
    outcome: CommandOutcome,
    events: Vec<EngineEvent>,
    owner: Box<dyn Send>,
}
impl HostReadResult {
    #[must_use]
    pub fn new(
        outcome: CommandOutcome,
        events: Vec<EngineEvent>,
        owner: impl Send + 'static,
    ) -> Self {
        Self {
            outcome,
            events,
            owner: Box::new(owner),
        }
    }
    #[must_use]
    pub fn outcome(&self) -> &CommandOutcome {
        &self.outcome
    }
    #[must_use]
    pub fn events(&self) -> &[EngineEvent] {
        &self.events
    }
}
impl std::fmt::Debug for HostReadResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostReadResult")
            .field("outcome", &self.outcome)
            .field("events", &self.events)
            .finish_non_exhaustive()
    }
}

/// Encoded direct reply. Cloning the bytes retains the same admission lease.
#[derive(Debug)]
pub struct HostReply {
    pub outcome: CommandOutcome,
    pub bytes: Bytes,
}
struct LeasedBytes {
    bytes: ReplyBuffer,
    _lease: Option<ReadLease>,
}
impl AsRef<[u8]> for LeasedBytes {
    fn as_ref(&self) -> &[u8] {
        match &self.bytes {
            ReplyBuffer::Owned(bytes) => bytes,
            ReplyBuffer::Static(bytes) => bytes,
        }
    }
}
enum ReplyBuffer {
    Owned(Vec<u8>),
    Static(&'static [u8]),
}

impl HostReply {
    fn encode(reply: CommandReply, lease: Option<ReadLease>) -> Self {
        let mut writer = ReplyWriter(Vec::new());
        if serde_json::to_writer(&mut writer, &reply).is_err() {
            drop(writer);
            let read = matches!(reply, CommandReply::Read { .. });
            drop(reply);
            let outcome = rejected("reply_limit", "reply exceeds the encoded byte limit");
            let bytes: &'static [u8] = if read {
                br#"{"type":"read","outcome":{"type":"rejected","error":{"category":"protocol","code":"reply_limit","message":"reply exceeds the encoded byte limit","retryable":false,"details":null}},"events":[]}"#
            } else {
                br#"{"type":"command","outcome":{"type":"rejected","error":{"category":"protocol","code":"reply_limit","message":"reply exceeds the encoded byte limit","retryable":false,"details":null}}}"#
            };
            return Self::from_buffer(outcome, ReplyBuffer::Static(bytes), lease);
        }
        let outcome = match reply {
            CommandReply::Command { outcome } | CommandReply::Read { outcome, .. } => outcome,
        };
        Self::from_buffer(outcome, ReplyBuffer::Owned(writer.0), lease)
    }
    fn from_buffer(
        outcome: CommandOutcome,
        bytes: ReplyBuffer,
        mut lease: Option<ReadLease>,
    ) -> Self {
        if let Some(lease) = &mut lease {
            let retained = match &bytes {
                ReplyBuffer::Owned(bytes) => bytes.capacity().div_ceil(REPLY_BYTE_UNIT),
                ReplyBuffer::Static(_) => 0,
            };
            let release = lease.bytes.num_permits().saturating_sub(retained);
            drop(lease.bytes.split(release));
        }
        Self {
            outcome,
            bytes: Bytes::from_owner(LeasedBytes {
                bytes,
                _lease: lease,
            }),
        }
    }
    /// Build a bounded control acknowledgement for restricted transport capabilities.
    #[must_use]
    pub fn command(outcome: CommandOutcome) -> Self {
        Self::encode(CommandReply::Command { outcome }, None)
    }
}
struct ReplyWriter(Vec<u8>);
impl Write for ReplyWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = self
            .0
            .len()
            .checked_add(bytes.len())
            .filter(|length| *length <= MAX_COMMAND_REPLY_BYTES)
            .ok_or_else(|| io::Error::other("reply limit"))?;
        if length > self.0.capacity() {
            let target = self
                .0
                .capacity()
                .max(4096)
                .saturating_mul(2)
                .max(length)
                .min(MAX_COMMAND_REPLY_BYTES);
            self.0
                .try_reserve_exact(target - self.0.len())
                .map_err(io::Error::other)?;
            if self.0.capacity() > MAX_COMMAND_REPLY_BYTES {
                return Err(io::Error::other("reply allocation limit"));
            }
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl EngineHost {
    /// Dispatch through the source-owned read/control class under authenticated identity.
    pub async fn dispatch(&self, bound: BoundClient, mut command: ClientCommand) -> HostReply {
        command.meta_mut().client_id = bound.client_id.clone();
        if command.meta().protocol_version != rw_types::PROTOCOL_VERSION
            || !command.meta().request_id.is_valid()
        {
            let outcome = rejected(
                "command_metadata",
                "unsupported protocol or invalid request identity",
            );
            return HostReply::encode(
                match command.execution() {
                    CommandExecution::Read => CommandReply::Read {
                        outcome,
                        events: Vec::new(),
                    },
                    CommandExecution::Control => CommandReply::Command { outcome },
                },
                None,
            );
        }
        match command.execution() {
            CommandExecution::Control => {
                HostReply::command(self.dispatch_control(bound, command).await)
            }
            CommandExecution::Read => {
                self.read_channel
                    .dispatch(bound, command, |command| async {
                        self.execute_inner(command)
                            .await
                            .map(|(outcome, _, events)| HostReadResult::new(outcome, events, ()))
                    })
                    .await
            }
        }
    }
}

/// Read-only dispatch capability with bounded identity, concurrency and response-byte ownership.
#[derive(Clone)]
pub struct HostReadChannel {
    dedupe: Arc<Mutex<DedupeRegistry>>,
    admission: Arc<ReadAdmission>,
    max_retained_ids: usize,
}
impl HostReadChannel {
    /// Construct a standalone read channel; it cannot dispatch mutations.
    ///
    /// # Errors
    /// Rejects a zero identity capacity.
    pub fn new(max_retained_ids: usize) -> Result<Self, HostError> {
        if max_retained_ids == 0 {
            return Err(HostError::Protocol(
                "read identity capacity must be positive".into(),
            ));
        }
        Ok(Self::shared(
            Arc::new(Mutex::new(DedupeRegistry::default())),
            max_retained_ids,
        ))
    }

    pub(super) fn shared(dedupe: Arc<Mutex<DedupeRegistry>>, max_retained_ids: usize) -> Self {
        Self {
            dedupe,
            admission: Arc::new(ReadAdmission::default()),
            max_retained_ids,
        }
    }

    /// Admit one source-classified read before invoking its backend. The returned
    /// bytes retain the identity and allocation leases through transport release.
    pub async fn dispatch<F, R>(
        &self,
        bound: BoundClient,
        mut command: ClientCommand,
        query: F,
    ) -> HostReply
    where
        F: FnOnce(ClientCommand) -> R,
        R: std::future::Future<Output = Result<HostReadResult, HostError>>,
    {
        command.meta_mut().client_id = bound.client_id.clone();
        if command.execution() != CommandExecution::Read {
            return HostReply::command(rejected(
                "read_only",
                "read channel cannot execute controls",
            ));
        }
        if command.meta().protocol_version != rw_types::PROTOCOL_VERSION
            || !command.meta().request_id.is_valid()
        {
            return HostReply::encode(
                CommandReply::Read {
                    outcome: rejected(
                        "command_metadata",
                        "unsupported protocol or invalid request identity",
                    ),
                    events: Vec::new(),
                },
                None,
            );
        }
        let mut lease = match self.admission.acquire(&bound.client_id) {
            Ok(lease) => lease,
            Err(error) => {
                return HostReply::encode(
                    CommandReply::Read {
                        outcome: rejected("read_busy", &error.to_string()),
                        events: Vec::new(),
                    },
                    None,
                );
            }
        };
        let identity = command_hash(&command);
        let Ok(hash) = identity else {
            return HostReply::encode(
                CommandReply::Read {
                    outcome: rejected("read_identity", "read identity cannot serialize"),
                    events: Vec::new(),
                },
                Some(lease),
            );
        };
        let key = (bound.client_id, command.meta().request_id.clone());
        let conflict = self.claim_identity(key, hash, &mut lease);
        if conflict {
            return HostReply::encode(
                CommandReply::Read {
                    outcome: rejected(
                        "request_id_conflict",
                        "request id was reused with a different operation",
                    ),
                    events: Vec::new(),
                },
                Some(lease),
            );
        }
        let result = match query(command).await {
            Ok(result)
                if result
                    .events
                    .iter()
                    .all(|event| event.delivery() == EngineEventDelivery::Connection) =>
            {
                result
            }
            Ok(_) => HostReadResult::new(
                rejected("read_delivery", "read produced non-query events"),
                Vec::new(),
                (),
            ),
            Err(error) => HostReadResult::new(
                rejected(super::host_error_code(&error), &error.to_string()),
                Vec::new(),
                (),
            ),
        };
        let HostReadResult {
            outcome,
            events,
            owner,
        } = result;
        let reply = HostReply::encode(CommandReply::Read { outcome, events }, Some(lease));
        // The decoded payload is gone; encoded bytes now retain the host allocation lease.
        drop(owner);
        reply
    }

    fn claim_identity(
        &self,
        key: (ClientId, RequestId),
        hash: String,
        lease: &mut ReadLease,
    ) -> bool {
        let mut ledger = self
            .dedupe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let conflict = match ledger.entries.get(&key) {
            Some(DedupeState::Read { payload_hash, .. }) => payload_hash != &hash,
            Some(_) => true,
            None => false,
        };
        if !conflict {
            if let Some(DedupeState::Read { active, .. }) = ledger.entries.get_mut(&key) {
                *active += 1;
            } else {
                ledger.entries.insert(
                    key.clone(),
                    DedupeState::Read {
                        payload_hash: hash,
                        active: 1,
                    },
                );
                ledger.order.push_back(key.clone());
            }
            lease.identity = Some(ReadIdentityLease {
                ledger: Arc::clone(&self.dedupe),
                key,
                limit: self.max_retained_ids,
            });
            super::trim_dedupe(&mut ledger, self.max_retained_ids);
        }
        conflict
    }
}

pub(super) fn command_hash(command: &ClientCommand) -> Result<String, serde_json::Error> {
    let mut hasher = blake3::Hasher::new();
    serde_json::to_writer(&mut hasher, command)?;
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod channel_tests;

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn reply_writer_never_grows_beyond_its_owned_byte_budget() {
        let mut writer = ReplyWriter(Vec::new());
        for _ in 0..MAX_COMMAND_REPLY_BYTES / 1024 {
            writer.write_all(&[b'x'; 1024]).expect("admitted byte");
            assert!(writer.0.capacity() <= MAX_COMMAND_REPLY_BYTES);
        }
        assert!(writer.write_all(b"x").is_err());
        assert_eq!(writer.0.len(), MAX_COMMAND_REPLY_BYTES);
    }

    #[test]
    fn aggregate_byte_reservations_follow_actual_retained_buffers() {
        let admission = ReadAdmission::default();
        let mut bodies = Vec::new();
        for index in 0..4 {
            let lease = admission
                .acquire(&ClientId(format!("large-{index}")))
                .expect("byte reservation");
            let reply = HostReply::encode(
                CommandReply::Read {
                    outcome: rejected("large", &"x".repeat(7 * 1024 * 1024)),
                    events: Vec::new(),
                },
                Some(lease),
            );
            bodies.push(reply.bytes);
        }
        assert!(admission.acquire(&ClientId("over-budget".into())).is_err());
        let clone = bodies[0].clone();
        bodies.remove(0);
        assert!(admission.acquire(&ClientId("still-owned".into())).is_err());
        drop(clone);
        let lease = admission
            .acquire(&ClientId("small".into()))
            .expect("released bytes");
        let small = HostReply::encode(
            CommandReply::Read {
                outcome: CommandOutcome::Accepted {},
                events: Vec::new(),
            },
            Some(lease),
        );
        assert!(
            admission.bytes.available_permits() >= 8 * 1024 - 8,
            "small body releases unused reservation"
        );
        drop(small);
    }

    #[test]
    fn encoded_limit_rejects_with_the_original_reply_class() {
        let error = rejected("oversized", &"x".repeat(MAX_COMMAND_REPLY_BYTES));
        for reply in [
            CommandReply::Command {
                outcome: error.clone(),
            },
            CommandReply::Read {
                outcome: error,
                events: Vec::new(),
            },
        ] {
            let read = matches!(reply, CommandReply::Read { .. });
            let encoded = HostReply::encode(reply, None);
            assert!(encoded.bytes.len() < 1024);
            let decoded: CommandReply =
                serde_json::from_slice(&encoded.bytes).expect("valid fallback");
            assert_eq!(matches!(decoded, CommandReply::Read { .. }), read);
            assert_eq!(
                decoded.outcome(),
                &encoded.outcome,
                "static failure body follows the typed contract"
            );
            assert!(
                matches!(encoded.outcome, CommandOutcome::Rejected { error } if error.code == "reply_limit")
            );
        }
    }
}
