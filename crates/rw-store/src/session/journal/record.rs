//! Indexed source reads reserve caller-owned decode space before materialization.
use super::{
    EventEnvelope, JournalReadMetrics, JournalReadView, MAX_JOURNAL_DECODE_BYTES,
    MAX_SEGMENT_BYTES, SequenceId, SessionEventPageLimits, SessionStoreError, decode,
    decode_page_event,
};
use rw_types::allocation::DecodeAllocation;
use serde::de::DeserializeOwned;

/// One exact source record and its independently measured read/decode costs.
#[derive(Debug)]
pub struct JournalRecord<T> {
    /// The requested authoritative envelope.
    pub envelope: EventEnvelope<T>,
    /// Structural admission charged before constructing the returned value.
    pub decode_bytes: usize,
    /// Actual containing-segment I/O and record inspection.
    pub metrics: JournalReadMetrics,
}

impl JournalReadView {
    /// Reads one sequence within the caller's remaining decoded allocation allowance.
    ///
    /// Locates and checksums only its containing segment. Encoded scratch is bounded
    /// by the journal's 16 MiB segment ceiling and is released before returning.
    /// No other record is deserialized. The caller must include retained values in
    /// its aggregate allowance when choosing `max_decode_bytes`.
    ///
    /// # Errors
    /// Rejects invalid allowances, absent sequences, corrupt source bytes and a
    /// structural charge exceeding the allowance, before deserializing the value.
    pub fn record_with_decode_limit<T: DeserializeOwned + DecodeAllocation>(
        &self,
        sequence: SequenceId,
        max_decode_bytes: usize,
    ) -> Result<JournalRecord<T>, SessionStoreError> {
        if max_decode_bytes == 0 || max_decode_bytes > MAX_JOURNAL_DECODE_BYTES {
            return Err(SessionStoreError::InvalidEventDecodeLimit);
        }
        if sequence.0 >= self.next_sequence {
            return Err(SessionStoreError::EventPageCursorAhead);
        }
        let first = self
            .segments
            .partition(self.segment_count, |segment| segment.next <= sequence.0);
        let (segment, active) =
            self.segments_from(first)
                .next()
                .ok_or(SessionStoreError::CorruptEvent(
                    "journal source segment is missing",
                ))?;
        if sequence.0 < segment.first || sequence.0 >= segment.next {
            return Err(SessionStoreError::CorruptEvent(
                "journal source index has a gap",
            ));
        }
        let mut metrics = JournalReadMetrics::default();
        let limits = SessionEventPageLimits {
            max_page_bytes: MAX_SEGMENT_BYTES as u64,
            max_page_events: 1,
            max_line_bytes: MAX_SEGMENT_BYTES,
            max_scan_bytes: MAX_SEGMENT_BYTES as u64,
            // Every record occupies at least one newline byte in this segment.
            max_scan_events: MAX_SEGMENT_BYTES as u64,
        };
        let (_file, bytes) = self.page_segment_bytes(&segment, active, limits, &mut metrics)?;
        let offset = usize::try_from(sequence.0 - segment.first)
            .map_err(|_| SessionStoreError::LimitOverflow)?;
        let line = bytes
            .split_inclusive(|byte| *byte == b'\n')
            .nth(offset)
            .ok_or(SessionStoreError::CorruptEvent(
                "journal source record is missing",
            ))?;
        let decode_bytes = decode::preflight_record::<T>(line)?;
        if decode_bytes > max_decode_bytes {
            return Err(SessionStoreError::EventDecodeLimitTooSmall {
                required_bytes: decode_bytes,
                max_bytes: max_decode_bytes,
            });
        }
        let envelope = decode_page_event(line, sequence.0)?;
        metrics.records_decoded = 1;
        Ok(JournalRecord {
            envelope,
            decode_bytes,
            metrics,
        })
    }
}

#[cfg(test)]
mod tests;
