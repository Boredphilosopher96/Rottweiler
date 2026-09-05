//! Shared event encoding with byte ownership through the final transport frame.
use super::{Arc, EngineEvent, HostError, SequenceId};
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
        let encoder = self
            .encoders
            .clone()
            .try_acquire_owned()
            .map_err(|_| limit_error())?;
        let mut credit = self
            .bytes
            .clone()
            .try_acquire_many_owned(u32::try_from(units).map_err(|_| limit_error())?)
            .map_err(|_| limit_error())?;
        let event = AllocationPlan::new(event.clone())
            .map_err(|_| limit_error())?
            .prepare();
        tokio::task::spawn_blocking(move || {
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
