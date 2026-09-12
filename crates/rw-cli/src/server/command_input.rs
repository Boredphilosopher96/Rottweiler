//! Bounded transport allocation before typed command admission.
use super::{COMMAND_BODY_LIMIT, ClientCommand, ClientId};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(crate) const LANE_HEADER: &str = "x-rottweiler-command-lane";
const UNIT: usize = 1024;
const URGENT_BODY_LIMIT: usize = 64 * 1024;
const MAX_JSON_NODES: usize = 16 * 1024;

#[derive(Debug)]
pub(super) struct CommandIngress {
    normal: Lane,
    urgent: Lane,
}
#[derive(Debug)]
struct Lane {
    slots: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    clients: Mutex<HashMap<ClientId, Weak<Semaphore>>>,
    client_count: usize,
}
impl Default for CommandIngress {
    fn default() -> Self {
        Self {
            normal: Lane::new(
                16,
                96 * 1024,
                rw_types::MAX_CLIENT_CONTROLS + rw_types::MAX_CLIENT_READS,
            ),
            urgent: Lane::new(8, 4 * 1024, 2),
        }
    }
}
impl Lane {
    fn new(count: usize, units: usize, client_count: usize) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(count)),
            bytes: Arc::new(Semaphore::new(units)),
            clients: Mutex::default(),
            client_count,
        }
    }
    fn client(&self, id: &ClientId) -> Arc<Semaphore> {
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clients.retain(|_, owner| owner.strong_count() > 0);
        if let Some(owner) = clients.get(id).and_then(Weak::upgrade) {
            return owner;
        }
        let owner = Arc::new(Semaphore::new(self.client_count));
        clients.insert(id.clone(), Arc::downgrade(&owner));
        owner
    }
}
#[derive(Debug)]
pub(super) enum AdmissionError {
    InvalidLane,
    BodyLimit,
    Busy,
}

pub(super) struct InputLease {
    slot: OwnedSemaphorePermit,
    bytes: OwnedSemaphorePermit,
    pool: Arc<Semaphore>,
    client: OwnedSemaphorePermit,
    pub(super) limit: usize,
    pub(super) urgent: bool,
}
impl CommandIngress {
    pub(super) fn acquire(
        &self,
        client: &ClientId,
        lane: &str,
        length: Option<usize>,
    ) -> Result<InputLease, AdmissionError> {
        let (owner, limit, urgent) = match lane {
            "normal" => (&self.normal, COMMAND_BODY_LIMIT, false),
            "urgent" => (&self.urgent, URGENT_BODY_LIMIT, true),
            _ => return Err(AdmissionError::InvalidLane),
        };
        if length.is_some_and(|length| length > limit) {
            return Err(AdmissionError::BodyLimit);
        }
        let wire = length.unwrap_or(limit);
        // The borrowed preflight owns only the wire body and string scratch.
        // Typed allocation credit is acquired from its source-derived shape.
        let units = wire.saturating_mul(2).saturating_add(4096).div_ceil(UNIT);
        let slot = owner
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| AdmissionError::Busy)?;
        let bytes = owner
            .bytes
            .clone()
            .try_acquire_many_owned(u32::try_from(units).map_err(|_| AdmissionError::Busy)?)
            .map_err(|_| AdmissionError::Busy)?;
        let client = owner
            .client(client)
            .try_acquire_owned()
            .map_err(|_| AdmissionError::Busy)?;
        Ok(InputLease {
            slot,
            bytes,
            pool: Arc::clone(&owner.bytes),
            client,
            limit: wire,
            urgent,
        })
    }
}

