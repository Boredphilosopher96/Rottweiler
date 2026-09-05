//! Bounded transport allocation before typed command admission.
use super::{COMMAND_BODY_LIMIT, ClientCommand, ClientId};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(crate) const LANE_HEADER: &str = "x-rottweiler-command-lane";
const UNIT: usize = 1024;
const URGENT_BODY_LIMIT: usize = 64 * 1024;
const MAX_JSON_NODES: usize = 16 * 1024;
pub(super) const BODY_TIMEOUT: Duration = Duration::from_secs(3);

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
}
impl Default for CommandIngress {
    fn default() -> Self {
        Self {
            normal: Lane::new(16, 64 * 1024),
            urgent: Lane::new(8, 4 * 1024),
        }
    }
}
impl Lane {
    fn new(count: usize, units: usize) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(count)),
            bytes: Arc::new(Semaphore::new(units)),
            clients: Mutex::default(),
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
        let owner = Arc::new(Semaphore::new(2));
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
    _slot: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
    _client: OwnedSemaphorePermit,
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
        // Collection and parser scratch can coexist with the owned typed tree.
        // The shape preflight bounds collection/object bookkeeping independently
        // of a payload's encoded strings before typed decoding begins.
        let nodes = (wire / 2).min(MAX_JSON_NODES);
        let units = wire
            .saturating_mul(3)
            .saturating_add(nodes * 128)
            .saturating_add(4096)
            .div_ceil(UNIT);
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
            _slot: slot,
            _bytes: bytes,
            _client: client,
            limit: wire,
            urgent,
        })
    }
}

pub(super) struct ParsedCommand {
    pub(super) command: ClientCommand,
    pub(super) lease: InputLease,
}
pub(super) async fn decode(
    bytes: hyper::body::Bytes,
    lease: InputLease,
) -> Result<ParsedCommand, &'static str> {
    tokio::task::spawn_blocking(move || {
        let mut remaining = MAX_JSON_NODES;
        let mut parser = serde_json::Deserializer::from_slice(&bytes);
        serde::de::DeserializeSeed::deserialize(Shape(&mut remaining), &mut parser)
            .map_err(|_| "command JSON shape exceeds its limits")?;
        parser
            .end()
            .map_err(|_| "command body is not valid protocol JSON")?;
        let command: ClientCommand = serde_json::from_slice(&bytes)
            .map_err(|_| "command body is not valid protocol JSON")?;
        if command.is_urgent() != lease.urgent {
            return Err("command does not belong to its declared lane");
        }
        use rw_types::allocation::PrepareAllocation as _;
        if command
            .prepared_bytes()
            .is_none_or(|bytes| bytes > COMMAND_BODY_LIMIT)
        {
            return Err("command retained allocation exceeds its limit");
        }
        Ok(ParsedCommand { command, lease })
    })
    .await
    .map_err(|_| "command decoder failed")?
}

struct Shape<'a>(&'a mut usize);
impl<'de> serde::de::DeserializeSeed<'de> for Shape<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        *self.0 = self
            .0
            .checked_sub(1)
            .ok_or_else(|| serde::de::Error::custom("command JSON node limit"))?;
        deserializer.deserialize_any(self)
    }
}
impl<'de> serde::de::Visitor<'de> for Shape<'_> {
    type Value = ();
    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("bounded command JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut values: A) -> Result<(), A::Error> {
        while values.next_element_seed(Shape(self.0))?.is_some() {}
        Ok(())
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut values: A) -> Result<(), A::Error> {
        while values.next_key_seed(Shape(self.0))?.is_some() {
            values.next_value_seed(Shape(self.0))?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    #[test]
    fn normal_input_saturation_preserves_urgent_credit() {
        let ingress = CommandIngress::default();
        let client = ClientId("client".into());
        let input = ingress
            .acquire(&client, "normal", None)
            .expect("large input");
        assert!(
            ingress
                .acquire(&ClientId("other".into()), "normal", None)
                .is_err()
        );
        let urgent = ingress
            .acquire(&client, "urgent", Some(1024))
            .expect("urgent is independent");
        drop(input);
        drop(urgent);
        ingress
            .acquire(&client, "normal", None)
            .expect("credits returned");
    }
    #[tokio::test]
    async fn valid_urgent_input_retains_and_releases_client_admission() {
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
        assert!(ingress.acquire(&client, "urgent", Some(1)).is_err());
        drop(parsed);
        ingress
            .acquire(&client, "urgent", Some(1))
            .expect("parsed input returned credit");
        drop(second);
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
            Err("command does not belong to its declared lane")
        ));
        let body = hyper::body::Bytes::from(format!("[{}0]", "0,".repeat(MAX_JSON_NODES)));
        let lease = ingress
            .acquire(&client, "normal", Some(body.len()))
            .expect("input");
        assert!(decode(body, lease).await.is_err());
    }
}
