//! Shared event encoding with byte ownership through the final transport frame.
use super::{Arc, ClientSubscriptionLease, EngineEvent, HostError, SequenceId};
use bytes::Bytes;
use rw_types::allocation::{AllocationPlan, PrepareAllocation};
use std::io::{self, Write};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const UNIT: usize = 1024;
const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// An encoded protocol event. Every byte clone retains its allocation credit.
#[derive(Clone, Debug)]
pub struct HostEvent {
    pub json: Bytes,
    pub sequence: Option<SequenceId>,
}

impl HostEvent {
    pub(super) fn for_subscription(self, lease: &Arc<ClientSubscriptionLease>) -> Self {
        Self {
            sequence: self.sequence,
            json: Bytes::from_owner(SubscriptionBytes {
                json: self.json,
                _lease: Arc::clone(lease),
            }),
        }
    }
}
struct SubscriptionBytes {
    json: Bytes,
    _lease: Arc<ClientSubscriptionLease>,
}
impl AsRef<[u8]> for SubscriptionBytes {
    fn as_ref(&self) -> &[u8] {
        &self.json
    }
}

/// Host-wide owner for event encoding and all retained transport payloads.
#[derive(Clone, Debug)]
pub struct HostEventBudget {
    bytes: Arc<Semaphore>,
    encoders: Arc<Semaphore>,
}
impl Default for HostEventBudget {
    fn default() -> Self {
        Self {
            bytes: Arc::new(Semaphore::new(96 * 1024)),
            encoders: Arc::new(Semaphore::new(4)),
        }
    }
}
impl HostEventBudget {
    /// Reserve before copying or encoding; a dropped caller leaves its worker owned.
    ///
    /// # Errors
    /// Rejects exhausted admission, oversized events, and encoding failures.
    pub async fn encode(&self, event: &EngineEvent) -> Result<HostEvent, HostError> {
        let retained = event
            .prepared_bytes()
            .filter(|size| *size <= MAX_EVENT_BYTES)
            .ok_or_else(limit_error)?;
        let output_limit = retained
            .saturating_mul(6)
            .saturating_add(4096)
            .min(MAX_EVENT_BYTES);
        let units = retained
            .saturating_add(output_limit)
            .saturating_mul(2)
            .div_ceil(UNIT);
        let encoder = tokio::time::timeout(
            super::HOST_EVENT_STALL_TIMEOUT,
            self.encoders.clone().acquire_owned(),
        )
        .await
        .map_err(|_| limit_error())?
        .map_err(|_| limit_error())?;
        let mut credit = self
            .bytes
            .clone()
            .try_acquire_many_owned(u32::try_from(units).map_err(|_| limit_error())?)
            .map_err(|_| limit_error())?;
        let event = AllocationPlan::new(event.clone())
            .map_err(|_| limit_error())?
            .prepare();
        rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || {
            let _encoder = encoder;
            let sequence = event.value().meta().map(|meta| meta.sequence_id);
            let mut writer = EventWriter {
                bytes: Vec::new(),
                limit: output_limit,
            };
            serde_json::to_writer(&mut writer, event.value()).map_err(|_| limit_error())?;
            drop(event);
            let unused = credit
                .num_permits()
                .saturating_sub(writer.bytes.capacity().div_ceil(UNIT));
            drop(credit.split(unused));
            Ok(HostEvent {
                json: Bytes::from_owner(EventBytes {
                    bytes: writer.bytes,
                    _credit: credit,
                }),
                sequence,
            })
        })
        .await
        .map_err(|_| limit_error())?
    }
}
fn limit_error() -> HostError {
    HostError::Protocol(
        "host event allocation admission exhausted or event exceeds its limit".into(),
    )
}
struct EventBytes {
    bytes: Vec<u8>,
    _credit: OwnedSemaphorePermit,
}
impl AsRef<[u8]> for EventBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
struct EventWriter {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for EventWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|size| *size <= self.limit)
            .ok_or_else(|| io::Error::other("event encoding limit"))?;
        if length > self.bytes.capacity() {
            let capacity = self
                .bytes
                .capacity()
                .max(1024)
                .saturating_mul(2)
                .max(length)
                .min(self.limit);
            self.bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .map_err(io::Error::other)?;
            if self.bytes.capacity() > self.limit {
                return Err(io::Error::other("event allocation limit"));
            }
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn event() -> EngineEvent {
        EngineEvent::HostShutdown {
            meta: crate::CommandAckMeta {
                protocol_version: crate::PROTOCOL_VERSION,
                client_id: crate::ClientId("client".into()),
                request_id: crate::RequestId("shutdown".into()),
                emitted_at: "2026-01-01T00:00:00Z".into(),
            },
        }
    }

    #[tokio::test]
    async fn final_byte_clone_owns_credit_after_all_event_wrappers_drop() {
        let budget = HostEventBudget::default();
        let capacity = budget.bytes.available_permits();
        let event = budget.encode(&event()).await.expect("encoded");
        let retained = budget.bytes.available_permits();
        assert!(retained < capacity);
        let clone = event.clone();
        assert_eq!(clone.json.as_ptr(), event.json.as_ptr());
        let transport = clone.json.clone();
        drop(event);
        drop(clone);
        assert_eq!(budget.bytes.available_permits(), retained);
        let decoded: EngineEvent = serde_json::from_slice(&transport).expect("protocol JSON");
        assert!(matches!(decoded, EngineEvent::HostShutdown { .. }));
        drop(transport);
        assert_eq!(budget.bytes.available_permits(), capacity);
    }

    #[tokio::test]
    async fn exhausted_admission_rejects_before_encoding_and_recovers() {
        let budget = HostEventBudget::default();
        let credit = budget
            .bytes
            .clone()
            .acquire_many_owned(96 * 1024)
            .await
            .expect("reserve");
        assert!(budget.encode(&event()).await.is_err());
        assert_eq!(budget.encoders.available_permits(), 4);
        drop(credit);
        assert!(budget.encode(&event()).await.is_ok());
    }

    #[tokio::test]
    async fn transported_bytes_keep_subscription_slots_after_worker_exit() {
        let global = Arc::new(Semaphore::new(1));
        let client = Arc::new(Semaphore::new(1));
        let lease = Arc::new(ClientSubscriptionLease {
            _global: global.clone().acquire_owned().await.expect("global slot"),
            _client: client.clone().acquire_owned().await.expect("client slot"),
        });
        let event = HostEventBudget::default()
            .encode(&event())
            .await
            .expect("encoded")
            .for_subscription(&lease);
        let transport = event.json.clone();
        drop(event);
        drop(lease);
        assert_eq!(global.available_permits(), 0);
        assert_eq!(client.available_permits(), 0);
        drop(transport);
        assert_eq!(global.available_permits(), 1);
        assert_eq!(client.available_permits(), 1);
    }
}
