//! Immediate typed control admission with isolated cancellation capacity.
use rw_types::{ClientCommand, ClientId, SessionId};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const UNIT: usize = 1024;

#[derive(Debug)]
pub(super) struct ControlAdmission {
    normal: Lane,
    urgent: Lane,
}

#[derive(Debug)]
struct Lane {
    count: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    client_count: usize,
    session_count: usize,
    single_bytes: usize,
    clients: Mutex<HashMap<ClientId, Weak<Semaphore>>>,
    sessions: Mutex<HashMap<SessionId, Weak<Semaphore>>>,
}

#[derive(Debug)]
pub(super) struct ControlLease {
    _count: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
    _client: OwnedSemaphorePermit,
    _session: Option<OwnedSemaphorePermit>,
}

impl Default for ControlAdmission {
    fn default() -> Self {
        Self {
            normal: Lane::new(64, 32 * 1024 * 1024, 8, 8, 8 * 1024 * 1024),
            urgent: Lane::new(8, 1024 * 1024, 2, 2, 64 * 1024),
        }
    }
}

impl ControlAdmission {
    pub(super) fn acquire(
        &self,
        command: &ClientCommand,
        bytes: usize,
    ) -> Result<ControlLease, &'static str> {
        let lane = if is_urgent(command) {
            &self.urgent
        } else {
            &self.normal
        };
        lane.acquire(command, bytes)
    }
}

pub(super) fn is_urgent(command: &ClientCommand) -> bool {
    matches!(
        command,
        ClientCommand::Interrupt { .. }
            | ClientCommand::InterruptSubagent { .. }
            | ClientCommand::CancelProviderAuth { .. }
            | ClientCommand::ApproveTool { .. }
            | ClientCommand::ApprovePlan { .. }
            | ClientCommand::ShutdownHost { .. }
    )
}

impl Lane {
    fn new(
        count: usize,
        bytes: usize,
        client_count: usize,
        session_count: usize,
        single_bytes: usize,
    ) -> Self {
        Self {
            count: Arc::new(Semaphore::new(count)),
            bytes: Arc::new(Semaphore::new(bytes / UNIT)),
            client_count,
            session_count,
            single_bytes,
            clients: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn acquire(&self, command: &ClientCommand, bytes: usize) -> Result<ControlLease, &'static str> {
        if bytes > self.single_bytes {
            return Err("command retained allocation exceeds its byte limit");
        }
        let count = self
            .count
            .clone()
            .try_acquire_owned()
            .map_err(|_| "host control count exhausted")?;
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(
                u32::try_from(bytes.div_ceil(UNIT)).map_err(|_| "command allocation overflow")?,
            )
            .map_err(|_| "host control bytes exhausted")?;
        let client = owner(&self.clients, &command.meta().client_id, self.client_count)
            .try_acquire_owned()
            .map_err(|_| "client control count exhausted")?;
        let session = command
            .session_id()
            .map(|session| {
                owner(&self.sessions, session, self.session_count)
                    .try_acquire_owned()
                    .map_err(|_| "session control count exhausted")
            })
            .transpose()?;
        Ok(ControlLease {
            _count: count,
            _bytes: bytes,
            _client: client,
            _session: session,
        })
    }
}

fn owner<K: Clone + Eq + std::hash::Hash>(
    registry: &Mutex<HashMap<K, Weak<Semaphore>>>,
    key: &K,
    count: usize,
) -> Arc<Semaphore> {
    let mut registry = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.retain(|_, value| value.strong_count() > 0);
    if let Some(owner) = registry.get(key).and_then(Weak::upgrade) {
        return owner;
    }
    let owner = Arc::new(Semaphore::new(count));
    registry.insert(key.clone(), Arc::downgrade(&owner));
    owner
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use rw_types::{CommandMeta, RequestId};

    fn command(client: &str, session: &str) -> ClientCommand {
        ClientCommand::RenameSession {
            meta: CommandMeta {
                protocol_version: rw_types::PROTOCOL_VERSION,
                client_id: ClientId(client.into()),
                request_id: RequestId("request".into()),
            },
            session_id: SessionId(session.into()),
            title: "title".into(),
        }
    }

    #[test]
    fn saturation_preserves_cancellation_and_releases_every_scope() {
        let admission = ControlAdmission::default();
        let normal = command("client", "session");
        let leases: Vec<_> = (0..8)
            .map(|_| admission.acquire(&normal, 1024).expect("slot"))
            .collect();
        assert_eq!(
            admission
                .acquire(&normal, 1024)
                .expect_err("saturated scope"),
            "client control count exhausted"
        );
        assert_eq!(
            admission
                .acquire(&command("other", "session"), 1024)
                .expect_err("saturated scope"),
            "session control count exhausted"
        );
        let urgent = ClientCommand::Interrupt {
            meta: normal.meta().clone(),
            session_id: normal.session_id().expect("session command").clone(),
        };
        let cancellation = admission
            .acquire(&urgent, 1024)
            .expect("isolated cancellation");
        drop(leases);
        admission
            .acquire(&normal, 1024)
            .expect("normal capacity released");
        drop(cancellation);
        assert_eq!(admission.normal.count.available_permits(), 64);
        assert_eq!(admission.normal.bytes.available_permits(), 32 * 1024);
    }

    #[test]
    fn aggregate_bytes_bound_independent_clients_and_sessions() {
        let admission = ControlAdmission::default();
        let leases: Vec<_> = (0..4)
            .map(|n| {
                admission
                    .acquire(
                        &command(&format!("c{n}"), &format!("s{n}")),
                        8 * 1024 * 1024,
                    )
                    .expect("bytes")
            })
            .collect();
        assert_eq!(
            admission
                .acquire(&command("another", "another"), 1024)
                .expect_err("saturated scope"),
            "host control bytes exhausted"
        );
        drop(leases);
        admission
            .acquire(&command("another", "another"), 8 * 1024 * 1024)
            .expect("bytes released");
    }
}
