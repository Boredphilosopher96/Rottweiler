//! Canonical append sizing and encoding before the exclusive writer performs I/O.
use super::{EVENT_SCHEMA_VERSION, EventEnvelope, MAX_SEGMENT_BYTES, SessionStoreError};
use rw_types::{SequenceId, json_encoding::JsonWriter};
use serde::Serialize;
use std::io::Write as _;

/// Allocation-free plan for an immutable ordered batch of event payloads.
#[derive(Clone, Copy, Debug)]
pub struct JournalAppendPlan {
    first: u64,
    next: u64,
    count: usize,
    bytes: usize,
}

impl JournalAppendPlan {
    /// Measures canonical envelopes without allocating their encoded buffer.
    ///
    /// # Errors
    /// Rejects sequence overflow, serialization errors and oversized encoded batches.
    pub fn measure<T: Serialize>(
        first: SequenceId,
        events: &[T],
    ) -> Result<Self, SessionStoreError> {
        let count = u64::try_from(events.len()).map_err(|_| SessionStoreError::SequenceOverflow)?;
        let next = first
            .0
            .checked_add(count)
            .ok_or(SessionStoreError::SequenceOverflow)?;
        let mut output = JsonWriter::count(MAX_SEGMENT_BYTES);
        encode_records(first.0, events, &mut output)?;
        Ok(Self {
            first: first.0,
            next,
            count: events.len(),
            bytes: output.written(),
        })
    }

    /// Requested allocation of the canonical byte buffer. Payload owners and
    /// their retained allocations require separate admission.
    #[must_use]
    pub const fn encoded_bytes(&self) -> usize {
        self.bytes
    }

    /// Allocates and encodes after the caller reserves `encoded_bytes()`.
    ///
    /// # Errors
    /// Rejects changed batch length/encoding and serialization errors. The output
    /// cannot grow beyond the measured reservation even for a stateful serializer.
    pub fn encode<T: Serialize + rw_types::allocation::DecodeAllocation>(
        self,
        events: &[T],
    ) -> Result<PreparedJournalAppend, SessionStoreError> {
        if events.len() != self.count {
            return Err(SessionStoreError::CorruptEvent(
                "prepared journal batch count changed",
            ));
        }
        let mut bytes = Vec::with_capacity(self.bytes);
        let mut output = JsonWriter::buffer(&mut bytes, self.bytes, 0)?;
        encode_records(self.first, events, &mut output)?;
        if output.written() != self.bytes {
            return Err(SessionStoreError::CorruptEvent(
                "prepared journal batch encoding changed",
            ));
        }
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            super::decode::preflight_record::<T>(line)?;
        }
        Ok(PreparedJournalAppend {
            first: self.first,
            next: self.next,
            count: self.count,
            bytes,
        })
    }
}

/// Immutable canonical bytes, complete envelopes, and the expected writer prefix.
#[derive(Debug)]
pub struct PreparedJournalAppend {
    pub(super) first: u64,
    pub(super) next: u64,
    pub(super) count: usize,
    pub(super) bytes: Vec<u8>,
}

impl PreparedJournalAppend {
    #[must_use]
    pub fn encoded_bytes(&self) -> usize {
        self.bytes.len()
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.bytes.capacity()
    }
}

fn encode_records<T: Serialize>(
    first: u64,
    events: &[T],
    output: &mut JsonWriter<'_>,
) -> Result<(), SessionStoreError> {
    for (offset, event) in events.iter().enumerate() {
        let offset = u64::try_from(offset).map_err(|_| SessionStoreError::SequenceOverflow)?;
        let sequence = first
            .checked_add(offset)
            .ok_or(SessionStoreError::SequenceOverflow)?;
        let envelope = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence: SequenceId(sequence),
            event,
        };
        write_envelope(&envelope, output)?;
    }
    Ok(())
}

fn write_envelope<T: Serialize>(
    envelope: &EventEnvelope<T>,
    output: &mut JsonWriter<'_>,
) -> Result<(), SessionStoreError> {
    let result = output.serialize(envelope);
    if output.exceeded() {
        return Err(SessionStoreError::EventRecordTooLarge {
            max_line_bytes: output.limit(),
        });
    }
    result?;
    output
        .write_all(b"\n")
        .map_err(|_| SessionStoreError::EventRecordTooLarge {
            max_line_bytes: output.limit(),
        })?;
    Ok(())
}

pub(super) fn encode_owned<T: Serialize + rw_types::allocation::DecodeAllocation>(
    first: u64,
    events: impl IntoIterator<Item = T>,
) -> Result<(PreparedJournalAppend, Vec<EventEnvelope<T>>), SessionStoreError> {
    let mut bytes = Vec::new();
    let mut envelopes = Vec::new();
    for event in events {
        let offset =
            u64::try_from(envelopes.len()).map_err(|_| SessionStoreError::SequenceOverflow)?;
        let sequence = first
            .checked_add(offset)
            .ok_or(SessionStoreError::SequenceOverflow)?;
        let envelope = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence: SequenceId(sequence),
            event,
        };
        let start = bytes.len();
        write_envelope(
            &envelope,
            &mut JsonWriter::buffer(&mut bytes, MAX_SEGMENT_BYTES, 0)?,
        )?;
        super::decode::preflight_record::<T>(&bytes[start..])?;
        envelopes.push(envelope);
    }
    let count = u64::try_from(envelopes.len()).map_err(|_| SessionStoreError::SequenceOverflow)?;
    let next = first
        .checked_add(count)
        .ok_or(SessionStoreError::SequenceOverflow)?;
    Ok((
        PreparedJournalAppend {
            first,
            next,
            count: envelopes.len(),
            bytes,
        },
        envelopes,
    ))
}

#[cfg(test)]
mod tests;