pub(super) struct ParsedCommand {
    pub(super) command: ClientCommand,
    pub(super) lease: RetainedInput,
}
/// Decoding concurrency ends before semantic dispatch. The retained command
/// allocation remains charged until the dispatch future relinquishes it.
pub(super) struct RetainedInput {
    _bytes: OwnedSemaphorePermit,
}
#[derive(Debug, Eq, PartialEq)]
pub(super) enum DecodeError {
    Busy,
    Invalid(&'static str),
}
pub(super) async fn decode(
    bytes: hyper::body::Bytes,
    mut lease: InputLease,
) -> Result<ParsedCommand, DecodeError> {
    tokio::task::spawn_blocking(move || {
        use rw_types::allocation::{AllocationPlan, PrepareAllocation as _};
        use rw_types::json_structure::{JsonStructureLimits, preflight_json};
        let shape = preflight_json(
            &bytes,
            JsonStructureLimits {
                max_encoded_bytes: lease.limit,
                max_nodes: MAX_JSON_NODES,
                max_string_bytes: lease.limit,
                max_depth: 64,
            },
        )
        .map_err(|_| DecodeError::Invalid("command JSON shape exceeds its limits"))?;
        let decode_bytes = shape
            .decode_bytes::<ClientCommand>()
            .filter(|bytes| *bytes <= 64 * 1024 * 1024)
            .ok_or(DecodeError::Invalid(
                "command decode allocation exceeds its limit",
            ))?;
        let required = bytes
            .len()
            .checked_add(decode_bytes)
            .ok_or(DecodeError::Invalid("command allocation overflow"))?
            .div_ceil(UNIT);
        let additional = required.saturating_sub(lease.bytes.num_permits());
        if additional > 0 {
            let extra = lease
                .pool
                .clone()
                .try_acquire_many_owned(u32::try_from(additional).map_err(|_| DecodeError::Busy)?)
                .map_err(|_| DecodeError::Busy)?;
            lease.bytes.merge(extra);
        }
        let command: ClientCommand = serde_json::from_slice(&bytes)
            .map_err(|_| DecodeError::Invalid("command body is not valid protocol JSON"))?;
        if command.is_urgent() != lease.urgent {
            return Err(DecodeError::Invalid(
                "command does not belong to its declared lane",
            ));
        }
        let retained_limit = if lease.urgent {
            URGENT_BODY_LIMIT
        } else {
            COMMAND_BODY_LIMIT
        };
        if command
            .prepared_bytes()
            .is_none_or(|bytes| bytes > retained_limit)
        {
            return Err(DecodeError::Invalid(
                "command retained allocation exceeds its limit",
            ));
        }
        let plan = AllocationPlan::new(command)
            .map_err(|_| DecodeError::Invalid("command retained allocation is invalid"))?;
        let retained = plan.bytes().div_ceil(UNIT);
        let command = plan.prepare().into_inner();
        drop(bytes);
        let unused = lease.bytes.num_permits().saturating_sub(retained);
        drop(lease.bytes.split(unused));
        let retained = RetainedInput {
            _bytes: lease.bytes,
        };
        drop(lease.slot);
        drop(lease.client);
        Ok(ParsedCommand {
            command,
            lease: retained,
        })
    })
    .await
    .map_err(|_| DecodeError::Invalid("command decoder failed"))?
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    #[test]
    fn transport_admits_the_clients_combined_read_and_control_window() {
        let ingress = CommandIngress::default();
        let client = ClientId("client".into());
        let limit = rw_types::MAX_CLIENT_CONTROLS + rw_types::MAX_CLIENT_READS;
        let admitted: Vec<_> = (0..limit)
            .map(|_| {
                ingress
                    .acquire(&client, "normal", Some(128))
                    .expect("semantic window")
            })
            .collect();
        assert!(matches!(
            ingress.acquire(&client, "normal", Some(128)),
            Err(AdmissionError::Busy)
        ));
        drop(admitted);
        assert_eq!(ingress.normal.slots.available_permits(), 16);
    }
    #[test]
    fn normal_input_saturation_preserves_urgent_credit() {
        let ingress = CommandIngress::default();
        let client = ClientId("client".into());
        let input = ingress
            .acquire(&client, "normal", None)
            .expect("large input");
        let second = ingress
            .acquire(&ClientId("second".into()), "normal", None)
            .expect("second large input");
        assert!(
            ingress
                .acquire(&ClientId("other".into()), "normal", None)
                .is_err()
        );
        let urgent = ingress
            .acquire(&client, "urgent", Some(1024))
            .expect("urgent is independent");
        drop(input);
        drop(second);
        drop(urgent);
        ingress
            .acquire(&client, "normal", None)
            .expect("credits returned");
    }
    #[tokio::test]
    async fn decoded_input_releases_decoder_slots_but_retains_allocation() {
        let ingress = CommandIngress::default();
        let client = ClientId("client".into());
        let body = hyper::body::Bytes::from_static(br#"{"type":"shutdown_host","meta":{"protocol_version":1,"client_id":"client","request_id":"request"}}"#);
        let lease = ingress
            .acquire(&client, "urgent", Some(body.len()))
            .expect("input");
        let parsed = decode(body, lease).await.expect("valid urgent input");
        assert!(parsed.command.is_urgent());
        let second = ingress
            .acquire(&client, "urgent", Some(1))
            .expect("second input");
        let third = ingress
            .acquire(&client, "urgent", Some(1))
            .expect("dispatch does not own a decoder slot");
        assert!(ingress.acquire(&client, "urgent", Some(1)).is_err());
        drop(second);
        drop(third);
        assert!(ingress.urgent.bytes.available_permits() < 4 * 1024);
        drop(parsed);
        assert_eq!(ingress.urgent.bytes.available_permits(), 4 * 1024);
        assert!(matches!(
            ingress.acquire(&client, "urgent", Some(URGENT_BODY_LIMIT + 1)),
            Err(AdmissionError::BodyLimit)
        ));
    }

    #[tokio::test]
    async fn decoding_rejects_wrong_lane_and_excessive_shape() {
        let ingress = CommandIngress::default();
        let client = ClientId("client".into());
        let body = hyper::body::Bytes::from_static(br#"{"type":"shutdown_host","meta":{"protocol_version":1,"client_id":"client","request_id":"request"}}"#);
        let lease = ingress
            .acquire(&client, "normal", Some(body.len()))
            .expect("input");
        assert!(matches!(
            decode(body, lease).await,
            Err(DecodeError::Invalid(
                "command does not belong to its declared lane"
            ))
        ));
        let body = hyper::body::Bytes::from(format!("[{}0]", "0,".repeat(MAX_JSON_NODES)));
        let lease = ingress
            .acquire(&client, "normal", Some(body.len()))
            .expect("input");
        assert!(decode(body, lease).await.is_err());
    }

    #[tokio::test]
    async fn typed_decode_requires_extra_credit_before_constructing_command() {
        let ingress = CommandIngress::default();
        let client = ClientId("client".into());
        let body = hyper::body::Bytes::from_static(br#"{"type":"shutdown_host","meta":{"protocol_version":1,"client_id":"client","request_id":"request"}}"#);
        let lease = ingress
            .acquire(&client, "urgent", Some(body.len()))
            .expect("wire admission");
        let occupied = ingress
            .urgent
            .bytes
            .clone()
            .try_acquire_many_owned(
                u32::try_from(ingress.urgent.bytes.available_permits()).expect("bounded pool"),
            )
            .expect("other admitted work");
        assert!(matches!(
            decode(body.clone(), lease).await,
            Err(DecodeError::Busy)
        ));
        drop(occupied);
        let lease = ingress
            .acquire(&client, "urgent", Some(body.len()))
            .expect("returned wire credit");
        assert!(decode(body, lease).await.is_ok());
    }
}
