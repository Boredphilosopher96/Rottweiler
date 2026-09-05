//! Canonical append sizing and encoding before the exclusive writer performs I/O.
use super::{EVENT_SCHEMA_VERSION, EventEnvelope, MAX_SEGMENT_BYTES, SessionStoreError};
use rw_types::SequenceId;
use serde::Serialize;
use std::io::{self, Write};

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
        let mut output = Output {
            destination: Count,
            written: 0,
            limit: MAX_SEGMENT_BYTES,
            exceeded: false,
        };
        encode_records(first.0, events, &mut output)?;
        Ok(Self {
            first: first.0,
            next,
            count: events.len(),
            bytes: output.written,
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
        let mut output = Output {
            destination: Vec::with_capacity(self.bytes),
            written: 0,
            limit: self.bytes,
            exceeded: false,
        };
        encode_records(self.first, events, &mut output)?;
        if output.written != self.bytes {
            return Err(SessionStoreError::CorruptEvent(
                "prepared journal batch encoding changed",
            ));
        }
        for line in output.destination.split_inclusive(|byte| *byte == b'\n') {
            super::decode::preflight_record::<T>(line)?;
        }
        Ok(PreparedJournalAppend {
            first: self.first,
            next: self.next,
            count: self.count,
            bytes: output.destination,
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

fn encode_records<T: Serialize, W: Write>(
    first: u64,
    events: &[T],
    output: &mut Output<W>,
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

fn write_envelope<T: Serialize, W: Write>(
    envelope: &EventEnvelope<T>,
    output: &mut Output<W>,
) -> Result<(), SessionStoreError> {
    let result = serde_json::to_writer(&mut *output, envelope);
    if output.exceeded {
        return Err(SessionStoreError::EventRecordTooLarge {
            max_line_bytes: output.limit,
        });
    }
    result?;
    output
        .write_all(b"\n")
        .map_err(|_| SessionStoreError::EventRecordTooLarge {
            max_line_bytes: output.limit,
        })?;
    Ok(())
}

struct Output<W> {
    destination: W,
    written: usize,
    limit: usize,
    exceeded: bool,
}
impl<W: Write> Write for Output<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.written {
            self.exceeded = true;
            return Err(io::Error::other(
                "journal batch exceeds its byte reservation",
            ));
        }
        self.destination.write_all(bytes)?;
        self.written += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.destination.flush()
    }
}
struct Count;
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn encode_owned<T: Serialize + rw_types::allocation::DecodeAllocation>(
    first: u64,
    events: impl IntoIterator<Item = T>,
) -> Result<(PreparedJournalAppend, Vec<EventEnvelope<T>>), SessionStoreError> {
    let mut output = Output {
        destination: Vec::new(),
        written: 0,
        limit: MAX_SEGMENT_BYTES,
        exceeded: false,
    };
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
        let start = output.written;
        write_envelope(&envelope, &mut output)?;
        super::decode::preflight_record::<T>(&output.destination[start..])?;
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
            bytes: output.destination,
        },
        envelopes,
    ))
}

#[cfg(test)]
mod tests;
