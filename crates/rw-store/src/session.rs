//! Crash-safe append-only session logs and the derived `SQLite` session index.

use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{BufRead, BufReader, Read as _, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::FileExt as _;
#[cfg(not(unix))]
use std::{
    fs::OpenOptions,
    io::{Seek as _, SeekFrom},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use rw_types::{AccountingAttribution, Cost, SequenceId, TurnId, Usage};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tempfile::TempDir;
use thiserror::Error;

/// Current public JSONL envelope version for durable session events.
pub const SESSION_EVENT_SCHEMA_VERSION: u16 = 1;
const EVENT_SCHEMA_VERSION: u16 = SESSION_EVENT_SCHEMA_VERSION;
const MAX_SEARCH_INDEX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SEARCH_INDEX_WAL_BYTES: u64 = 64 * 1024 * 1024;
static INDEX_TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One versioned event in a session's public JSONL transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope<T> {
    /// Event-log schema version.
    pub schema_version: u16,
    /// Contiguous zero-based sequence within the session.
    pub sequence: SequenceId,
    /// Provider- and UI-neutral event payload.
    pub event: T,
}

/// Independent resource ceilings for one descriptor-stable event-log page.
///
/// Page limits bound retained output while scan limits bound the work required
/// to validate the complete snapshot and calculate truthful tail metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionEventPageLimits {
    /// Maximum serialized JSONL bytes retained in one returned page.
    pub max_page_bytes: u64,
    /// Maximum envelopes retained in one returned page.
    pub max_page_events: usize,
    /// Maximum bytes in one JSON record, excluding its newline delimiter.
    pub max_line_bytes: usize,
    /// Maximum descriptor snapshot size which may be scanned.
    pub max_scan_bytes: u64,
    /// Maximum envelopes which may be validated in one scan.
    pub max_scan_events: u64,
}

impl Default for SessionEventPageLimits {
    fn default() -> Self {
        Self {
            max_page_bytes: 8 * 1024 * 1024,
            max_page_events: 2_000,
            max_line_bytes: 16 * 1024 * 1024,
            max_scan_bytes: 512 * 1024 * 1024,
            max_scan_events: 1_000_000,
        }
    }
}

/// One validated page from a complete, stable event-log snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionEventPage<T> {
    /// Cursor-exclusive page contents in durable sequence order.
    pub events: Vec<EventEnvelope<T>>,
    /// Serialized JSONL bytes represented by `events`, including delimiters.
    pub page_bytes: u64,
    /// Cursor callers should pass to retrieve the next page. It remains equal
    /// to the input cursor when that cursor was already at the tail.
    pub next_cursor: Option<SequenceId>,
    /// Whether at least one validated event remains after `next_cursor`.
    pub has_more: bool,
    /// Number of validated events before the first returned event.
    pub events_before_page: u64,
    /// Number of validated events after `next_cursor`.
    pub events_after_page: u64,
    /// Total validated envelopes in the stable snapshot.
    pub total_events: u64,
    /// Exact byte length of the stable descriptor snapshot.
    pub total_bytes: u64,
    /// Tail sequence of the stable snapshot, or `None` for an empty log.
    pub tail_sequence: Option<SequenceId>,
}

/// Append-only event log for one session actor.
#[derive(Debug)]
pub struct SessionEventLog {
    path: PathBuf,
    next_sequence: u64,
    file: File,
    writer_state: SessionWriterState,
}

/// Whether an append can safely continue using the open writer descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionWriterState {
    Healthy,
    Poisoned,
}

impl SessionEventLog {
    /// Opens or creates `sessions/<id>/events.jsonl`, repairing an incomplete
    /// final record left by a killed writer.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe id, I/O failure, corrupt non-final record,
    /// schema mismatch, or non-contiguous sequence.
    pub fn open(root: &Path, session_id: &str) -> Result<Self, SessionStoreError> {
        validate_session_id(session_id)?;
        let directory = root.join("sessions").join(session_id);
        let path = directory.join("events.jsonl");

        #[cfg(unix)]
        let file = open_session_file(root, session_id)?;
        #[cfg(not(unix))]
        let file = open_session_file_portable(root, &directory, &path)?;

        ensure_regular_file(&file)?;
        lock_writer(&file)?;
        let next_sequence = recover_and_validate(&file)?;
        Ok(Self {
            path,
            next_sequence,
            file,
            writer_state: SessionWriterState::Healthy,
        })
    }

    /// Durable path of this session's event log.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sequence assigned to the next appended event.
    #[must_use]
    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Last persisted sequence, distinguishing an empty log from event zero.
    #[must_use]
    pub const fn last_sequence(&self) -> Option<SequenceId> {
        if self.next_sequence == 0 {
            None
        } else {
            Some(SequenceId(self.next_sequence - 1))
        }
    }

    /// Appends one event and synchronizes it before returning.
    ///
    /// # Errors
    ///
    /// Returns a serialization, sequence-overflow, or durable-write error.
    pub fn append<T: Serialize>(
        &mut self,
        event: T,
    ) -> Result<EventEnvelope<T>, SessionStoreError> {
        let mut appended = self.append_batch([event])?;
        appended.pop().ok_or(SessionStoreError::CorruptEvent(
            "append produced no persisted event",
        ))
    }

    /// Appends only when a caller's expected id is exactly the next id.
    ///
    /// This is the fail-closed adapter for engines which allocate the id before
    /// persistence. Prefer [`Self::append`] and broadcast its returned envelope
    /// when the log can be the sole authority.
    ///
    /// # Errors
    ///
    /// Returns a sequence mismatch before serializing or writing any bytes.
    pub fn append_expected<T: Serialize>(
        &mut self,
        expected: SequenceId,
        event: T,
    ) -> Result<EventEnvelope<T>, SessionStoreError> {
        if expected != SequenceId(self.next_sequence) {
            return Err(SessionStoreError::UnexpectedEventSequence {
                expected: SequenceId(self.next_sequence),
                actual: expected,
            });
        }
        self.append(event)
    }

    /// Serializes a batch before writing, then appends it under the existing
    /// writer lock and performs one durable synchronization. A failed append
    /// is rolled back to its original length before its error is returned.
    ///
    /// # Errors
    ///
    /// Returns a serialization, sequence-overflow, or durable-write error.
    pub fn append_batch<T: Serialize>(
        &mut self,
        events: impl IntoIterator<Item = T>,
    ) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        if self.writer_state == SessionWriterState::Poisoned {
            return Err(SessionStoreError::EventWriterPoisoned);
        }
        let events = events.into_iter().collect::<Vec<_>>();
        let count = u64::try_from(events.len()).map_err(|_| SessionStoreError::SequenceOverflow)?;
        self.next_sequence
            .checked_add(count)
            .ok_or(SessionStoreError::SequenceOverflow)?;
        let mut bytes = Vec::new();
        let mut envelopes = Vec::with_capacity(events.len());
        for (offset, event) in events.into_iter().enumerate() {
            let offset = u64::try_from(offset).map_err(|_| SessionStoreError::SequenceOverflow)?;
            let envelope = EventEnvelope {
                schema_version: EVENT_SCHEMA_VERSION,
                sequence: SequenceId(
                    self.next_sequence
                        .checked_add(offset)
                        .ok_or(SessionStoreError::SequenceOverflow)?,
                ),
                event,
            };
            serde_json::to_writer(&mut bytes, &envelope)?;
            bytes.push(b'\n');
            envelopes.push(envelope);
        }
        if bytes.is_empty() {
            return Ok(envelopes);
        }
        let pre_append_len = self.file.metadata()?.len();
        if let Err(append_error) = append_event_bytes(&mut self.file, &bytes) {
            return Err(self.rollback_failed_append(pre_append_len, append_error));
        }
        self.next_sequence += count;
        Ok(envelopes)
    }

    fn rollback_failed_append(
        &mut self,
        pre_append_len: u64,
        append_error: std::io::Error,
    ) -> SessionStoreError {
        match truncate_and_sync_event_file(&self.file, pre_append_len) {
            Ok(()) => SessionStoreError::Io(append_error),
            Err(rollback_error) => {
                self.writer_state = SessionWriterState::Poisoned;
                SessionStoreError::AppendRollbackFailed {
                    append: append_error,
                    rollback: rollback_error,
                }
            }
        }
    }

    /// Backward-compatible name for appending one durably synchronized turn.
    ///
    /// # Errors
    ///
    /// Returns a serialization, sequence-overflow, or durable-write error.
    pub fn append_turn<T: Serialize>(
        &mut self,
        events: impl IntoIterator<Item = T>,
    ) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        self.append_batch(events)
    }

    /// Loads every complete event after validating version and sequence.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O, JSON, schema, or sequence corruption.
    pub fn load<T: DeserializeOwned>(&self) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        load_events(&self.file)
    }

    /// Loads complete events strictly after a durable sequence cursor.
    ///
    /// The open writer has already validated the prefix. This path still reads
    /// one stable descriptor snapshot, but decodes only the requested suffix.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O, JSON, schema, sequence corruption, or a cursor
    /// ahead of the durable tail.
    pub fn load_after<T: DeserializeOwned>(
        &self,
        after_sequence: Option<SequenceId>,
    ) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        let bytes = read_opened_file(&self.file)?;
        let start_sequence = match after_sequence {
            Some(sequence) => sequence
                .0
                .checked_add(1)
                .ok_or(SessionStoreError::EventPageCursorAhead)?,
            None => 0,
        };
        if start_sequence > self.next_sequence {
            return Err(SessionStoreError::EventPageCursorAhead);
        }
        if start_sequence == self.next_sequence {
            return Ok(Vec::new());
        }
        let records_to_skip =
            usize::try_from(start_sequence).map_err(|_| SessionStoreError::EventPageCursorAhead)?;
        let mut skipped = 0_usize;
        let start = bytes
            .split_inclusive(|byte| *byte == b'\n')
            .take(records_to_skip)
            .try_fold(0_usize, |offset, record| {
                skipped = skipped
                    .checked_add(1)
                    .ok_or(SessionStoreError::LimitOverflow)?;
                offset
                    .checked_add(record.len())
                    .ok_or(SessionStoreError::LimitOverflow)
            })?;
        if skipped != records_to_skip {
            return Err(SessionStoreError::CorruptEvent(
                "event log is shorter than its durable tail",
            ));
        }
        parse_events_bounded_from_sequence(&bytes[start..], usize::MAX, start_sequence)
    }

    /// Loads an existing session log without acquiring the lifetime writer lock.
    ///
    /// This is the read-only boundary for host queries while the owning actor
    /// may still be active. A concurrent append is detected by the stable-file
    /// read and returned as an error rather than projecting a torn record.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe id or path, missing log, concurrent
    /// mutation, malformed record, schema mismatch, or non-contiguous sequence.
    pub fn load_existing<T: DeserializeOwned>(
        root: &Path,
        session_id: &str,
    ) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        validate_session_id(session_id)?;
        #[cfg(unix)]
        let file = open_existing_session_file(root, session_id)?;
        #[cfg(not(unix))]
        let file = open_existing_session_file_portable(root, session_id)?;
        ensure_regular_file(&file)?;
        load_events(&file)
    }

    /// Loads a read-only log under explicit byte and event-count limits.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe id or path, missing log, concurrent
    /// mutation, malformed record, schema mismatch, non-contiguous sequence,
    /// or either configured limit being exceeded.
    pub fn load_existing_bounded<T: DeserializeOwned>(
        root: &Path,
        session_id: &str,
        max_bytes: u64,
        max_events: usize,
    ) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        Self::load_existing_bounded_with_size(root, session_id, max_bytes, max_events)
            .map(|(events, _)| events)
    }

    /// Loads a bounded read-only log and returns the byte length observed from
    /// the same stable file descriptor used for parsing.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::load_existing_bounded`].
    pub fn load_existing_bounded_with_size<T: DeserializeOwned>(
        root: &Path,
        session_id: &str,
        max_bytes: u64,
        max_events: usize,
    ) -> Result<(Vec<EventEnvelope<T>>, u64), SessionStoreError> {
        validate_session_id(session_id)?;
        #[cfg(unix)]
        let file = open_existing_session_file(root, session_id)?;
        #[cfg(not(unix))]
        let file = open_existing_session_file_portable(root, session_id)?;
        ensure_regular_file(&file)?;
        load_events_bounded_with_size(&file, max_bytes, max_events)
    }

    /// Streams and validates an existing log while retaining only one bounded,
    /// cursor-exclusive page.
    ///
    /// Unlike the legacy bounded whole-log reader, the cursor is applied while
    /// scanning, so logs larger than one page remain readable. Every traversed
    /// envelope is still decoded and checked for schema and contiguous sequence;
    /// the complete stable snapshot is scanned to produce truthful total/tail
    /// metadata. A concurrent append or mutation fails the entire read.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe identity/path components, malformed or
    /// oversized records, invalid sequence/schema, an out-of-range cursor,
    /// exceeded page/scan limits, or concurrent descriptor mutation.
    pub fn load_existing_page<T: DeserializeOwned>(
        root: &Path,
        session_id: &str,
        after_sequence: Option<SequenceId>,
        limits: SessionEventPageLimits,
    ) -> Result<SessionEventPage<T>, SessionStoreError> {
        validate_session_id(session_id)?;
        #[cfg(unix)]
        let file = open_existing_session_file(root, session_id)?;
        #[cfg(not(unix))]
        let file = open_existing_session_file_portable(root, session_id)?;
        ensure_regular_file(&file)?;
        load_event_page_with_hook(&file, after_sequence, limits, || {})
    }

    /// Streams and validates an existing log while retaining only its most
    /// recent bounded page.
    ///
    /// This is the initial-inspection companion to [`Self::load_existing_page`]:
    /// it scans the same stable descriptor snapshot and validates every
    /// envelope, but keeps a rolling page ending at the durable tail instead of
    /// retaining the oldest page. Callers can therefore inspect a very large
    /// log without first transferring its entire history.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe identity/path components, malformed or
    /// oversized records, invalid sequence/schema, exceeded page/scan limits,
    /// or concurrent descriptor mutation.
    pub fn load_existing_tail_page<T: DeserializeOwned>(
        root: &Path,
        session_id: &str,
        limits: SessionEventPageLimits,
    ) -> Result<SessionEventPage<T>, SessionStoreError> {
        validate_session_id(session_id)?;
        #[cfg(unix)]
        let file = open_existing_session_file(root, session_id)?;
        #[cfg(not(unix))]
        let file = open_existing_session_file_portable(root, session_id)?;
        ensure_regular_file(&file)?;
        load_event_tail_page_with_hook(&file, limits, || {})
    }

    /// Creates or idempotently completes a child log from an exact parent
    /// prefix. The parent is opened read-only and never modified.
    ///
    /// An existing child is accepted only when it is an exact prefix of the
    /// requested fork, which makes recovery after a killed append safe while
    /// rejecting identity reuse or post-fork divergence.
    ///
    /// # Errors
    ///
    /// Returns an error for identical identities, a missing parent cursor, a
    /// conflicting child prefix, concurrent parent mutation, or durable I/O.
    pub fn fork(
        root: &Path,
        parent_session_id: &str,
        child_session_id: &str,
        through_sequence: Option<SequenceId>,
    ) -> Result<Self, SessionStoreError> {
        validate_session_id(parent_session_id)?;
        validate_session_id(child_session_id)?;
        if parent_session_id == child_session_id {
            return Err(SessionStoreError::ForkIdentityConflict);
        }
        #[cfg(unix)]
        let parent_file = open_existing_session_file(root, parent_session_id)?;
        #[cfg(not(unix))]
        let parent_file = open_existing_session_file_portable(root, parent_session_id)?;
        ensure_regular_file(&parent_file)?;
        let parent_bytes = read_opened_file(&parent_file)?;
        let parent_events = parse_events::<serde_json::Value>(&parent_bytes)?;
        let event_count = through_sequence.map_or(Ok(0), |through| {
            let index = usize::try_from(through.0).map_err(|_| SessionStoreError::LimitOverflow)?;
            if parent_events.get(index).map(|event| event.sequence) != Some(through) {
                return Err(SessionStoreError::ForkSourceCursorMissing);
            }
            index.checked_add(1).ok_or(SessionStoreError::LimitOverflow)
        })?;
        let target_len = parent_bytes
            .split_inclusive(|byte| *byte == b'\n')
            .take(event_count)
            .try_fold(0_usize, |total, line| {
                total
                    .checked_add(line.len())
                    .ok_or(SessionStoreError::LimitOverflow)
            })?;
        let target = &parent_bytes[..target_len];
        let mut child = Self::open(root, child_session_id)?;
        let existing = read_opened_file(&child.file)?;
        if existing.len() > target.len() || !target.starts_with(&existing) {
            return Err(SessionStoreError::ForkTargetConflict);
        }
        child.file.write_all(&target[existing.len()..])?;
        child.file.flush()?;
        sync_event_file(&child.file)?;
        child.next_sequence =
            u64::try_from(event_count).map_err(|_| SessionStoreError::SequenceOverflow)?;
        Ok(child)
    }

    /// Typed fork primitive which can rewrite payload-owned session identity
    /// while preserving envelope sequence and an exact durable prefix.
    ///
    /// `None` means the explicit empty prefix. Callers for a non-empty fork
    /// must resolve and pass the exact durable boundary sequence.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identities, a missing source boundary,
    /// conflicting child history, mapping failure, or durable storage I/O.
    pub fn fork_mapped<T, Map>(
        root: &Path,
        parent_session_id: &str,
        child_session_id: &str,
        through_sequence: Option<SequenceId>,
        map: Map,
    ) -> Result<Self, SessionStoreError>
    where
        T: DeserializeOwned + PartialEq + Serialize,
        Map: FnMut(T) -> Result<T, SessionStoreError>,
    {
        let parent = Self::load_existing::<T>(root, parent_session_id)?;
        Self::fork_mapped_loaded(
            root,
            parent_session_id,
            child_session_id,
            parent,
            through_sequence,
            map,
        )
    }

    /// Forks an already validated event vector without rereading the source log.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identities or cursors, mapper failures, an
    /// incompatible existing target prefix, or durable write failures.
    pub fn fork_mapped_loaded<T, Map>(
        root: &Path,
        parent_session_id: &str,
        child_session_id: &str,
        mut parent: Vec<EventEnvelope<T>>,
        through_sequence: Option<SequenceId>,
        mut map: Map,
    ) -> Result<Self, SessionStoreError>
    where
        T: DeserializeOwned + PartialEq + Serialize,
        Map: FnMut(T) -> Result<T, SessionStoreError>,
    {
        validate_session_id(parent_session_id)?;
        validate_session_id(child_session_id)?;
        if parent_session_id == child_session_id {
            return Err(SessionStoreError::ForkIdentityConflict);
        }
        if let Some(through_sequence) = through_sequence {
            let through_index = usize::try_from(through_sequence.0)
                .map_err(|_| SessionStoreError::LimitOverflow)?;
            if parent.get(through_index).map(|event| event.sequence) != Some(through_sequence) {
                return Err(SessionStoreError::ForkSourceCursorMissing);
            }
            parent.truncate(
                through_index
                    .checked_add(1)
                    .ok_or(SessionStoreError::LimitOverflow)?,
            );
        } else {
            parent.clear();
        }
        let parent = parent
            .into_iter()
            .map(|envelope| {
                Ok(EventEnvelope {
                    schema_version: envelope.schema_version,
                    sequence: envelope.sequence,
                    event: map(envelope.event)?,
                })
            })
            .collect::<Result<Vec<_>, SessionStoreError>>()?;

        let mut child = Self::open(root, child_session_id)?;
        let existing = child.load::<T>()?;
        if existing.len() > parent.len()
            || existing
                .iter()
                .zip(&parent)
                .any(|(child, parent)| child != parent)
        {
            return Err(SessionStoreError::ForkTargetConflict);
        }
        child.append_batch(
            parent
                .into_iter()
                .skip(existing.len())
                .map(|envelope| envelope.event),
        )?;
        Ok(child)
    }
}

/// One denormalized row in the session listing/search index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSummary {
    /// Stable session id.
    pub id: String,
    /// Current display title.
    pub title: String,
    /// Caller-supplied deterministic update time in Unix milliseconds.
    pub updated_unix_ms: i64,
    /// Accumulated ordinary micro-dollar equivalent, when applicable.
    pub cost_micros: i64,
}

/// One complete, rebuildable projection of a session event log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionProjection {
    /// Listing fields derived from the log.
    pub summary: SessionSummary,
    /// Searchable transcript derived from the same log prefix.
    pub transcript: String,
    /// Last event incorporated into this projection, or `None` for an empty log.
    pub projected_through: Option<SequenceId>,
}

/// Validated UTC calendar-day key used by accounting queries and projections.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UtcDayKey(String);

impl UtcDayKey {
    /// Parses an exact `YYYY-MM-DD` UTC day key.
    ///
    /// # Errors
    ///
    /// Returns [`SessionStoreError::InvalidAccountingTimestamp`] for malformed
    /// or impossible calendar dates.
    pub fn parse(value: impl Into<String>) -> Result<Self, SessionStoreError> {
        let value = value.into();
        validate_utc_day(&value)?;
        Ok(Self(value))
    }

    /// Returns the validated wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UtcDayKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Validated millisecond-precision UTC timestamp used as an injected budget
/// and spend-rate clock boundary.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UtcTimestamp(String);

impl UtcTimestamp {
    /// Parses an exact `YYYY-MM-DDTHH:MM:SS.mmmZ` UTC timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`SessionStoreError::InvalidAccountingTimestamp`] for malformed
    /// or impossible timestamps.
    pub fn parse(value: impl Into<String>) -> Result<Self, SessionStoreError> {
        let value = value.into();
        validate_utc_timestamp(&value)?;
        Ok(Self(value))
    }

    /// Converts milliseconds since the Unix epoch to the normalized UTC wire
    /// representation used by event metadata.
    ///
    /// # Errors
    ///
    /// Returns [`SessionStoreError::InvalidAccountingTimestamp`] when the
    /// instant is outside the four-digit year range supported by the schema.
    pub fn from_unix_millis(unix_millis: u64) -> Result<Self, SessionStoreError> {
        let seconds = unix_millis / 1_000;
        let millis = unix_millis % 1_000;
        let days = i64::try_from(seconds / 86_400)
            .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
        let second_of_day = seconds % 86_400;
        let (year, month, day) = civil_from_days(days);
        if !(1..=9_999).contains(&year) {
            return Err(SessionStoreError::InvalidAccountingTimestamp);
        }
        let hour = second_of_day / 3_600;
        let minute = (second_of_day % 3_600) / 60;
        let second = second_of_day % 60;
        Self::parse(format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
        ))
    }

    /// Returns the validated wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Derives the UTC calendar day from this already-validated timestamp.
    #[must_use]
    pub fn utc_day(&self) -> UtcDayKey {
        UtcDayKey(self.0[..10].to_owned())
    }
}

impl std::fmt::Display for UtcTimestamp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch.saturating_add(719_468);
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

/// One authoritative per-turn accounting fact projected from a durable
/// `TurnFinished` engine event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TurnAccountingEntry {
    /// Session whose turn incurred the cost or quota usage.
    pub session_id: String,
    /// Stable engine turn identifier.
    pub turn_id: TurnId,
    /// Sequence of the authoritative `TurnFinished` event.
    pub sequence_id: SequenceId,
    /// Normalized UTC event timestamp (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
    pub emitted_at_utc: UtcTimestamp,
    /// UTC calendar day (`YYYY-MM-DD`) derived from the injected event clock.
    pub utc_day: UtcDayKey,
    /// Runtime role which incurred this usage and cost.
    pub attribution: AccountingAttribution,
    /// Provider-normalized token usage for this accounting fact.
    pub usage: Usage,
    /// Provider-neutral accounting disposition. Subscription and unavailable
    /// values remain typed instead of becoming zero-cost monetary entries.
    pub cost: Cost,
}

/// Durable totals used by session and calendar-day budget decisions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountingTotals {
    /// Session selected by the query.
    pub session_id: String,
    /// UTC calendar day selected by the query.
    pub utc_day: UtcDayKey,
    /// Inclusive start of the selected UTC day.
    pub utc_day_start_utc: UtcTimestamp,
    /// Inclusive start of the injected trailing spend-rate window.
    pub trailing_window_start_utc: UtcTimestamp,
    /// Inclusive end of the injected trailing spend-rate window.
    pub trailing_window_end_utc: UtcTimestamp,
    /// All-time USD micro-cost for the selected session.
    pub session_micros_usd: u64,
    /// USD micro-cost across all sessions during the selected UTC day.
    pub day_micros_usd: u64,
    /// USD micro-cost for the selected session inside the trailing window.
    pub trailing_session_micros_usd: u64,
    /// USD micro-cost across all sessions inside the trailing window.
    pub trailing_all_sessions_micros_usd: u64,
    /// All-time AI-credit micro-units for the selected session.
    pub session_ai_credit_micros: u64,
    /// AI-credit micro-units across all sessions during the selected UTC day.
    pub day_ai_credit_micros: u64,
    /// AI-credit micro-units for the selected session inside the trailing window.
    pub trailing_session_ai_credit_micros: u64,
    /// AI-credit micro-units across all sessions inside the trailing window.
    pub trailing_all_sessions_ai_credit_micros: u64,
    /// Subscription-quota turns in the selected session.
    pub session_subscription_quota_turns: u64,
    /// Subscription-quota turns during the selected UTC day.
    pub day_subscription_quota_turns: u64,
    /// Cost-unavailable turns in the selected session.
    pub session_unavailable_turns: u64,
    /// Cost-unavailable turns during the selected UTC day.
    pub day_unavailable_turns: u64,
    /// Non-USD monetary turns retained for the selected session but excluded
    /// from USD caps.
    pub session_non_usd_monetary_turns: u64,
    /// Non-USD monetary turns during the selected UTC day.
    pub day_non_usd_monetary_turns: u64,
}

/// Rebuildable `SQLite` accounting projection. Session JSONL logs remain the
/// authority; every method is idempotent so startup reconciliation is safe.
#[derive(Clone, Debug)]
pub struct AccountingLedger {
    path: PathBuf,
}

impl AccountingLedger {
    /// Opens the shared derived index and installs the accounting schema when
    /// migrating an older M0-M2 index.
    ///
    /// # Errors
    ///
    /// Returns an I/O or `SQLite` schema error.
    pub fn open(root: &Path) -> Result<Self, SessionStoreError> {
        fs::create_dir_all(root)?;
        let ledger = Self {
            path: root.join("index.sqlite"),
        };
        ledger.connection()?;
        Ok(ledger)
    }

    /// Idempotently records one event-log-derived turn entry.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/time fields, conflicting event
    /// identities, serialization failure, or `SQLite` failure.
    pub fn record(&self, entry: &TurnAccountingEntry) -> Result<(), SessionStoreError> {
        validate_accounting_entry(entry)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        insert_accounting_entry(&transaction, entry)?;
        transaction.commit()?;
        Ok(())
    }

    /// Reconciles a projected event-log prefix without deleting rows written by
    /// concurrently active sessions.
    ///
    /// # Errors
    ///
    /// Returns an invalid-entry, conflict, serialization, or transaction error.
    pub fn reconcile(&self, entries: &[TurnAccountingEntry]) -> Result<(), SessionStoreError> {
        for entry in entries {
            validate_accounting_entry(entry)?;
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        for entry in entries {
            insert_accounting_entry(&transaction, entry)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Atomically replaces the accounting projection from authoritative event
    /// logs. Callers must first quiesce session writers; ordinary startup should
    /// prefer [`Self::reconcile`].
    ///
    /// # Errors
    ///
    /// Returns an invalid-entry, conflict, serialization, or transaction error.
    pub fn replace_all(&self, entries: &[TurnAccountingEntry]) -> Result<(), SessionStoreError> {
        for entry in entries {
            validate_accounting_entry(entry)?;
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM turn_accounting", [])?;
        for entry in entries {
            insert_accounting_entry(&transaction, entry)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Returns typed entries for one session in numeric event-sequence order.
    ///
    /// # Errors
    ///
    /// Returns an invalid-id, corrupt-row, JSON, or `SQLite` error.
    pub fn entries_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<TurnAccountingEntry>, SessionStoreError> {
        validate_session_id(session_id)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT session_id,turn_id,sequence_id,emitted_at_utc,utc_day,\
                    attribution_json,usage_json,cost_json \
             FROM turn_accounting WHERE session_id=?1 \
             ORDER BY length(sequence_id),sequence_id",
        )?;
        let rows = statement.query_map([session_id], accounting_entry_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(SessionStoreError::from)
    }

    /// Returns every typed entry in stable session and numeric sequence order.
    /// This is primarily useful for validating or copying a derived projection;
    /// rebuild callers should prefer entries projected directly from JSONL.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-row, JSON, or `SQLite` error.
    pub fn entries(&self) -> Result<Vec<TurnAccountingEntry>, SessionStoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT session_id,turn_id,sequence_id,emitted_at_utc,utc_day,\
                    attribution_json,usage_json,cost_json \
             FROM turn_accounting \
             ORDER BY session_id,length(sequence_id),sequence_id",
        )?;
        let rows = statement.query_map([], accounting_entry_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(SessionStoreError::from)
    }

    /// Reads a bounded UTC range from an existing accounting projection
    /// without opening the live database for writes or creating its schema.
    ///
    /// The database and any committed WAL are first copied through the same
    /// descriptor-stable snapshot boundary used by historical session search.
    /// Event logs remain authoritative; this surface is intended for
    /// read-only historical reporting over the continuously reconciled index.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or reversed UTC bounds, an unsafe or
    /// oversized index, a corrupt row, or a result larger than `max_entries`.
    pub fn entries_read_only_bounded(
        root: &Path,
        start_utc: &UtcTimestamp,
        end_utc: &UtcTimestamp,
        max_entries: usize,
    ) -> Result<Vec<TurnAccountingEntry>, SessionStoreError> {
        if start_utc > end_utc {
            return Err(SessionStoreError::InvalidAccountingTimestamp);
        }
        if max_entries > 1_000_000 {
            return Err(SessionStoreError::AccountingQueryLimitTooLarge);
        }
        let sql_limit = max_entries
            .checked_add(1)
            .ok_or(SessionStoreError::LimitOverflow)?;
        let sql_limit = i64::try_from(sql_limit).map_err(|_| SessionStoreError::LimitOverflow)?;
        let path = root.join("index.sqlite");
        let before = validate_read_only_index(&path)?;
        let canonical_root = fs::canonicalize(root)?;
        let canonical_path = fs::canonicalize(&path)?;
        if canonical_path.parent() != Some(canonical_root.as_path()) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        let snapshot = read_only_index_snapshot(&canonical_root, &before)?;
        let snapshot_path = fs::canonicalize(snapshot.path().join("index.sqlite"))?;
        let connection = Connection::open_with_flags(
            snapshot_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let after = validate_read_only_index(&path)?;
        if !same_file_identity(&before, &after) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        let mut statement = connection.prepare(
            "SELECT session_id,turn_id,sequence_id,emitted_at_utc,utc_day,\
                    attribution_json,usage_json,cost_json \
             FROM turn_accounting \
             WHERE emitted_at_utc>=?1 AND emitted_at_utc<=?2 \
             ORDER BY emitted_at_utc,session_id,length(sequence_id),sequence_id \
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![start_utc.as_str(), end_utc.as_str(), sql_limit],
            accounting_entry_from_row,
        )?;
        let mut entries = rows.collect::<Result<Vec<_>, _>>()?;
        if entries.len() > max_entries {
            return Err(SessionStoreError::AccountingResultTooLarge { max_entries });
        }
        entries.shrink_to_fit();
        Ok(entries)
    }

    /// Computes session, UTC-day, and trailing-window totals as of the injected
    /// window end. The caller supplies every boundary so replay never reads a
    /// clock and future-dated rows cannot affect a current budget decision.
    ///
    /// # Errors
    ///
    /// Returns an invalid query, corrupt-row, overflow, JSON, or `SQLite` error.
    #[allow(clippy::too_many_lines)]
    pub fn totals(
        &self,
        session_id: &str,
        utc_day: &UtcDayKey,
        trailing_window_start_utc: &UtcTimestamp,
        trailing_window_end_utc: &UtcTimestamp,
    ) -> Result<AccountingTotals, SessionStoreError> {
        validate_session_id(session_id)?;
        if trailing_window_start_utc > trailing_window_end_utc {
            return Err(SessionStoreError::InvalidAccountingTimestamp);
        }
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT session_id,utc_day,emitted_at_utc,cost_json FROM turn_accounting \
             WHERE emitted_at_utc<=?4 \
               AND (session_id=?1 OR utc_day=?2 OR emitted_at_utc>=?3)",
        )?;
        let rows = statement.query_map(
            params![
                session_id,
                utc_day.as_str(),
                trailing_window_start_utc.as_str(),
                trailing_window_end_utc.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )?;
        let mut totals = AccountingTotals::empty(
            session_id,
            utc_day,
            trailing_window_start_utc,
            trailing_window_end_utc,
        );
        for row in rows {
            let (row_session, row_day, emitted_at, cost_json) = row?;
            let cost: Cost = serde_json::from_str(&cost_json)?;
            let in_session = row_session == session_id;
            let in_day = row_day == utc_day.as_str();
            let in_window = emitted_at.as_str() >= trailing_window_start_utc.as_str()
                && emitted_at.as_str() <= trailing_window_end_utc.as_str();
            if in_session {
                add_accounting_cost(&cost, &mut totals, AccountingScope::Session)?;
                if in_window {
                    add_accounting_cost(&cost, &mut totals, AccountingScope::TrailingSession)?;
                }
            }
            if in_day {
                add_accounting_cost(&cost, &mut totals, AccountingScope::Day)?;
            }
            if in_window {
                add_accounting_cost(&cost, &mut totals, AccountingScope::TrailingAllSessions)?;
            }
        }
        Ok(totals)
    }

    fn connection(&self) -> Result<Connection, SessionStoreError> {
        let connection = Connection::open(&self.path)?;
        configure_connection(&connection)?;
        ensure_accounting_schema(&connection)?;
        Ok(connection)
    }
}

impl AccountingTotals {
    fn empty(
        session_id: &str,
        utc_day: &UtcDayKey,
        trailing_window_start_utc: &UtcTimestamp,
        trailing_window_end_utc: &UtcTimestamp,
    ) -> Self {
        Self {
            session_id: session_id.to_owned(),
            utc_day: utc_day.clone(),
            utc_day_start_utc: UtcTimestamp(format!("{utc_day}T00:00:00.000Z")),
            trailing_window_start_utc: trailing_window_start_utc.clone(),
            trailing_window_end_utc: trailing_window_end_utc.clone(),
            session_micros_usd: 0,
            day_micros_usd: 0,
            trailing_session_micros_usd: 0,
            trailing_all_sessions_micros_usd: 0,
            session_ai_credit_micros: 0,
            day_ai_credit_micros: 0,
            trailing_session_ai_credit_micros: 0,
            trailing_all_sessions_ai_credit_micros: 0,
            session_subscription_quota_turns: 0,
            day_subscription_quota_turns: 0,
            session_unavailable_turns: 0,
            day_unavailable_turns: 0,
            session_non_usd_monetary_turns: 0,
            day_non_usd_monetary_turns: 0,
        }
    }
}

#[derive(Clone, Copy)]
enum AccountingScope {
    Session,
    Day,
    TrailingSession,
    TrailingAllSessions,
}

fn add_accounting_cost(
    cost: &Cost,
    totals: &mut AccountingTotals,
    scope: AccountingScope,
) -> Result<(), SessionStoreError> {
    match cost {
        Cost::Monetary {
            amount_micros,
            currency,
        } if currency.eq_ignore_ascii_case("USD") => match scope {
            AccountingScope::Session => checked_add(&mut totals.session_micros_usd, *amount_micros),
            AccountingScope::Day => checked_add(&mut totals.day_micros_usd, *amount_micros),
            AccountingScope::TrailingSession => {
                checked_add(&mut totals.trailing_session_micros_usd, *amount_micros)
            }
            AccountingScope::TrailingAllSessions => {
                checked_add(&mut totals.trailing_all_sessions_micros_usd, *amount_micros)
            }
        },
        Cost::Monetary { .. } => match scope {
            AccountingScope::Session => checked_add(&mut totals.session_non_usd_monetary_turns, 1),
            AccountingScope::Day => checked_add(&mut totals.day_non_usd_monetary_turns, 1),
            AccountingScope::TrailingSession | AccountingScope::TrailingAllSessions => Ok(()),
        },
        Cost::AiCredits { credits_micros, .. } => match scope {
            AccountingScope::Session => {
                checked_add(&mut totals.session_ai_credit_micros, *credits_micros)
            }
            AccountingScope::Day => checked_add(&mut totals.day_ai_credit_micros, *credits_micros),
            AccountingScope::TrailingSession => checked_add(
                &mut totals.trailing_session_ai_credit_micros,
                *credits_micros,
            ),
            AccountingScope::TrailingAllSessions => checked_add(
                &mut totals.trailing_all_sessions_ai_credit_micros,
                *credits_micros,
            ),
        },
        Cost::SubscriptionQuota { .. } => match scope {
            AccountingScope::Session => {
                checked_add(&mut totals.session_subscription_quota_turns, 1)
            }
            AccountingScope::Day => checked_add(&mut totals.day_subscription_quota_turns, 1),
            AccountingScope::TrailingSession | AccountingScope::TrailingAllSessions => Ok(()),
        },
        Cost::Unavailable { .. } => match scope {
            AccountingScope::Session => checked_add(&mut totals.session_unavailable_turns, 1),
            AccountingScope::Day => checked_add(&mut totals.day_unavailable_turns, 1),
            AccountingScope::TrailingSession | AccountingScope::TrailingAllSessions => Ok(()),
        },
    }
}

fn checked_add(total: &mut u64, value: u64) -> Result<(), SessionStoreError> {
    *total = total
        .checked_add(value)
        .ok_or(SessionStoreError::AccountingOverflow)?;
    Ok(())
}

fn validate_accounting_entry(entry: &TurnAccountingEntry) -> Result<(), SessionStoreError> {
    validate_session_id(&entry.session_id)?;
    if entry.turn_id.0.is_empty() || entry.turn_id.0.len() > 128 {
        return Err(SessionStoreError::InvalidAccountingIdentity);
    }
    if entry.emitted_at_utc.utc_day() != entry.utc_day {
        return Err(SessionStoreError::InvalidAccountingTimestamp);
    }
    Ok(())
}

fn validate_utc_day(value: &str) -> Result<(), SessionStoreError> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| !matches!(index, 4 | 7) && !byte.is_ascii_digit())
    {
        return Err(SessionStoreError::InvalidAccountingTimestamp);
    }
    let year = value[0..4]
        .parse::<u32>()
        .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
    let month = value[5..7]
        .parse::<u32>()
        .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
    let day = value[8..10]
        .parse::<u32>()
        .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
    let leap_year =
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return Err(SessionStoreError::InvalidAccountingTimestamp),
    };
    if year == 0 || !(1..=days_in_month).contains(&day) {
        return Err(SessionStoreError::InvalidAccountingTimestamp);
    }
    Ok(())
}

fn validate_utc_timestamp(value: &str) -> Result<(), SessionStoreError> {
    let bytes = value.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
        || bytes.iter().enumerate().any(|(index, byte)| {
            !matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) && !byte.is_ascii_digit()
        })
    {
        return Err(SessionStoreError::InvalidAccountingTimestamp);
    }
    validate_utc_day(&value[..10])?;
    let hour = value[11..13]
        .parse::<u32>()
        .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
    let minute = value[14..16]
        .parse::<u32>()
        .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
    let second = value[17..19]
        .parse::<u32>()
        .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
    let millis = value[20..23]
        .parse::<u32>()
        .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
    if hour > 23 || minute > 59 || second > 59 || millis > 999 {
        return Err(SessionStoreError::InvalidAccountingTimestamp);
    }
    Ok(())
}

fn insert_accounting_entry(
    connection: &Connection,
    entry: &TurnAccountingEntry,
) -> Result<(), SessionStoreError> {
    let sequence = entry.sequence_id.0.to_string();
    let attribution_json = serde_json::to_string(&entry.attribution)?;
    let usage_json = serde_json::to_string(&entry.usage)?;
    let cost_json = serde_json::to_string(&entry.cost)?;
    let inserted = connection.execute(
        "INSERT OR IGNORE INTO turn_accounting(\
           session_id,turn_id,sequence_id,emitted_at_utc,utc_day,\
           attribution_json,usage_json,cost_json\
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            entry.session_id,
            entry.turn_id.0,
            sequence,
            entry.emitted_at_utc.as_str(),
            entry.utc_day.as_str(),
            attribution_json,
            usage_json,
            cost_json,
        ],
    )?;
    if inserted == 1 {
        return Ok(());
    }
    let existing = connection
        .query_row(
            "SELECT session_id,turn_id,sequence_id,emitted_at_utc,utc_day,\
                    attribution_json,usage_json,cost_json \
             FROM turn_accounting WHERE session_id=?1 AND sequence_id=?2 \
             LIMIT 1",
            params![entry.session_id, entry.sequence_id.0.to_string()],
            accounting_entry_from_row,
        )
        .optional()?;
    if existing.as_ref() == Some(entry) {
        Ok(())
    } else {
        Err(SessionStoreError::AccountingConflict)
    }
}

fn accounting_entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TurnAccountingEntry> {
    let sequence = row.get::<_, String>(2)?;
    let attribution_json = row.get::<_, String>(5)?;
    let usage_json = row.get::<_, String>(6)?;
    let cost_json = row.get::<_, String>(7)?;
    let sequence_id = sequence.parse::<u64>().map(SequenceId).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let attribution =
        serde_json::from_str::<AccountingAttribution>(&attribution_json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    let usage = serde_json::from_str::<Usage>(&usage_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let cost = serde_json::from_str::<Cost>(&cost_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(TurnAccountingEntry {
        session_id: row.get(0)?,
        turn_id: TurnId(row.get(1)?),
        sequence_id,
        emitted_at_utc: UtcTimestamp::parse(row.get::<_, String>(3)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        utc_day: UtcDayKey::parse(row.get::<_, String>(4)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        attribution,
        usage,
        cost,
    })
}

fn configure_connection(connection: &Connection) -> Result<(), SessionStoreError> {
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
    Ok(())
}

fn ensure_accounting_schema(connection: &Connection) -> Result<(), SessionStoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS turn_accounting(
           session_id TEXT NOT NULL,
           turn_id TEXT NOT NULL,
           sequence_id TEXT NOT NULL,
           emitted_at_utc TEXT NOT NULL,
           utc_day TEXT NOT NULL,
           attribution_json TEXT NOT NULL,
           usage_json TEXT NOT NULL,
           cost_json TEXT NOT NULL,
           PRIMARY KEY(session_id,sequence_id)
         );",
    )?;
    ensure_accounting_columns(connection)?;
    remove_legacy_turn_uniqueness(connection)?;
    connection.execute_batch(
        "CREATE INDEX IF NOT EXISTS turn_accounting_session_time
           ON turn_accounting(session_id,emitted_at_utc);
         CREATE INDEX IF NOT EXISTS turn_accounting_day_time
           ON turn_accounting(utc_day,emitted_at_utc);
         CREATE INDEX IF NOT EXISTS turn_accounting_time
           ON turn_accounting(emitted_at_utc);",
    )?;
    Ok(())
}

fn remove_legacy_turn_uniqueness(connection: &Connection) -> Result<(), SessionStoreError> {
    let schema = connection.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='turn_accounting'",
        [],
        |row| row.get::<_, String>(0),
    )?;
    let normalized = schema
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if !normalized.contains("unique(session_id,turn_id)") {
        return Ok(());
    }
    connection.execute_batch(
        "BEGIN IMMEDIATE;
         ALTER TABLE turn_accounting RENAME TO turn_accounting_legacy_turn_unique;
         CREATE TABLE turn_accounting(
           session_id TEXT NOT NULL,
           turn_id TEXT NOT NULL,
           sequence_id TEXT NOT NULL,
           emitted_at_utc TEXT NOT NULL,
           utc_day TEXT NOT NULL,
           attribution_json TEXT NOT NULL,
           usage_json TEXT NOT NULL,
           cost_json TEXT NOT NULL,
           PRIMARY KEY(session_id,sequence_id)
         );
         INSERT INTO turn_accounting(
           session_id,turn_id,sequence_id,emitted_at_utc,utc_day,
           attribution_json,usage_json,cost_json
         ) SELECT
           session_id,turn_id,sequence_id,emitted_at_utc,utc_day,
           attribution_json,usage_json,cost_json
         FROM turn_accounting_legacy_turn_unique;
         DROP TABLE turn_accounting_legacy_turn_unique;
         COMMIT;",
    )?;
    Ok(())
}

fn ensure_accounting_columns(connection: &Connection) -> Result<(), SessionStoreError> {
    let mut statement = connection.prepare("PRAGMA table_info(turn_accounting)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|column| column == "attribution_json") {
        connection.execute(
            "ALTER TABLE turn_accounting ADD COLUMN attribution_json TEXT NOT NULL \
             DEFAULT '\"main\"'",
            [],
        )?;
    }
    if !columns.iter().any(|column| column == "usage_json") {
        connection.execute(
            "ALTER TABLE turn_accounting ADD COLUMN usage_json TEXT NOT NULL DEFAULT \
             '{\"input_tokens\":\"0\",\"output_tokens\":\"0\",\
               \"cache_read_tokens\":\"0\",\"cache_write_tokens\":\"0\",\
               \"reasoning_tokens\":\"0\"}'",
            [],
        )?;
    }
    Ok(())
}

/// Whether a derived index row agrees with the authoritative event log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionStatus {
    /// No row exists for the session.
    Missing,
    /// The row was built through the requested last event.
    Current,
    /// The row exists but was built from a different log prefix.
    Stale {
        /// Last event represented by the row.
        projected_through: Option<SequenceId>,
    },
}

/// `SQLite` projection for session listing and full-text search.
#[derive(Clone, Debug)]
pub struct SessionIndex {
    path: PathBuf,
}

impl SessionIndex {
    /// Opens or creates `index.sqlite` under the storage root.
    ///
    /// # Errors
    ///
    /// Returns an I/O or `SQLite` initialization error.
    pub fn open(root: &Path) -> Result<Self, SessionStoreError> {
        fs::create_dir_all(root)?;
        let index = Self {
            path: root.join("index.sqlite"),
        };
        index.connection()?;
        Ok(index)
    }

    /// Inserts or replaces one derived session projection.
    ///
    /// # Errors
    ///
    /// Returns an invalid-id or `SQLite` error.
    pub fn upsert(&self, projection: &SessionProjection) -> Result<(), SessionStoreError> {
        validate_session_id(&projection.summary.id)?;
        let connection = self.connection()?;
        upsert_projection(&connection, projection)?;
        Ok(())
    }

    /// Atomically replaces every derived row from caller-projected event logs.
    ///
    /// The JSONL logs remain authoritative; callers use this after any missing
    /// or stale watermark is detected.
    ///
    /// # Errors
    ///
    /// Returns an invalid-id or `SQLite` transaction error.
    pub fn replace_all(&self, projections: &[SessionProjection]) -> Result<(), SessionStoreError> {
        for projection in projections {
            validate_session_id(&projection.summary.id)?;
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM sessions", [])?;
        for projection in projections {
            upsert_projection(&transaction, projection)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Recreates a missing or corrupt derived database from authoritative logs.
    ///
    /// A temporary database is fully populated and synchronized before it
    /// replaces the old index. No event log is modified.
    ///
    /// # Errors
    ///
    /// Returns an invalid projection, I/O, or `SQLite` error.
    pub fn rebuild(
        root: &Path,
        projections: &[SessionProjection],
        accounting_entries: &[TurnAccountingEntry],
    ) -> Result<Self, SessionStoreError> {
        fs::create_dir_all(root)?;
        for projection in projections {
            validate_session_id(&projection.summary.id)?;
        }
        for entry in accounting_entries {
            validate_accounting_entry(entry)?;
        }
        let temporary = root.join(format!(
            ".index-{}-{}.sqlite",
            std::process::id(),
            INDEX_TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        remove_if_exists(&temporary)?;
        let temporary_index = Self {
            path: temporary.clone(),
        };
        remove_if_exists(&sidecar_path(&temporary, "-wal"))?;
        remove_if_exists(&sidecar_path(&temporary, "-shm"))?;
        temporary_index.replace_all(projections)?;
        AccountingLedger {
            path: temporary.clone(),
        }
        .replace_all(accounting_entries)?;
        {
            let connection = temporary_index.connection()?;
            connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        }
        remove_if_exists(&sidecar_path(&temporary, "-wal"))?;
        remove_if_exists(&sidecar_path(&temporary, "-shm"))?;
        let final_path = root.join("index.sqlite");
        remove_if_exists(&sidecar_path(&final_path, "-wal"))?;
        remove_if_exists(&sidecar_path(&final_path, "-shm"))?;
        remove_if_exists(&final_path)?;
        fs::rename(&temporary, &final_path)?;
        File::open(root)?.sync_all()?;
        let rebuilt = Self { path: final_path };
        rebuilt.connection()?;
        Ok(rebuilt)
    }

    /// Compares one row's projection watermark with the event log's last id.
    ///
    /// # Errors
    ///
    /// Returns an invalid-id, corrupt-watermark, or `SQLite` query error.
    pub fn projection_status(
        &self,
        id: &str,
        log_last_sequence: Option<SequenceId>,
    ) -> Result<ProjectionStatus, SessionStoreError> {
        validate_session_id(id)?;
        let connection = self.connection()?;
        let stored = connection
            .query_row(
                "SELECT projected_sequence FROM sessions WHERE id=?1",
                [id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        let Some(stored) = stored else {
            return Ok(ProjectionStatus::Missing);
        };
        let Some(stored) = stored else {
            return Ok(ProjectionStatus::Stale {
                projected_through: None,
            });
        };
        let projected_through = if stored == "-" {
            None
        } else {
            Some(
                stored
                    .parse::<u64>()
                    .map(SequenceId)
                    .map_err(|_| SessionStoreError::CorruptProjectionWatermark)?,
            )
        };
        if projected_through == log_last_sequence {
            Ok(ProjectionStatus::Current)
        } else {
            Ok(ProjectionStatus::Stale { projected_through })
        }
    }

    fn connection(&self) -> Result<Connection, SessionStoreError> {
        let connection = Connection::open(&self.path)?;
        configure_connection(&connection)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions(
               id TEXT NOT NULL UNIQUE,
               title TEXT NOT NULL,
               updated_unix_ms INTEGER NOT NULL,
               cost_micros INTEGER NOT NULL,
               transcript TEXT NOT NULL,
               projected_sequence TEXT
             );",
        )?;
        ensure_projection_column(&connection)?;
        ensure_accounting_schema(&connection)?;
        connection.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS sessions_fts USING fts5(
               title,transcript,content='sessions',content_rowid='rowid'
             );
             CREATE TRIGGER IF NOT EXISTS sessions_ai AFTER INSERT ON sessions BEGIN
               INSERT INTO sessions_fts(rowid,title,transcript)
               VALUES (new.rowid,new.title,new.transcript);
             END;
             CREATE TRIGGER IF NOT EXISTS sessions_ad AFTER DELETE ON sessions BEGIN
               INSERT INTO sessions_fts(sessions_fts,rowid,title,transcript)
               VALUES ('delete',old.rowid,old.title,old.transcript);
             END;
             CREATE TRIGGER IF NOT EXISTS sessions_au AFTER UPDATE ON sessions BEGIN
               INSERT INTO sessions_fts(sessions_fts,rowid,title,transcript)
               VALUES ('delete',old.rowid,old.title,old.transcript);
               INSERT INTO sessions_fts(rowid,title,transcript)
               VALUES (new.rowid,new.title,new.transcript);
             END;",
        )?;
        Ok(connection)
    }

    /// Lists newest sessions with a deterministic id tie-break.
    ///
    /// # Errors
    ///
    /// Returns a `SQLite` query error.
    pub fn list(&self, limit: usize) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let limit = i64::try_from(limit).map_err(|_| SessionStoreError::LimitOverflow)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,title,updated_unix_ms,cost_micros FROM sessions \
             ORDER BY updated_unix_ms DESC,id ASC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], summary_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Searches title and transcript text through `SQLite` FTS5.
    ///
    /// # Errors
    ///
    /// Returns a `SQLite` query error.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).map_err(|_| SessionStoreError::LimitOverflow)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT s.id,s.title,s.updated_unix_ms,s.cost_micros \
             FROM sessions_fts f JOIN sessions s ON s.rowid=f.rowid \
             WHERE sessions_fts MATCH ?1 \
             ORDER BY bm25(sessions_fts),s.updated_unix_ms DESC,s.id ASC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![query, limit], summary_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Searches an existing index through a `SQLite` read-only connection.
    ///
    /// # Errors
    ///
    /// Returns an error when the query or result limit is too large, the index
    /// path is unsafe or changes during the read, or the database cannot be
    /// queried through its read-only connection.
    pub fn search_read_only(
        root: &Path,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        if query.len() > 512 {
            return Err(SessionStoreError::SearchQueryTooLarge);
        }
        if limit > 1_001 {
            return Err(SessionStoreError::SearchLimitTooLarge);
        }
        let query = plain_fts_query(query);
        let limit = i64::try_from(limit).map_err(|_| SessionStoreError::LimitOverflow)?;
        let path = root.join("index.sqlite");
        let before = validate_read_only_index(&path)?;
        let canonical_root = fs::canonicalize(root)?;
        let canonical_path = fs::canonicalize(&path)?;
        if canonical_path.parent() != Some(canonical_root.as_path()) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        let snapshot = read_only_index_snapshot(&canonical_root, &before)?;
        let snapshot_path = fs::canonicalize(snapshot.path().join("index.sqlite"))?;
        let connection = Connection::open_with_flags(
            snapshot_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let after = validate_read_only_index(&path)?;
        if !same_file_identity(&before, &after) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        let mut statement = connection.prepare(
            "SELECT s.id,s.title,s.updated_unix_ms,s.cost_micros \
             FROM sessions_fts f JOIN sessions s ON s.rowid=f.rowid \
             WHERE sessions_fts MATCH ?1 \
             ORDER BY bm25(sessions_fts),s.updated_unix_ms DESC,s.id ASC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![query, limit], summary_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Lists newest sessions from a private read-only snapshot of an existing
    /// index. The live database and WAL are never modified by this query.
    ///
    /// # Errors
    ///
    /// Returns an error when the result limit is too large, the index path is
    /// unsafe or changes during the read, or the snapshot cannot be queried.
    pub fn list_read_only(
        root: &Path,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        if limit > 1_001 {
            return Err(SessionStoreError::SearchLimitTooLarge);
        }
        let limit = i64::try_from(limit).map_err(|_| SessionStoreError::LimitOverflow)?;
        let path = root.join("index.sqlite");
        let before = validate_read_only_index(&path)?;
        let canonical_root = fs::canonicalize(root)?;
        let canonical_path = fs::canonicalize(&path)?;
        if canonical_path.parent() != Some(canonical_root.as_path()) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        let snapshot = read_only_index_snapshot(&canonical_root, &before)?;
        let snapshot_path = fs::canonicalize(snapshot.path().join("index.sqlite"))?;
        let connection = Connection::open_with_flags(
            snapshot_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let after = validate_read_only_index(&path)?;
        if !same_file_identity(&before, &after) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        let mut statement = connection.prepare(
            "SELECT id,title,updated_unix_ms,cost_micros FROM sessions \
             ORDER BY updated_unix_ms DESC,id ASC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], summary_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Returns one projection by id.
    ///
    /// # Errors
    ///
    /// Returns an invalid-id or `SQLite` query error.
    pub fn get(&self, id: &str) -> Result<Option<SessionSummary>, SessionStoreError> {
        validate_session_id(id)?;
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id,title,updated_unix_ms,cost_micros FROM sessions WHERE id=?1",
                [id],
                summary_from_row,
            )
            .optional()
            .map_err(Into::into)
    }
}

fn validate_read_only_index(path: &Path) -> Result<fs::Metadata, SessionStoreError> {
    let link = fs::symlink_metadata(path)?;
    if link.file_type().is_symlink() || !link.is_file() {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if link.nlink() != 1 {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
    }
    Ok(link)
}

/// Copies the `SQLite` database and any committed WAL into a private snapshot.
///
/// `SQLite` WAL readers may create an empty `-wal` file or update read marks in
/// `-shm`, even when the database handle itself is opened read-only. Querying a
/// private snapshot keeps the live derived index byte-for-byte unchanged while
/// still including committed frames which have not yet been checkpointed.
#[cfg(unix)]
fn read_only_index_snapshot(
    root: &Path,
    expected: &fs::Metadata,
) -> Result<TempDir, SessionStoreError> {
    use rustix::{
        fs::{FileType, Mode, OFlags},
        io::Errno,
    };

    let directory = File::from(
        rustix::fs::open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let main = open_snapshot_source(&directory, "index.sqlite")?
        .ok_or(SessionStoreError::UnsafeSessionIndex)?;
    let main_before = main.metadata()?;
    if !same_file_identity(expected, &main_before) {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    validate_snapshot_source_size(&main_before, "index.sqlite", MAX_SEARCH_INDEX_BYTES)?;

    let wal = open_snapshot_source(&directory, "index.sqlite-wal")?;
    let wal_before = wal.as_ref().map(File::metadata).transpose()?;
    if let Some(metadata) = wal_before.as_ref() {
        validate_snapshot_source_size(metadata, "index.sqlite-wal", MAX_SEARCH_INDEX_WAL_BYTES)?;
    }
    let snapshot = tempfile::tempdir()?;
    copy_snapshot_source(
        &main,
        &snapshot.path().join("index.sqlite"),
        "index.sqlite",
        MAX_SEARCH_INDEX_BYTES,
    )?;
    if let Some(wal) = wal.as_ref() {
        copy_snapshot_source(
            wal,
            &snapshot.path().join("index.sqlite-wal"),
            "index.sqlite-wal",
            MAX_SEARCH_INDEX_WAL_BYTES,
        )?;
    }

    let main_after = main.metadata()?;
    if !same_snapshot_version(&main_before, &main_after)
        || !snapshot_name_still_refers_to(&directory, "index.sqlite", &main_before)?
    {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    match (wal_before.as_ref(), wal.as_ref()) {
        (Some(before), Some(wal)) => {
            if !same_snapshot_version(before, &wal.metadata()?)
                || !snapshot_name_still_refers_to(&directory, "index.sqlite-wal", before)?
            {
                return Err(SessionStoreError::UnsafeSessionIndex);
            }
        }
        (None, None) => match rustix::fs::openat(
            &directory,
            "index.sqlite-wal",
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Err(Errno::NOENT) => {}
            Ok(_) | Err(_) => return Err(SessionStoreError::UnsafeSessionIndex),
        },
        _ => return Err(SessionStoreError::UnsafeSessionIndex),
    }

    let stat = rustix::fs::fstat(&main).map_err(std::io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    Ok(snapshot)
}

#[cfg(unix)]
fn open_snapshot_source(parent: &File, name: &str) -> Result<Option<File>, SessionStoreError> {
    use rustix::{
        fs::{FileType, Mode, OFlags},
        io::Errno,
    };

    match rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(descriptor) => {
            let stat = rustix::fs::fstat(&descriptor).map_err(std::io::Error::from)?;
            if !FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
                return Err(SessionStoreError::UnsafeSessionIndex);
            }
            Ok(Some(File::from(descriptor)))
        }
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(std::io::Error::from(error).into()),
    }
}

#[cfg(unix)]
fn copy_snapshot_source(
    source: &File,
    destination: &Path,
    component: &'static str,
    max_bytes: u64,
) -> Result<(), SessionStoreError> {
    let metadata = source.metadata()?;
    validate_snapshot_source_size(&metadata, component, max_bytes)?;
    let length = usize::try_from(metadata.len()).map_err(|_| SessionStoreError::LimitOverflow)?;
    let mut output = File::create(destination)?;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut offset = 0_usize;
    while offset < length {
        let remaining = buffer.len().min(length.saturating_sub(offset));
        let count = source.read_at(
            &mut buffer[..remaining],
            u64::try_from(offset).map_err(|_| SessionStoreError::LimitOverflow)?,
        )?;
        if count == 0 {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        output.write_all(&buffer[..count])?;
        offset = offset
            .checked_add(count)
            .ok_or(SessionStoreError::LimitOverflow)?;
    }
    output.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn snapshot_name_still_refers_to(
    parent: &File,
    name: &str,
    expected: &fs::Metadata,
) -> Result<bool, SessionStoreError> {
    let Some(current) = open_snapshot_source(parent, name)? else {
        return Ok(false);
    };
    Ok(same_file_identity(expected, &current.metadata()?))
}

fn same_snapshot_version(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_file_identity(left, right) && left.modified().ok() == right.modified().ok()
}

fn validate_snapshot_source_size(
    metadata: &fs::Metadata,
    component: &'static str,
    max_bytes: u64,
) -> Result<(), SessionStoreError> {
    if metadata.len() > max_bytes {
        return Err(SessionStoreError::SessionIndexSnapshotTooLarge {
            component,
            max_bytes,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn read_only_index_snapshot(
    root: &Path,
    expected: &fs::Metadata,
) -> Result<TempDir, SessionStoreError> {
    let main_path = root.join("index.sqlite");
    let main_link = fs::symlink_metadata(&main_path)?;
    if main_link.file_type().is_symlink() || !same_file_identity(expected, &main_link) {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    validate_snapshot_source_size(&main_link, "index.sqlite", MAX_SEARCH_INDEX_BYTES)?;
    let wal_path = root.join("index.sqlite-wal");
    let wal_link = match fs::symlink_metadata(&wal_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(SessionStoreError::UnsafeSessionIndex);
            }
            validate_snapshot_source_size(
                &metadata,
                "index.sqlite-wal",
                MAX_SEARCH_INDEX_WAL_BYTES,
            )?;
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let snapshot = tempfile::tempdir()?;
    copy_snapshot_path(
        &main_path,
        &snapshot.path().join("index.sqlite"),
        &main_link,
        "index.sqlite",
        MAX_SEARCH_INDEX_BYTES,
    )?;
    if let Some(wal_link) = wal_link.as_ref() {
        copy_snapshot_path(
            &wal_path,
            &snapshot.path().join("index.sqlite-wal"),
            wal_link,
            "index.sqlite-wal",
            MAX_SEARCH_INDEX_WAL_BYTES,
        )?;
        if !same_snapshot_version(&wal_link, &fs::symlink_metadata(&wal_path)?) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
    }
    if !same_snapshot_version(&main_link, &fs::symlink_metadata(&main_path)?) {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    Ok(snapshot)
}

#[cfg(not(unix))]
fn copy_snapshot_path(
    source_path: &Path,
    destination: &Path,
    expected: &fs::Metadata,
    component: &'static str,
    max_bytes: u64,
) -> Result<(), SessionStoreError> {
    let source = File::open(source_path)?;
    let metadata = source.metadata()?;
    if !same_file_identity(expected, &metadata) {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    validate_snapshot_source_size(&metadata, component, max_bytes)?;
    let mut bounded = source.take(max_bytes.saturating_add(1));
    let mut output = File::create(destination)?;
    let copied = std::io::copy(&mut bounded, &mut output)?;
    if copied > max_bytes {
        return Err(SessionStoreError::SessionIndexSnapshotTooLarge {
            component,
            max_bytes,
        });
    }
    output.sync_all()?;
    Ok(())
}

fn plain_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino() && left.len() == right.len()
}

#[cfg(not(unix))]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

fn summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionSummary> {
    Ok(SessionSummary {
        id: row.get(0)?,
        title: row.get(1)?,
        updated_unix_ms: row.get(2)?,
        cost_micros: row.get(3)?,
    })
}

fn upsert_projection(
    connection: &Connection,
    projection: &SessionProjection,
) -> Result<(), SessionStoreError> {
    connection.execute(
        "INSERT INTO sessions(\
           id,title,updated_unix_ms,cost_micros,transcript,projected_sequence\
         ) VALUES (?1,?2,?3,?4,?5,?6) \
         ON CONFLICT(id) DO UPDATE SET title=excluded.title, \
         updated_unix_ms=excluded.updated_unix_ms, \
         cost_micros=excluded.cost_micros, transcript=excluded.transcript, \
         projected_sequence=excluded.projected_sequence",
        params![
            projection.summary.id,
            projection.summary.title,
            projection.summary.updated_unix_ms,
            projection.summary.cost_micros,
            projection.transcript,
            projection
                .projected_through
                .map_or_else(|| "-".to_owned(), |sequence| sequence.0.to_string())
        ],
    )?;
    Ok(())
}

fn ensure_projection_column(connection: &Connection) -> Result<(), SessionStoreError> {
    let mut statement = connection.prepare("PRAGMA table_info(sessions)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|column| column == "projected_sequence") {
        connection.execute(
            "ALTER TABLE sessions ADD COLUMN projected_sequence TEXT",
            [],
        )?;
    }
    Ok(())
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn remove_if_exists(path: &Path) -> Result<(), SessionStoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn recover_and_validate(file: &File) -> Result<u64, SessionStoreError> {
    let bytes = read_opened_file(file)?;
    let mut next_sequence = 0_u64;
    let mut record_start = 0_usize;

    for record in bytes.split_inclusive(|byte| *byte == b'\n') {
        let record_end = record_start
            .checked_add(record.len())
            .ok_or(SessionStoreError::LimitOverflow)?;
        if record_end == bytes.len() && record.last() != Some(&b'\n') {
            truncate_and_sync_event_file(
                file,
                u64::try_from(record_start).map_err(|_| SessionStoreError::LimitOverflow)?,
            )?;
            return Ok(next_sequence);
        }

        validate_recovered_event(&record[..record.len().saturating_sub(1)], next_sequence)?;
        next_sequence = next_sequence
            .checked_add(1)
            .ok_or(SessionStoreError::SequenceOverflow)?;
        record_start = record_end;
    }
    Ok(next_sequence)
}

fn validate_recovered_event(
    record: &[u8],
    expected_sequence: u64,
) -> Result<(), SessionStoreError> {
    if record.is_empty() {
        return Err(SessionStoreError::CorruptEvent("blank JSONL record"));
    }
    let envelope: EventEnvelope<serde_json::Value> = serde_json::from_slice(record)?;
    if envelope.schema_version != EVENT_SCHEMA_VERSION {
        return Err(SessionStoreError::UnsupportedEventVersion(
            envelope.schema_version,
        ));
    }
    if envelope.sequence != SequenceId(expected_sequence) {
        return Err(SessionStoreError::CorruptEvent(
            "non-contiguous event sequence",
        ));
    }
    Ok(())
}

fn append_event_bytes(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    write_event_bytes(file, bytes)?;
    file.flush()?;
    sync_event_file(file)
}

fn truncate_and_sync_event_file(file: &File, len: u64) -> std::io::Result<()> {
    set_event_file_len(file, len)?;
    sync_event_file(file)
}

fn write_event_bytes(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(fail_after) = take_partial_append_write_fault() {
        file.write_all(&bytes[..bytes.len().min(fail_after)])?;
        return Err(std::io::Error::other(
            "injected partial event-log append failure",
        ));
    }
    file.write_all(bytes)
}

fn set_event_file_len(file: &File, len: u64) -> std::io::Result<()> {
    #[cfg(test)]
    if take_append_truncate_fault() {
        return Err(std::io::Error::other("injected event-log rollback failure"));
    }
    file.set_len(len)
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct AppendFault {
    partial_write_after: Option<usize>,
    fail_truncate: bool,
}

#[cfg(test)]
thread_local! {
    static APPEND_FAULT: std::cell::Cell<Option<AppendFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn install_append_fault(partial_write_after: usize, fail_truncate: bool) -> AppendFaultGuard {
    APPEND_FAULT.with(|fault| {
        assert!(fault.get().is_none(), "append fault already installed");
        fault.set(Some(AppendFault {
            partial_write_after: Some(partial_write_after),
            fail_truncate,
        }));
    });
    AppendFaultGuard
}

#[cfg(test)]
struct AppendFaultGuard;

#[cfg(test)]
impl Drop for AppendFaultGuard {
    fn drop(&mut self) {
        APPEND_FAULT.with(|fault| fault.set(None));
    }
}

#[cfg(test)]
fn take_partial_append_write_fault() -> Option<usize> {
    APPEND_FAULT.with(|fault| {
        let mut state = fault.get()?;
        let fail_after = state.partial_write_after.take();
        fault.set(Some(state));
        fail_after
    })
}

#[cfg(test)]
fn take_append_truncate_fault() -> bool {
    APPEND_FAULT.with(|fault| {
        let Some(mut state) = fault.get() else {
            return false;
        };
        let fail = state.fail_truncate;
        state.fail_truncate = false;
        fault.set(Some(state));
        fail
    })
}

fn load_events<T: DeserializeOwned>(
    file: &File,
) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
    let bytes = read_opened_file(file)?;
    parse_events_bounded(&bytes, usize::MAX)
}

fn load_events_bounded_with_size<T: DeserializeOwned>(
    file: &File,
    max_bytes: u64,
    max_events: usize,
) -> Result<(Vec<EventEnvelope<T>>, u64), SessionStoreError> {
    let bytes = read_opened_file_bounded(file, max_bytes)?;
    let byte_count = u64::try_from(bytes.len()).map_err(|_| SessionStoreError::LimitOverflow)?;
    parse_events_bounded(&bytes, max_events).map(|events| (events, byte_count))
}

fn load_event_page_with_hook<T, Hook>(
    file: &File,
    after_sequence: Option<SequenceId>,
    limits: SessionEventPageLimits,
    after_snapshot: Hook,
) -> Result<SessionEventPage<T>, SessionStoreError>
where
    T: DeserializeOwned,
    Hook: FnOnce(),
{
    validate_page_limits(limits)?;
    let snapshot = event_file_snapshot(file)?;
    if snapshot.len() > limits.max_scan_bytes {
        return Err(SessionStoreError::EventScanBytesExceeded {
            max_bytes: limits.max_scan_bytes,
        });
    }
    after_snapshot();
    let result = scan_event_page(file, snapshot.len(), after_sequence, limits);
    // Stability wins over any parse result. A writer can expose an incomplete
    // tail while the scan is in progress; callers must never mistake that race
    // for durable corruption in the captured session.
    verify_event_file_snapshot(file, &snapshot)?;
    result
}

fn load_event_tail_page_with_hook<T, Hook>(
    file: &File,
    limits: SessionEventPageLimits,
    after_snapshot: Hook,
) -> Result<SessionEventPage<T>, SessionStoreError>
where
    T: DeserializeOwned,
    Hook: FnOnce(),
{
    validate_page_limits(limits)?;
    let snapshot = event_file_snapshot(file)?;
    if snapshot.len() > limits.max_scan_bytes {
        return Err(SessionStoreError::EventScanBytesExceeded {
            max_bytes: limits.max_scan_bytes,
        });
    }
    after_snapshot();
    let result = scan_event_tail_page(file, snapshot.len(), limits);
    verify_event_file_snapshot(file, &snapshot)?;
    result
}

fn validate_page_limits(limits: SessionEventPageLimits) -> Result<(), SessionStoreError> {
    if limits.max_page_bytes == 0
        || limits.max_page_events == 0
        || limits.max_line_bytes == 0
        || limits.max_scan_bytes == 0
        || limits.max_scan_events == 0
    {
        return Err(SessionStoreError::InvalidEventPageLimits);
    }
    Ok(())
}

fn scan_event_page<T: DeserializeOwned>(
    file: &File,
    snapshot_bytes: u64,
    after_sequence: Option<SequenceId>,
    limits: SessionEventPageLimits,
) -> Result<SessionEventPage<T>, SessionStoreError> {
    let mut remaining = snapshot_bytes;
    let mut reader = BufReader::new(file.take(snapshot_bytes));
    let mut line = Vec::with_capacity(limits.max_line_bytes.min(16 * 1024));
    let mut events = Vec::with_capacity(limits.max_page_events.min(2_000));
    let mut page_bytes = 0_u64;
    let mut total_events = 0_u64;

    while let Some(line_bytes) = read_bounded_snapshot_line(
        &mut reader,
        &mut remaining,
        limits.max_line_bytes,
        &mut line,
    )? {
        if total_events >= limits.max_scan_events {
            return Err(SessionStoreError::EventScanCountExceeded {
                max_events: limits.max_scan_events,
            });
        }
        let envelope: EventEnvelope<T> = serde_json::from_slice(&line)?;
        if envelope.schema_version != EVENT_SCHEMA_VERSION {
            return Err(SessionStoreError::UnsupportedEventVersion(
                envelope.schema_version,
            ));
        }
        if envelope.sequence != SequenceId(total_events) {
            return Err(SessionStoreError::CorruptEvent(
                "non-contiguous event sequence",
            ));
        }
        total_events = total_events
            .checked_add(1)
            .ok_or(SessionStoreError::SequenceOverflow)?;

        if after_sequence.is_none_or(|cursor| envelope.sequence > cursor)
            && events.len() < limits.max_page_events
        {
            let next_page_bytes = page_bytes
                .checked_add(line_bytes)
                .ok_or(SessionStoreError::LimitOverflow)?;
            if next_page_bytes <= limits.max_page_bytes {
                page_bytes = next_page_bytes;
                events.push(envelope);
            } else if events.is_empty() {
                return Err(SessionStoreError::EventPageByteLimitTooSmall {
                    required_bytes: line_bytes,
                    max_bytes: limits.max_page_bytes,
                });
            }
        }
    }

    let tail_sequence = total_events.checked_sub(1).map(SequenceId);
    if after_sequence.is_some_and(|cursor| tail_sequence.is_none_or(|tail| cursor > tail)) {
        return Err(SessionStoreError::EventPageCursorAhead);
    }
    let first_page_sequence = events.first().map(|envelope| envelope.sequence);
    let next_cursor = events
        .last()
        .map(|envelope| envelope.sequence)
        .or(after_sequence);
    let covered_events = next_cursor.map_or(0, |cursor| cursor.0.saturating_add(1));
    let events_after_page = total_events
        .checked_sub(covered_events)
        .ok_or(SessionStoreError::EventPageCursorAhead)?;
    let events_before_page = first_page_sequence.map_or(covered_events, |sequence| sequence.0);
    Ok(SessionEventPage {
        events,
        page_bytes,
        next_cursor,
        has_more: events_after_page > 0,
        events_before_page,
        events_after_page,
        total_events,
        total_bytes: snapshot_bytes,
        tail_sequence,
    })
}

fn scan_event_tail_page<T: DeserializeOwned>(
    file: &File,
    snapshot_bytes: u64,
    limits: SessionEventPageLimits,
) -> Result<SessionEventPage<T>, SessionStoreError> {
    let mut remaining = snapshot_bytes;
    let mut reader = BufReader::new(file.take(snapshot_bytes));
    let mut line = Vec::with_capacity(limits.max_line_bytes.min(16 * 1024));
    let mut retained = VecDeque::with_capacity(limits.max_page_events.min(2_000));
    let mut page_bytes = 0_u64;
    let mut total_events = 0_u64;

    while let Some(line_bytes) = read_bounded_snapshot_line(
        &mut reader,
        &mut remaining,
        limits.max_line_bytes,
        &mut line,
    )? {
        if total_events >= limits.max_scan_events {
            return Err(SessionStoreError::EventScanCountExceeded {
                max_events: limits.max_scan_events,
            });
        }
        let envelope: EventEnvelope<T> = serde_json::from_slice(&line)?;
        if envelope.schema_version != EVENT_SCHEMA_VERSION {
            return Err(SessionStoreError::UnsupportedEventVersion(
                envelope.schema_version,
            ));
        }
        if envelope.sequence != SequenceId(total_events) {
            return Err(SessionStoreError::CorruptEvent(
                "non-contiguous event sequence",
            ));
        }
        total_events = total_events
            .checked_add(1)
            .ok_or(SessionStoreError::SequenceOverflow)?;
        if line_bytes > limits.max_page_bytes {
            return Err(SessionStoreError::EventPageByteLimitTooSmall {
                required_bytes: line_bytes,
                max_bytes: limits.max_page_bytes,
            });
        }
        while retained.len() >= limits.max_page_events
            || page_bytes.saturating_add(line_bytes) > limits.max_page_bytes
        {
            let (_, removed_bytes) = retained
                .pop_front()
                .ok_or(SessionStoreError::LimitOverflow)?;
            page_bytes = page_bytes
                .checked_sub(removed_bytes)
                .ok_or(SessionStoreError::LimitOverflow)?;
        }
        page_bytes = page_bytes
            .checked_add(line_bytes)
            .ok_or(SessionStoreError::LimitOverflow)?;
        retained.push_back((envelope, line_bytes));
    }

    let events_before_page = retained
        .front()
        .map_or(total_events, |(envelope, _)| envelope.sequence.0);
    let events = retained
        .into_iter()
        .map(|(envelope, _)| envelope)
        .collect::<Vec<_>>();
    let tail_sequence = total_events.checked_sub(1).map(SequenceId);
    let next_cursor = events.last().map(|envelope| envelope.sequence);
    Ok(SessionEventPage {
        events,
        page_bytes,
        next_cursor,
        has_more: false,
        events_before_page,
        events_after_page: 0,
        total_events,
        total_bytes: snapshot_bytes,
        tail_sequence,
    })
}

fn read_bounded_snapshot_line<R: BufRead>(
    reader: &mut R,
    remaining: &mut u64,
    max_line_bytes: usize,
    line: &mut Vec<u8>,
) -> Result<Option<u64>, SessionStoreError> {
    line.clear();
    if *remaining == 0 {
        return Ok(None);
    }
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Err(SessionStoreError::CorruptEvent(
                "event log ended before its descriptor snapshot",
            ));
        }
        let remaining_usize = usize::try_from(*remaining).unwrap_or(usize::MAX);
        let available_len = available.len().min(remaining_usize);
        let chunk = &available[..available_len];
        if let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
            if line.len().saturating_add(newline) > max_line_bytes {
                return Err(SessionStoreError::EventRecordTooLarge { max_line_bytes });
            }
            line.extend_from_slice(&chunk[..newline]);
            let consumed = newline
                .checked_add(1)
                .ok_or(SessionStoreError::LimitOverflow)?;
            reader.consume(consumed);
            let consumed = u64::try_from(consumed).map_err(|_| SessionStoreError::LimitOverflow)?;
            *remaining = remaining
                .checked_sub(consumed)
                .ok_or(SessionStoreError::LimitOverflow)?;
            if line.is_empty() {
                return Err(SessionStoreError::CorruptEvent("blank JSONL record"));
            }
            return Ok(Some(
                u64::try_from(line.len())
                    .map_err(|_| SessionStoreError::LimitOverflow)?
                    .checked_add(1)
                    .ok_or(SessionStoreError::LimitOverflow)?,
            ));
        }
        if line.len().saturating_add(chunk.len()) > max_line_bytes {
            return Err(SessionStoreError::EventRecordTooLarge { max_line_bytes });
        }
        line.extend_from_slice(chunk);
        reader.consume(available_len);
        let consumed =
            u64::try_from(available_len).map_err(|_| SessionStoreError::LimitOverflow)?;
        *remaining = remaining
            .checked_sub(consumed)
            .ok_or(SessionStoreError::LimitOverflow)?;
        if *remaining == 0 {
            return Err(SessionStoreError::CorruptEvent(
                "incomplete final JSONL record",
            ));
        }
    }
}

#[cfg(unix)]
struct EventFileSnapshot {
    stat: rustix::fs::Stat,
}

#[cfg(unix)]
impl EventFileSnapshot {
    fn len(&self) -> u64 {
        u64::try_from(self.stat.st_size).unwrap_or(u64::MAX)
    }
}

#[cfg(unix)]
fn event_file_snapshot(file: &File) -> Result<EventFileSnapshot, SessionStoreError> {
    let stat = rustix::fs::fstat(file).map_err(std::io::Error::from)?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
        return Err(SessionStoreError::UnsafeEventFileType);
    }
    u64::try_from(stat.st_size).map_err(|_| SessionStoreError::LimitOverflow)?;
    Ok(EventFileSnapshot { stat })
}

#[cfg(unix)]
fn verify_event_file_snapshot(
    file: &File,
    before: &EventFileSnapshot,
) -> Result<(), SessionStoreError> {
    let after = rustix::fs::fstat(file).map_err(std::io::Error::from)?;
    if !rustix::fs::FileType::from_raw_mode(after.st_mode).is_file()
        || after.st_nlink != 1
        || after.st_dev != before.stat.st_dev
        || after.st_ino != before.stat.st_ino
        || after.st_size != before.stat.st_size
        || after.st_mtime != before.stat.st_mtime
        || after.st_mtime_nsec != before.stat.st_mtime_nsec
        || after.st_ctime != before.stat.st_ctime
        || after.st_ctime_nsec != before.stat.st_ctime_nsec
    {
        return Err(SessionStoreError::EventFileChangedDuringRead);
    }
    Ok(())
}

#[cfg(not(unix))]
struct EventFileSnapshot {
    len: u64,
    modified: std::time::SystemTime,
}

#[cfg(not(unix))]
impl EventFileSnapshot {
    const fn len(&self) -> u64 {
        self.len
    }
}

#[cfg(not(unix))]
fn event_file_snapshot(file: &File) -> Result<EventFileSnapshot, SessionStoreError> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(SessionStoreError::UnsafeEventFileType);
    }
    Ok(EventFileSnapshot {
        len: metadata.len(),
        modified: metadata.modified()?,
    })
}

#[cfg(not(unix))]
fn verify_event_file_snapshot(
    file: &File,
    before: &EventFileSnapshot,
) -> Result<(), SessionStoreError> {
    let after = file.metadata()?;
    if !after.file_type().is_file()
        || after.len() != before.len
        || after.modified()? != before.modified
    {
        return Err(SessionStoreError::EventFileChangedDuringRead);
    }
    Ok(())
}

fn parse_events<T: DeserializeOwned>(
    bytes: &[u8],
) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
    parse_events_bounded(bytes, usize::MAX)
}

fn parse_events_bounded<T: DeserializeOwned>(
    bytes: &[u8],
    max_events: usize,
) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
    parse_events_bounded_from_sequence(bytes, max_events, 0)
}

fn parse_events_bounded_from_sequence<T: DeserializeOwned>(
    bytes: &[u8],
    max_events: usize,
    first_sequence: u64,
) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
    let mut events = Vec::new();
    for line in BufReader::new(bytes).lines() {
        if events.len() >= max_events {
            return Err(SessionStoreError::EventCountTooLarge { max_events });
        }
        let line = line?;
        if line.is_empty() {
            return Err(SessionStoreError::CorruptEvent("blank JSONL record"));
        }
        let envelope: EventEnvelope<T> = serde_json::from_str(&line)?;
        if envelope.schema_version != EVENT_SCHEMA_VERSION {
            return Err(SessionStoreError::UnsupportedEventVersion(
                envelope.schema_version,
            ));
        }
        let expected = first_sequence
            .checked_add(
                u64::try_from(events.len()).map_err(|_| SessionStoreError::SequenceOverflow)?,
            )
            .ok_or(SessionStoreError::SequenceOverflow)?;
        if envelope.sequence != SequenceId(expected) {
            return Err(SessionStoreError::CorruptEvent(
                "non-contiguous event sequence",
            ));
        }
        events.push(envelope);
    }
    Ok(events)
}

#[cfg(unix)]
fn read_opened_file(file: &File) -> Result<Vec<u8>, SessionStoreError> {
    read_opened_file_bounded(file, u64::MAX)
}

#[cfg(unix)]
fn read_opened_file_bounded(file: &File, max_bytes: u64) -> Result<Vec<u8>, SessionStoreError> {
    let stat = rustix::fs::fstat(file).map_err(std::io::Error::from)?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
        return Err(SessionStoreError::UnsafeEventFileType);
    }
    let file_bytes = u64::try_from(stat.st_size).map_err(|_| SessionStoreError::LimitOverflow)?;
    if file_bytes > max_bytes {
        return Err(SessionStoreError::EventLogTooLarge { max_bytes });
    }
    let length = usize::try_from(file_bytes).map_err(|_| SessionStoreError::LimitOverflow)?;
    let mut bytes = vec![0_u8; length];
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let position = u64::try_from(offset).map_err(|_| SessionStoreError::LimitOverflow)?;
        let read = file.read_at(&mut bytes[offset..], position)?;
        if read == 0 {
            bytes.truncate(offset);
            break;
        }
        offset = offset
            .checked_add(read)
            .ok_or(SessionStoreError::LimitOverflow)?;
    }
    let after = rustix::fs::fstat(file).map_err(std::io::Error::from)?;
    if !rustix::fs::FileType::from_raw_mode(after.st_mode).is_file()
        || after.st_nlink != 1
        || after.st_dev != stat.st_dev
        || after.st_ino != stat.st_ino
        || after.st_size != stat.st_size
        || after.st_mtime != stat.st_mtime
        || after.st_mtime_nsec != stat.st_mtime_nsec
        || after.st_ctime != stat.st_ctime
        || after.st_ctime_nsec != stat.st_ctime_nsec
    {
        return Err(SessionStoreError::EventFileChangedDuringRead);
    }
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_opened_file(file: &File) -> Result<Vec<u8>, SessionStoreError> {
    read_opened_file_bounded(file, u64::MAX)
}

#[cfg(not(unix))]
fn read_opened_file_bounded(file: &File, max_bytes: u64) -> Result<Vec<u8>, SessionStoreError> {
    let mut file = file.try_clone()?;
    if file.metadata()?.len() > max_bytes {
        return Err(SessionStoreError::EventLogTooLarge { max_bytes });
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_err(|_| SessionStoreError::LimitOverflow)? > max_bytes {
        return Err(SessionStoreError::EventLogTooLarge { max_bytes });
    }
    Ok(bytes)
}

fn ensure_regular_file(file: &File) -> Result<(), SessionStoreError> {
    if file.metadata()?.file_type().is_file() {
        Ok(())
    } else {
        Err(SessionStoreError::UnsafeEventFileType)
    }
}

#[cfg(unix)]
fn open_session_file(root: &Path, session_id: &str) -> Result<File, SessionStoreError> {
    fs::create_dir_all(root)?;
    let root = File::open(root)?;
    if !root.metadata()?.is_dir() {
        return Err(SessionStoreError::UnsafeSessionDirectory);
    }
    let sessions = open_or_create_directory(&root, "sessions")?;
    let session = open_or_create_directory(&sessions, session_id)?;
    open_or_create_event_file(&session)
}

#[cfg(unix)]
fn open_existing_session_file(root: &Path, session_id: &str) -> Result<File, SessionStoreError> {
    let root = File::open(root)?;
    if !root.metadata()?.is_dir() {
        return Err(SessionStoreError::UnsafeSessionDirectory);
    }
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::CLOEXEC
        | rustix::fs::OFlags::NOFOLLOW;
    let sessions = File::from(
        rustix::fs::openat(&root, "sessions", flags, rustix::fs::Mode::empty())
            .map_err(std::io::Error::from)?,
    );
    let session = File::from(
        rustix::fs::openat(&sessions, session_id, flags, rustix::fs::Mode::empty())
            .map_err(std::io::Error::from)?,
    );
    let event_flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::CLOEXEC
        | rustix::fs::OFlags::NOFOLLOW;
    rustix::fs::openat(
        &session,
        "events.jsonl",
        event_flags,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| std::io::Error::from(error).into())
}

#[cfg(unix)]
fn open_or_create_directory(parent: &File, name: &str) -> Result<File, SessionStoreError> {
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::CLOEXEC
        | rustix::fs::OFlags::NOFOLLOW;
    match rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty()) {
        Ok(descriptor) => Ok(File::from(descriptor)),
        Err(rustix::io::Errno::NOENT) => {
            match rustix::fs::mkdirat(parent, name, rustix::fs::Mode::from_bits_truncate(0o700)) {
                Ok(()) => sync_event_file(parent)?,
                Err(rustix::io::Errno::EXIST) => {}
                Err(source) => return Err(std::io::Error::from(source).into()),
            }
            let descriptor = rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty())
                .map_err(std::io::Error::from)?;
            Ok(File::from(descriptor))
        }
        Err(source) => Err(std::io::Error::from(source).into()),
    }
}

#[cfg(unix)]
fn open_or_create_event_file(parent: &File) -> Result<File, SessionStoreError> {
    let flags = rustix::fs::OFlags::RDWR
        | rustix::fs::OFlags::APPEND
        | rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::CLOEXEC
        | rustix::fs::OFlags::NOFOLLOW;
    match rustix::fs::openat(parent, "events.jsonl", flags, rustix::fs::Mode::empty()) {
        Ok(descriptor) => Ok(File::from(descriptor)),
        Err(rustix::io::Errno::NOENT) => {
            let created = rustix::fs::openat(
                parent,
                "events.jsonl",
                flags | rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL,
                rustix::fs::Mode::from_bits_truncate(0o600),
            );
            match created {
                Ok(descriptor) => {
                    let file = File::from(descriptor);
                    sync_event_file(&file)?;
                    sync_event_file(parent)?;
                    Ok(file)
                }
                Err(rustix::io::Errno::EXIST) => {
                    let descriptor = rustix::fs::openat(
                        parent,
                        "events.jsonl",
                        flags,
                        rustix::fs::Mode::empty(),
                    )
                    .map_err(std::io::Error::from)?;
                    Ok(File::from(descriptor))
                }
                Err(source) => Err(std::io::Error::from(source).into()),
            }
        }
        Err(source) => Err(std::io::Error::from(source).into()),
    }
}

#[cfg(not(unix))]
fn open_session_file_portable(
    root: &Path,
    directory: &Path,
    path: &Path,
) -> Result<File, SessionStoreError> {
    create_checked_directory_portable(root)?;
    create_checked_directory_portable(&root.join("sessions"))?;
    create_checked_directory_portable(directory)?;
    let file = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            OpenOptions::new().read(true).append(true).open(path)?
        }
        Ok(_) => return Err(SessionStoreError::UnsafeEventFileType),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => OpenOptions::new()
            .create_new(true)
            .read(true)
            .append(true)
            .open(path)?,
        Err(source) => return Err(source.into()),
    };
    sync_event_file(&file)?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_existing_session_file_portable(
    root: &Path,
    session_id: &str,
) -> Result<File, SessionStoreError> {
    let sessions = root.join("sessions");
    let directory = sessions.join(session_id);
    let path = directory.join("events.jsonl");
    for candidate in [root, sessions.as_path(), directory.as_path()] {
        let metadata = fs::symlink_metadata(candidate)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(SessionStoreError::UnsafeSessionDirectory);
        }
    }
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SessionStoreError::UnsafeEventFileType);
    }
    File::open(path).map_err(Into::into)
}

#[cfg(not(unix))]
fn create_checked_directory_portable(path: &Path) -> Result<(), SessionStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(SessionStoreError::UnsafeSessionDirectory),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            Ok(())
        }
        Err(source) => Err(source.into()),
    }
}

#[cfg(unix)]
fn sync_event_file(file: &File) -> std::io::Result<()> {
    rustix::fs::fsync(file).map_err(std::io::Error::from)
}

#[cfg(not(unix))]
fn sync_event_file(file: &File) -> std::io::Result<()> {
    file.sync_all()
}

#[cfg(unix)]
fn lock_writer(file: &File) -> Result<(), SessionStoreError> {
    const MAX_TRANSIENT_RETRIES: usize = 20;
    for attempt in 0..=MAX_TRANSIENT_RETRIES {
        match rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(()),
            Err(source)
                if source.kind() == std::io::ErrorKind::WouldBlock
                    && attempt < MAX_TRANSIENT_RETRIES =>
            {
                // A concurrent fork can briefly inherit the old writer's descriptor before
                // CLOEXEC closes it. Bound the retry so a real second writer still fails closed.
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(source) => return Err(std::io::Error::from(source).into()),
        }
    }
    unreachable!("bounded writer-lock loop always returns")
}

#[cfg(not(unix))]
fn lock_writer(_file: &File) -> Result<(), SessionStoreError> {
    Ok(())
}

fn validate_session_id(value: &str) -> Result<(), SessionStoreError> {
    if value.is_empty()
        || value.len() > 128
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(SessionStoreError::InvalidSessionId);
    }
    Ok(())
}

/// Session log/index failure without transcript contents in diagnostics.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    /// Session ids are path components and must use the restricted alphabet.
    #[error("session id is empty, too long, or contains unsafe characters")]
    InvalidSessionId,
    /// Session storage components must be real directories, not links or special files.
    #[error("session storage contains an unsafe directory component")]
    UnsafeSessionDirectory,
    /// The authoritative log must be a regular, non-symlink file.
    #[error("session event log is not a regular file")]
    UnsafeEventFileType,
    /// An unlocked external writer changed the log during a descriptor-stable read.
    #[error("session event log changed while it was being read")]
    EventFileChangedDuringRead,
    /// Parent and child identities in a fork must differ.
    #[error("session fork identities conflict")]
    ForkIdentityConflict,
    /// The requested parent event cursor does not exist.
    #[error("session fork source cursor does not exist")]
    ForkSourceCursorMissing,
    /// An existing child log is not the requested parent prefix.
    #[error("session fork target contains conflicting events")]
    ForkTargetConflict,
    #[error("session event log exceeds the {max_bytes}-byte read limit")]
    EventLogTooLarge { max_bytes: u64 },
    #[error("session event log exceeds the {max_events}-event read limit")]
    EventCountTooLarge { max_events: usize },
    /// Paged scans require positive, independently bounded limits.
    #[error("session event page limits must all be greater than zero")]
    InvalidEventPageLimits,
    /// The descriptor snapshot exceeded the caller's total scan budget.
    #[error("session event log exceeds the {max_bytes}-byte page scan limit")]
    EventScanBytesExceeded { max_bytes: u64 },
    /// The validated envelope count exceeded the caller's total scan budget.
    #[error("session event log exceeds the {max_events}-event page scan limit")]
    EventScanCountExceeded { max_events: u64 },
    /// A single JSONL record exceeded the bounded line buffer.
    #[error("session event record exceeds the {max_line_bytes}-byte line limit")]
    EventRecordTooLarge { max_line_bytes: usize },
    /// One legal event cannot fit in an otherwise empty requested page.
    #[error("session event requires {required_bytes} bytes but the page byte limit is {max_bytes}")]
    EventPageByteLimitTooSmall { required_bytes: u64, max_bytes: u64 },
    /// Cursor must identify an event in the captured snapshot.
    #[error("session event page cursor is ahead of the durable log tail")]
    EventPageCursorAhead,
    #[error("session search query exceeds 512 bytes")]
    SearchQueryTooLarge,
    #[error("session search internal limit exceeds 1001")]
    SearchLimitTooLarge,
    #[error("accounting query limit exceeds 1000000 entries")]
    AccountingQueryLimitTooLarge,
    #[error("accounting query exceeds the {max_entries}-entry read limit")]
    AccountingResultTooLarge { max_entries: usize },
    #[error("session search index is missing or has an unsafe file identity")]
    UnsafeSessionIndex,
    #[error("session search {component} exceeds the {max_bytes}-byte snapshot limit")]
    SessionIndexSnapshotTooLarge {
        /// Derived index component which exceeded its independent ceiling.
        component: &'static str,
        /// Maximum bytes the read-only search snapshot accepts for this component.
        max_bytes: u64,
    },
    /// A complete JSONL record was structurally corrupt.
    #[error("session event log is corrupt: {0}")]
    CorruptEvent(&'static str),
    /// An append failed and the original log length could not be restored durably.
    #[error("session event append failed and rollback could not be completed")]
    AppendRollbackFailed {
        /// Original write, flush, or synchronization failure.
        #[source]
        append: std::io::Error,
        /// Failure while truncating or synchronizing the rollback.
        rollback: std::io::Error,
    },
    /// An earlier append rollback failed, so this writer cannot append safely.
    #[error("session event writer is poisoned after an incomplete append rollback")]
    EventWriterPoisoned,
    /// A derived index row stored a malformed decimal watermark.
    #[error("session index projection watermark is corrupt")]
    CorruptProjectionWatermark,
    /// Turn/sequence identity in an accounting projection is malformed.
    #[error("accounting entry identity is invalid")]
    InvalidAccountingIdentity,
    /// Accounting timestamps must be normalized UTC values with a matching day key.
    #[error("accounting timestamp or UTC day key is invalid")]
    InvalidAccountingTimestamp,
    /// The same durable turn or sequence was projected with different accounting data.
    #[error("accounting projection conflicts with an existing durable event identity")]
    AccountingConflict,
    /// Accumulated accounting values exceeded their lossless representation.
    #[error("accounting total overflow")]
    AccountingOverflow,
    /// A pre-sequenced event did not match the durable log tail.
    #[error("session event sequence mismatch: expected {expected:?}, got {actual:?}")]
    UnexpectedEventSequence {
        /// Sequence which could be safely appended.
        expected: SequenceId,
        /// Sequence supplied by the caller.
        actual: SequenceId,
    },
    /// The reader does not understand this schema version.
    #[error("unsupported session event schema version {0}")]
    UnsupportedEventVersion(u16),
    /// Event sequence cannot be represented.
    #[error("session event sequence overflow")]
    SequenceOverflow,
    /// Caller-supplied query limit cannot be represented by `SQLite`.
    #[error("session query limit overflow")]
    LimitOverflow,
    /// Filesystem failure.
    #[error("session storage I/O failed")]
    Io(#[from] std::io::Error),
    /// JSON failure. Payload contents are intentionally omitted.
    #[error("session event JSON is invalid")]
    Json(#[from] serde_json::Error),
    /// `SQLite` failure. `SQLite`'s structural diagnostic is retained.
    #[error("session index failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize, Serializer, ser::Error as _};
    use tempfile::tempdir;

    use rw_types::{AccountingAttribution, Cost, SequenceId, TurnId, Usage};

    use super::{
        AccountingLedger, EventEnvelope, MAX_SEARCH_INDEX_BYTES, MAX_SEARCH_INDEX_WAL_BYTES,
        ProjectionStatus, SessionEventLog, SessionEventPageLimits, SessionIndex, SessionProjection,
        SessionStoreError, SessionSummary, TurnAccountingEntry, UtcDayKey, UtcTimestamp,
        install_append_fault, upsert_projection,
    };

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    struct FixtureEvent {
        kind: String,
        text: String,
    }

    #[test]
    fn fork_copies_exact_prefix_and_parent_and_child_diverge_independently() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let mut parent = SessionEventLog::open(root.path(), "parent")
            .unwrap_or_else(|error| panic!("parent must open: {error}"));
        let events = (0..3)
            .map(|index| FixtureEvent {
                kind: "parent".to_owned(),
                text: format!("event-{index}"),
            })
            .collect::<Vec<_>>();
        parent
            .append_batch(events.clone())
            .unwrap_or_else(|error| panic!("parent events must append: {error}"));
        drop(parent);

        let mut child = SessionEventLog::fork(root.path(), "parent", "child", Some(SequenceId(1)))
            .unwrap_or_else(|error| panic!("child fork must succeed: {error}"));
        assert_eq!(
            child
                .load::<FixtureEvent>()
                .unwrap_or_else(|error| panic!("child prefix must load: {error}"))
                .into_iter()
                .map(|event| event.event)
                .collect::<Vec<_>>(),
            events[..2]
        );
        let parent_bytes = std::fs::read(root.path().join("sessions/parent/events.jsonl"))
            .unwrap_or_else(|error| panic!("parent bytes must read: {error}"));
        let child_bytes = std::fs::read(child.path())
            .unwrap_or_else(|error| panic!("child bytes must read: {error}"));
        let expected_prefix_len = parent_bytes
            .split_inclusive(|byte| *byte == b'\n')
            .take(2)
            .map(<[u8]>::len)
            .sum::<usize>();
        assert_eq!(child_bytes, parent_bytes[..expected_prefix_len]);
        child
            .append(FixtureEvent {
                kind: "child".to_owned(),
                text: "diverged".to_owned(),
            })
            .unwrap_or_else(|error| panic!("child divergence must append: {error}"));
        drop(child);

        let mut parent = SessionEventLog::open(root.path(), "parent")
            .unwrap_or_else(|error| panic!("parent must reopen: {error}"));
        parent
            .append(FixtureEvent {
                kind: "parent".to_owned(),
                text: "continued".to_owned(),
            })
            .unwrap_or_else(|error| panic!("parent continuation must append: {error}"));
        drop(parent);

        let parent = SessionEventLog::load_existing::<FixtureEvent>(root.path(), "parent")
            .unwrap_or_else(|error| panic!("parent must load: {error}"));
        let child = SessionEventLog::load_existing::<FixtureEvent>(root.path(), "child")
            .unwrap_or_else(|error| panic!("child must load: {error}"));
        assert_eq!(parent.len(), 4);
        assert_eq!(child.len(), 3);
        assert_eq!(parent[2].event, events[2]);
        assert_eq!(parent[3].event.text, "continued");
        assert_eq!(child[2].event.kind, "child");
        assert!(matches!(
            SessionEventLog::fork(root.path(), "parent", "child", Some(SequenceId(1))),
            Err(SessionStoreError::ForkTargetConflict)
        ));
        assert!(matches!(
            SessionEventLog::fork(root.path(), "parent", "parent", None),
            Err(SessionStoreError::ForkIdentityConflict)
        ));
    }

    #[test]
    fn fork_resumes_an_exact_partial_child_prefix_idempotently() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let events = (0..3)
            .map(|index| FixtureEvent {
                kind: "fixture".to_owned(),
                text: format!("event-{index}"),
            })
            .collect::<Vec<_>>();
        let mut parent = SessionEventLog::open(root.path(), "parent")
            .unwrap_or_else(|error| panic!("parent must open: {error}"));
        parent
            .append_batch(events.clone())
            .unwrap_or_else(|error| panic!("parent must append: {error}"));
        drop(parent);
        let mut partial = SessionEventLog::open(root.path(), "partial")
            .unwrap_or_else(|error| panic!("partial child must open: {error}"));
        partial
            .append(events[0].clone())
            .unwrap_or_else(|error| panic!("partial child must append: {error}"));
        drop(partial);

        let completed =
            SessionEventLog::fork(root.path(), "parent", "partial", Some(SequenceId(2)))
                .unwrap_or_else(|error| panic!("partial fork must recover: {error}"));
        assert_eq!(
            completed
                .load::<FixtureEvent>()
                .unwrap_or_else(|error| panic!("completed child must load: {error}"))
                .into_iter()
                .map(|event| event.event)
                .collect::<Vec<_>>(),
            events
        );
    }

    fn accounting_entry(
        session_id: &str,
        turn: u64,
        sequence: u64,
        emitted_at_utc: &str,
        cost: Cost,
    ) -> TurnAccountingEntry {
        let emitted_at_utc = UtcTimestamp::parse(emitted_at_utc)
            .unwrap_or_else(|error| panic!("fixture timestamp must parse: {error}"));
        TurnAccountingEntry {
            session_id: session_id.to_owned(),
            turn_id: TurnId(turn.to_string()),
            sequence_id: SequenceId(sequence),
            utc_day: emitted_at_utc.utc_day(),
            emitted_at_utc,
            attribution: AccountingAttribution::Main,
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            cost,
        }
    }

    fn utc_day(value: &str) -> UtcDayKey {
        UtcDayKey::parse(value).unwrap_or_else(|error| panic!("fixture day must parse: {error}"))
    }

    fn utc_timestamp(value: &str) -> UtcTimestamp {
        UtcTimestamp::parse(value)
            .unwrap_or_else(|error| panic!("fixture timestamp must parse: {error}"))
    }

    struct FailableEvent {
        text: &'static str,
        fail: bool,
    }

    impl Serialize for FailableEvent {
        fn serialize<SerializerType>(
            &self,
            serializer: SerializerType,
        ) -> Result<SerializerType::Ok, SerializerType::Error>
        where
            SerializerType: Serializer,
        {
            if self.fail {
                Err(SerializerType::Error::custom(
                    "fixture serialization failure",
                ))
            } else {
                serializer.serialize_str(self.text)
            }
        }
    }

    #[test]
    fn killed_partial_tail_is_truncated_and_sequence_resumes() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let mut log = SessionEventLog::open(root.path(), "session-1")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        log.append(FixtureEvent {
            kind: "user".to_owned(),
            text: "complete".to_owned(),
        })
        .unwrap_or_else(|error| panic!("event must append: {error}"));
        let mut file = OpenOptions::new()
            .append(true)
            .open(log.path())
            .unwrap_or_else(|error| panic!("tail file must open: {error}"));
        file.write_all(br#"{"schema_version":1,"sequence":1,"event":{"kind":"assistant""#)
            .unwrap_or_else(|error| panic!("partial tail must write: {error}"));
        file.sync_data()
            .unwrap_or_else(|error| panic!("partial tail must sync: {error}"));
        drop(file);
        drop(log);

        let mut recovered = SessionEventLog::open(root.path(), "session-1")
            .unwrap_or_else(|error| panic!("partial tail must recover: {error}"));
        assert_eq!(recovered.next_sequence(), 1);
        recovered
            .append(FixtureEvent {
                kind: "assistant".to_owned(),
                text: "resumed".to_owned(),
            })
            .unwrap_or_else(|error| panic!("resumed event must append: {error}"));
        let events = recovered
            .load::<FixtureEvent>()
            .unwrap_or_else(|error| panic!("events must load: {error}"));
        assert_eq!(
            events,
            vec![
                EventEnvelope {
                    schema_version: 1,
                    sequence: SequenceId(0),
                    event: FixtureEvent {
                        kind: "user".to_owned(),
                        text: "complete".to_owned(),
                    },
                },
                EventEnvelope {
                    schema_version: 1,
                    sequence: SequenceId(1),
                    event: FixtureEvent {
                        kind: "assistant".to_owned(),
                        text: "resumed".to_owned(),
                    },
                },
            ]
        );
    }

    #[test]
    fn partial_append_failure_rolls_back_and_the_writer_can_continue() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let mut log = SessionEventLog::open(root.path(), "partial-append")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        log.append(FixtureEvent {
            kind: "user".to_owned(),
            text: "durable prefix".to_owned(),
        })
        .unwrap_or_else(|error| panic!("prefix must append: {error}"));
        let before = std::fs::read(log.path())
            .unwrap_or_else(|error| panic!("prefix bytes must read: {error}"));

        let fault = install_append_fault(7, false);
        assert!(matches!(
            log.append(FixtureEvent {
                kind: "assistant".to_owned(),
                text: "must roll back".to_owned(),
            }),
            Err(SessionStoreError::Io(_))
        ));
        assert_eq!(
            std::fs::read(log.path()).unwrap_or_else(|error| panic!("rolled-back bytes: {error}")),
            before
        );
        drop(fault);

        log.append(FixtureEvent {
            kind: "assistant".to_owned(),
            text: "clean retry".to_owned(),
        })
        .unwrap_or_else(|error| panic!("retry must append: {error}"));
        drop(log);

        let recovered = SessionEventLog::open(root.path(), "partial-append")
            .unwrap_or_else(|error| panic!("clean retry must recover: {error}"));
        assert_eq!(recovered.next_sequence(), 2);
        assert_eq!(
            recovered
                .load::<FixtureEvent>()
                .unwrap_or_else(|error| panic!("recovered events must load: {error}"))
                .into_iter()
                .map(|event| event.event.text)
                .collect::<Vec<_>>(),
            vec!["durable prefix", "clean retry"]
        );
    }

    #[test]
    fn trailing_malformed_record_with_newline_fails_closed_without_truncating() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let mut log = SessionEventLog::open(root.path(), "malformed-tail")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        log.append(FixtureEvent {
            kind: "user".to_owned(),
            text: "complete".to_owned(),
        })
        .unwrap_or_else(|error| panic!("event must append: {error}"));
        let path = log.path().to_path_buf();
        let mut file = OpenOptions::new()
            .append(true)
            .open(log.path())
            .unwrap_or_else(|error| panic!("tail file must open: {error}"));
        file.write_all(b"{\"schema_version\":1,\"sequence\":1,\"event\":\n")
            .unwrap_or_else(|error| panic!("malformed tail must write: {error}"));
        file.sync_data()
            .unwrap_or_else(|error| panic!("malformed tail must sync: {error}"));
        drop(file);
        drop(log);

        let before_open =
            std::fs::read(&path).unwrap_or_else(|error| panic!("corrupt bytes must read: {error}"));
        assert!(matches!(
            SessionEventLog::open(root.path(), "malformed-tail"),
            Err(SessionStoreError::Json(_))
        ));
        assert_eq!(
            std::fs::read(path)
                .unwrap_or_else(|error| panic!("preserved bytes must read: {error}")),
            before_open
        );
    }

    #[test]
    fn trailing_unsupported_version_with_newline_fails_closed_without_truncating() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let log = SessionEventLog::open(root.path(), "unsupported-tail")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        let path = log.path().to_path_buf();
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap_or_else(|error| panic!("tail file must open: {error}"));
        file.write_all(b"{\"schema_version\":2,\"sequence\":\"0\",\"event\":{}}\n")
            .unwrap_or_else(|error| panic!("unsupported tail must write: {error}"));
        file.sync_data()
            .unwrap_or_else(|error| panic!("unsupported tail must sync: {error}"));
        drop(file);
        drop(log);

        let before_open = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("unsupported bytes must read: {error}"));
        assert!(matches!(
            SessionEventLog::open(root.path(), "unsupported-tail"),
            Err(SessionStoreError::UnsupportedEventVersion(2))
        ));
        assert_eq!(
            std::fs::read(path)
                .unwrap_or_else(|error| panic!("preserved bytes must read: {error}")),
            before_open
        );
    }

    #[test]
    fn trailing_non_contiguous_record_with_newline_fails_closed_without_truncating() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let log = SessionEventLog::open(root.path(), "non-contiguous-tail")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        let path = log.path().to_path_buf();
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap_or_else(|error| panic!("tail file must open: {error}"));
        file.write_all(b"{\"schema_version\":1,\"sequence\":\"1\",\"event\":{}}\n")
            .unwrap_or_else(|error| panic!("non-contiguous tail must write: {error}"));
        file.sync_data()
            .unwrap_or_else(|error| panic!("non-contiguous tail must sync: {error}"));
        drop(file);
        drop(log);

        let before_open = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("non-contiguous bytes must read: {error}"));
        assert!(matches!(
            SessionEventLog::open(root.path(), "non-contiguous-tail"),
            Err(SessionStoreError::CorruptEvent(
                "non-contiguous event sequence"
            ))
        ));
        assert_eq!(
            std::fs::read(path)
                .unwrap_or_else(|error| panic!("preserved bytes must read: {error}")),
            before_open
        );
    }

    #[test]
    fn malformed_record_before_the_tail_fails_closed() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let mut log = SessionEventLog::open(root.path(), "malformed-middle")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        log.append(FixtureEvent {
            kind: "user".to_owned(),
            text: "complete".to_owned(),
        })
        .unwrap_or_else(|error| panic!("event must append: {error}"));
        let mut file = OpenOptions::new()
            .append(true)
            .open(log.path())
            .unwrap_or_else(|error| panic!("tail file must open: {error}"));
        file.write_all(b"{\"schema_version\":1,\"sequence\":1,\"event\":\n")
            .unwrap_or_else(|error| panic!("malformed middle must write: {error}"));
        file.write_all(
            br#"{"schema_version":1,"sequence":1,"event":{"kind":"assistant","text":"later"}}"#,
        )
        .unwrap_or_else(|error| panic!("later event must write: {error}"));
        file.write_all(b"\n")
            .unwrap_or_else(|error| panic!("later event delimiter must write: {error}"));
        file.sync_data()
            .unwrap_or_else(|error| panic!("malformed middle must sync: {error}"));
        drop(file);
        drop(log);

        assert!(matches!(
            SessionEventLog::open(root.path(), "malformed-middle"),
            Err(SessionStoreError::Json(_))
        ));
    }

    #[test]
    fn failed_rollback_poisons_the_writer() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let mut log = SessionEventLog::open(root.path(), "poisoned-writer")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        let fault = install_append_fault(1, true);
        assert!(matches!(
            log.append(FixtureEvent {
                kind: "user".to_owned(),
                text: "will fail".to_owned(),
            }),
            Err(SessionStoreError::AppendRollbackFailed { .. })
        ));
        drop(fault);
        assert!(matches!(
            log.append(FixtureEvent {
                kind: "user".to_owned(),
                text: "must not write".to_owned(),
            }),
            Err(SessionStoreError::EventWriterPoisoned)
        ));
    }

    #[test]
    fn batch_append_assigns_exact_envelopes_and_serialization_is_all_or_nothing() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let mut log = SessionEventLog::open(root.path(), "batch")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        let appended = log
            .append_batch([
                FixtureEvent {
                    kind: "turn_started".to_owned(),
                    text: "one".to_owned(),
                },
                FixtureEvent {
                    kind: "user_message".to_owned(),
                    text: "two".to_owned(),
                },
            ])
            .unwrap_or_else(|error| panic!("batch must append: {error}"));
        assert_eq!(
            appended
                .iter()
                .map(|envelope| envelope.sequence)
                .collect::<Vec<_>>(),
            vec![SequenceId(0), SequenceId(1)]
        );
        assert_eq!(
            log.load::<FixtureEvent>()
                .unwrap_or_else(|error| panic!("batch must load: {error}")),
            appended
        );

        let before = std::fs::read(log.path())
            .unwrap_or_else(|error| panic!("batch log must read: {error}"));
        assert!(
            log.append_batch([
                FailableEvent {
                    text: "serializable",
                    fail: false,
                },
                FailableEvent {
                    text: "fails",
                    fail: true,
                },
            ])
            .is_err()
        );
        assert_eq!(log.next_sequence(), 2);
        assert_eq!(
            std::fs::read(log.path())
                .unwrap_or_else(|error| panic!("batch log must reread: {error}")),
            before
        );
    }

    #[test]
    fn load_after_uses_an_exclusive_cursor_and_rejects_ahead() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let mut log = SessionEventLog::open(root.path(), "suffix")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        let appended = log
            .append_batch((0..4).map(|index| FixtureEvent {
                kind: "sample".to_owned(),
                text: index.to_string(),
            }))
            .unwrap_or_else(|error| panic!("events must append: {error}"));

        assert_eq!(
            log.load_after::<FixtureEvent>(None)
                .unwrap_or_else(|error| panic!("full suffix must load: {error}")),
            appended
        );
        assert_eq!(
            log.load_after::<FixtureEvent>(Some(SequenceId(1)))
                .unwrap_or_else(|error| panic!("tail suffix must load: {error}")),
            appended[2..]
        );
        assert!(
            log.load_after::<FixtureEvent>(Some(SequenceId(3)))
                .unwrap_or_else(|error| panic!("empty suffix must load: {error}"))
                .is_empty()
        );
        assert!(matches!(
            log.load_after::<FixtureEvent>(Some(SequenceId(4))),
            Err(SessionStoreError::EventPageCursorAhead)
        ));

        OpenOptions::new()
            .write(true)
            .open(log.path())
            .and_then(|file| file.set_len(0))
            .unwrap_or_else(|error| panic!("test truncation must succeed: {error}"));
        assert!(matches!(
            log.load_after::<FixtureEvent>(Some(SequenceId(1))),
            Err(SessionStoreError::CorruptEvent(
                "event log is shorter than its durable tail"
            ))
        ));
    }

    #[test]
    fn five_hundred_event_batch_round_trips_contiguously() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let mut log = SessionEventLog::open(root.path(), "five-hundred")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        let events = (0..500).map(|index| FixtureEvent {
            kind: "sample".to_owned(),
            text: index.to_string(),
        });
        let appended = log
            .append_batch(events)
            .unwrap_or_else(|error| panic!("batch must append: {error}"));
        assert_eq!(appended.len(), 500);
        assert_eq!(
            appended.last().map(|event| event.sequence),
            Some(SequenceId(499))
        );
        assert_eq!(
            log.load::<FixtureEvent>()
                .unwrap_or_else(|error| panic!("batch must load: {error}")),
            appended
        );
    }

    #[test]
    fn sqlite_index_lists_updates_and_searches_transcripts() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let index = SessionIndex::open(root.path())
            .unwrap_or_else(|error| panic!("index must open: {error}"));
        let first = SessionSummary {
            id: "first".to_owned(),
            title: "Rust parser".to_owned(),
            updated_unix_ms: 10,
            cost_micros: 7,
        };
        let second = SessionSummary {
            id: "second".to_owned(),
            title: "TypeScript UI".to_owned(),
            updated_unix_ms: 20,
            cost_micros: 9,
        };
        index
            .upsert(&SessionProjection {
                summary: first.clone(),
                transcript: "implemented a resilient parser".to_owned(),
                projected_through: Some(SequenceId(3)),
            })
            .unwrap_or_else(|error| panic!("first index row must write: {error}"));
        index
            .upsert(&SessionProjection {
                summary: second.clone(),
                transcript: "rendered terminal cells".to_owned(),
                projected_through: Some(SequenceId(8)),
            })
            .unwrap_or_else(|error| panic!("second index row must write: {error}"));
        assert_eq!(
            index
                .list(10)
                .unwrap_or_else(|error| panic!("sessions must list: {error}")),
            vec![second.clone(), first.clone()]
        );
        assert_eq!(
            index
                .search("resilient", 10)
                .unwrap_or_else(|error| panic!("sessions must search: {error}")),
            vec![first.clone()]
        );
        let updated = SessionSummary {
            title: "Rust event parser".to_owned(),
            updated_unix_ms: 30,
            cost_micros: 11,
            ..first
        };
        index
            .upsert(&SessionProjection {
                summary: updated.clone(),
                transcript: "recovered a truncated transcript".to_owned(),
                projected_through: Some(SequenceId(11)),
            })
            .unwrap_or_else(|error| panic!("updated row must write: {error}"));
        assert!(
            index
                .search("resilient", 10)
                .unwrap_or_else(|error| panic!("old search must work: {error}"))
                .is_empty()
        );
        assert_eq!(
            index
                .search("truncated", 10)
                .unwrap_or_else(|error| panic!("updated search must work: {error}")),
            vec![updated]
        );
        assert_eq!(
            index
                .projection_status("first", Some(SequenceId(11)))
                .unwrap_or_else(|error| panic!("watermark must query: {error}")),
            ProjectionStatus::Current
        );
        assert!(matches!(
            index
                .projection_status("first", Some(SequenceId(12)))
                .unwrap_or_else(|error| panic!("stale watermark must query: {error}")),
            ProjectionStatus::Stale { .. }
        ));
    }

    #[test]
    fn accounting_schema_migrates_an_existing_index_without_losing_rows() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let path = root.path().join("index.sqlite");
        let connection = rusqlite::Connection::open(&path)
            .unwrap_or_else(|error| panic!("legacy index must open: {error}"));
        connection
            .execute_batch(
                "CREATE TABLE legacy_marker(value TEXT NOT NULL); \
                 INSERT INTO legacy_marker(value) VALUES ('preserved'); \
                 CREATE TABLE turn_accounting( \
                   session_id TEXT NOT NULL, turn_id TEXT NOT NULL, \
                   sequence_id TEXT NOT NULL, emitted_at_utc TEXT NOT NULL, \
                   utc_day TEXT NOT NULL, cost_json TEXT NOT NULL, \
                   PRIMARY KEY(session_id,sequence_id), UNIQUE(session_id,turn_id) \
                 ); \
                 INSERT INTO turn_accounting( \
                   session_id,turn_id,sequence_id,emitted_at_utc,utc_day,cost_json \
                 ) VALUES ( \
                   'legacy-accounting','1','0','2026-01-01T00:00:00.000Z', \
                   '2026-01-01', \
                   '{\"kind\":\"monetary\",\"amount_micros\":\"5\",\"currency\":\"USD\"}' \
                 );",
            )
            .unwrap_or_else(|error| panic!("legacy schema must create: {error}"));
        drop(connection);

        let ledger = AccountingLedger::open(root.path())
            .unwrap_or_else(|error| panic!("accounting migration must open: {error}"));
        let legacy = ledger
            .entries_for_session("legacy-accounting")
            .unwrap_or_else(|error| panic!("legacy accounting must migrate: {error}"));
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].attribution, AccountingAttribution::Main);
        assert_eq!(
            legacy[0].usage,
            Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            }
        );
        let mut repeated_turn_charge = accounting_entry(
            "legacy-accounting",
            1,
            1,
            "2026-01-01T00:00:01.000Z",
            Cost::Monetary {
                amount_micros: 6,
                currency: "USD".to_owned(),
            },
        );
        repeated_turn_charge.attribution = AccountingAttribution::Compaction;
        ledger
            .record(&repeated_turn_charge)
            .unwrap_or_else(|error| panic!("same turn may carry another charge: {error}"));
        assert_eq!(
            ledger
                .entries_for_session("legacy-accounting")
                .unwrap_or_else(|error| panic!("repeated charges must query: {error}"))
                .len(),
            2
        );
        ledger
            .record(&accounting_entry(
                "migrated",
                1,
                0,
                "2026-01-01T00:00:00.000Z",
                Cost::Monetary {
                    amount_micros: 7,
                    currency: "USD".to_owned(),
                },
            ))
            .unwrap_or_else(|error| panic!("migrated ledger must record: {error}"));
        let connection = rusqlite::Connection::open(&path)
            .unwrap_or_else(|error| panic!("migrated index must reopen: {error}"));
        let marker = connection
            .query_row("SELECT value FROM legacy_marker", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap_or_else(|error| panic!("legacy marker must remain: {error}"));
        assert_eq!(marker, "preserved");
    }

    #[test]
    fn bounded_read_only_accounting_filters_utc_and_never_mutates_live_index() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let ledger = AccountingLedger::open(root.path())
            .unwrap_or_else(|error| panic!("ledger must open: {error}"));
        ledger
            .reconcile(&[
                accounting_entry(
                    "first",
                    1,
                    0,
                    "2026-07-09T23:59:59.999Z",
                    Cost::Monetary {
                        amount_micros: 1,
                        currency: "USD".to_owned(),
                    },
                ),
                accounting_entry(
                    "second",
                    1,
                    0,
                    "2026-07-10T00:00:00.000Z",
                    Cost::SubscriptionQuota {
                        used: Some("1".to_owned()),
                        unit: Some("request".to_owned()),
                    },
                ),
            ])
            .unwrap_or_else(|error| panic!("fixtures must reconcile: {error}"));
        drop(ledger);
        let paths = [
            root.path().join("index.sqlite"),
            root.path().join("index.sqlite-wal"),
            root.path().join("index.sqlite-shm"),
        ];
        let before = paths
            .iter()
            .map(|path| std::fs::read(path).ok())
            .collect::<Vec<_>>();
        let entries = AccountingLedger::entries_read_only_bounded(
            root.path(),
            &utc_timestamp("2026-07-10T00:00:00.000Z"),
            &utc_timestamp("2026-07-10T23:59:59.999Z"),
            10,
        )
        .unwrap_or_else(|error| panic!("read-only entries must load: {error}"));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "second");
        let after = paths
            .iter()
            .map(|path| std::fs::read(path).ok())
            .collect::<Vec<_>>();
        assert_eq!(after, before);

        assert!(matches!(
            AccountingLedger::entries_read_only_bounded(
                root.path(),
                &utc_timestamp("2026-07-09T00:00:00.000Z"),
                &utc_timestamp("2026-07-10T23:59:59.999Z"),
                1,
            ),
            Err(SessionStoreError::AccountingResultTooLarge { max_entries: 1 })
        ));
    }

    #[test]
    fn read_only_accounting_rejects_corrupt_typed_rows() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        AccountingLedger::open(root.path())
            .and_then(|ledger| {
                ledger.record(&accounting_entry(
                    "corrupt",
                    1,
                    0,
                    "2026-07-10T12:00:00.000Z",
                    Cost::Unavailable {
                        reason: "fixture".to_owned(),
                    },
                ))
            })
            .unwrap_or_else(|error| panic!("fixture must record: {error}"));
        rusqlite::Connection::open(root.path().join("index.sqlite"))
            .and_then(|connection| {
                connection.execute("UPDATE turn_accounting SET usage_json='not-json'", [])
            })
            .unwrap_or_else(|error| panic!("fixture must corrupt row: {error}"));
        assert!(
            AccountingLedger::entries_read_only_bounded(
                root.path(),
                &utc_timestamp("2026-07-10T00:00:00.000Z"),
                &utc_timestamp("2026-07-10T23:59:59.999Z"),
                10,
            )
            .is_err()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn accounting_totals_cross_utc_day_without_erasing_nonpriced_costs() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let ledger = AccountingLedger::open(root.path())
            .unwrap_or_else(|error| panic!("ledger must open: {error}"));
        let entries = vec![
            accounting_entry(
                "session-a",
                1,
                0,
                "2026-01-01T23:59:30.000Z",
                Cost::Monetary {
                    amount_micros: 100,
                    currency: "USD".to_owned(),
                },
            ),
            accounting_entry(
                "session-a",
                2,
                1,
                "2026-01-02T00:00:10.000Z",
                Cost::AiCredits {
                    credits_micros: 200,
                    nominal_amount_micros: None,
                    currency: None,
                },
            ),
            accounting_entry(
                "session-b",
                1,
                0,
                "2026-01-02T00:00:20.000Z",
                Cost::Monetary {
                    amount_micros: 300,
                    currency: "USD".to_owned(),
                },
            ),
            accounting_entry(
                "session-a",
                3,
                2,
                "2026-01-02T00:00:30.000Z",
                Cost::SubscriptionQuota {
                    used: Some("1".to_owned()),
                    unit: Some("request".to_owned()),
                },
            ),
            accounting_entry(
                "session-a",
                4,
                3,
                "2026-01-02T00:00:40.000Z",
                Cost::Unavailable {
                    reason: "subscription pricing unavailable".to_owned(),
                },
            ),
            accounting_entry(
                "session-a",
                5,
                4,
                "2026-01-02T00:00:50.000Z",
                Cost::Monetary {
                    amount_micros: 999,
                    currency: "EUR".to_owned(),
                },
            ),
            accounting_entry(
                "session-b",
                2,
                1,
                "2026-01-02T00:02:00.000Z",
                Cost::Monetary {
                    amount_micros: 5_000,
                    currency: "USD".to_owned(),
                },
            ),
        ];
        ledger
            .reconcile(&entries)
            .unwrap_or_else(|error| panic!("entries must reconcile: {error}"));
        let totals = ledger
            .totals(
                "session-a",
                &utc_day("2026-01-02"),
                &utc_timestamp("2026-01-01T23:59:45.000Z"),
                &utc_timestamp("2026-01-02T00:00:59.999Z"),
            )
            .unwrap_or_else(|error| panic!("totals must query: {error}"));

        assert_eq!(
            totals.utc_day_start_utc.as_str(),
            "2026-01-02T00:00:00.000Z"
        );
        assert_eq!(totals.session_micros_usd, 100);
        assert_eq!(totals.day_micros_usd, 300);
        assert_eq!(totals.trailing_session_micros_usd, 0);
        assert_eq!(totals.trailing_all_sessions_micros_usd, 300);
        assert_eq!(totals.session_ai_credit_micros, 200);
        assert_eq!(totals.day_ai_credit_micros, 200);
        assert_eq!(totals.trailing_session_ai_credit_micros, 200);
        assert_eq!(totals.trailing_all_sessions_ai_credit_micros, 200);
        assert_eq!(totals.session_subscription_quota_turns, 1);
        assert_eq!(totals.day_subscription_quota_turns, 1);
        assert_eq!(totals.session_unavailable_turns, 1);
        assert_eq!(totals.day_unavailable_turns, 1);
        assert_eq!(totals.session_non_usd_monetary_turns, 1);
        assert_eq!(totals.day_non_usd_monetary_turns, 1);
    }

    #[test]
    fn accounting_rebuild_is_idempotent_conflict_checked_and_rewind_independent() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let ledger = AccountingLedger::open(root.path())
            .unwrap_or_else(|error| panic!("ledger must open: {error}"));
        let paid = accounting_entry(
            "paid-session",
            1,
            4,
            "2026-02-03T04:05:06.007Z",
            Cost::Monetary {
                amount_micros: 41,
                currency: "USD".to_owned(),
            },
        );
        ledger
            .record(&paid)
            .and_then(|()| ledger.record(&paid))
            .unwrap_or_else(|error| panic!("duplicate projection must be idempotent: {error}"));
        assert_eq!(
            ledger
                .entries_for_session("paid-session")
                .unwrap_or_else(|error| panic!("entries must query: {error}")),
            vec![paid.clone()]
        );

        let mut conflicting = paid.clone();
        conflicting.cost = Cost::Monetary {
            amount_micros: 42,
            currency: "USD".to_owned(),
        };
        assert!(matches!(
            ledger.record(&conflicting),
            Err(SessionStoreError::AccountingConflict)
        ));

        let replacement = accounting_entry(
            "other-session",
            1,
            0,
            "2026-02-03T04:05:07.000Z",
            Cost::AiCredits {
                credits_micros: 9,
                nominal_amount_micros: None,
                currency: None,
            },
        );
        ledger
            .reconcile(&[])
            .unwrap_or_else(|error| panic!("empty rewind reconciliation must succeed: {error}"));
        assert_eq!(
            ledger
                .entries_for_session("paid-session")
                .unwrap_or_else(|error| panic!("paid history must query: {error}")),
            vec![paid.clone()]
        );

        ledger
            .replace_all(&[paid.clone(), replacement.clone()])
            .unwrap_or_else(|error| panic!("authoritative rebuild must replace rows: {error}"));
        assert_eq!(
            ledger
                .entries_for_session("paid-session")
                .unwrap_or_else(|error| panic!("rebuilt paid history must query: {error}")),
            vec![paid]
        );
        assert_eq!(
            ledger
                .entries_for_session("other-session")
                .unwrap_or_else(|error| panic!("replacement must query: {error}")),
            vec![replacement]
        );
    }

    #[test]
    fn accounting_rejects_invalid_dates_future_window_rows_and_overflow() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let ledger = AccountingLedger::open(root.path())
            .unwrap_or_else(|error| panic!("ledger must open: {error}"));
        assert_eq!(
            UtcTimestamp::from_unix_millis(0)
                .unwrap_or_else(|error| panic!("epoch must convert: {error}"))
                .as_str(),
            "1970-01-01T00:00:00.000Z"
        );
        assert_eq!(
            UtcTimestamp::from_unix_millis(1_709_164_800_123)
                .unwrap_or_else(|error| panic!("leap day must convert: {error}"))
                .as_str(),
            "2024-02-29T00:00:00.123Z"
        );
        for timestamp in [
            "2025-02-29T00:00:00.000Z",
            "2024-02-30T00:00:00.000Z",
            "2024-13-01T00:00:00.000Z",
            "2024-01-01T24:00:00.000Z",
            "2024-01-01T00:60:00.000Z",
        ] {
            assert!(matches!(
                UtcTimestamp::parse(timestamp),
                Err(SessionStoreError::InvalidAccountingTimestamp)
            ));
        }

        let entries = [
            accounting_entry(
                "overflow",
                1,
                0,
                "2026-03-01T00:00:00.000Z",
                Cost::Monetary {
                    amount_micros: u64::MAX,
                    currency: "USD".to_owned(),
                },
            ),
            accounting_entry(
                "overflow",
                2,
                1,
                "2026-03-01T00:00:01.000Z",
                Cost::Monetary {
                    amount_micros: 1,
                    currency: "USD".to_owned(),
                },
            ),
        ];
        ledger
            .reconcile(&entries)
            .unwrap_or_else(|error| panic!("valid rows must reconcile: {error}"));
        assert!(matches!(
            ledger.totals(
                "overflow",
                &utc_day("2026-03-01"),
                &utc_timestamp("2026-03-01T00:00:00.000Z"),
                &utc_timestamp("2026-03-01T00:00:59.999Z"),
            ),
            Err(SessionStoreError::AccountingOverflow)
        ));

        let future_root =
            tempdir().unwrap_or_else(|error| panic!("future fixture tempdir must create: {error}"));
        let future_ledger = AccountingLedger::open(future_root.path())
            .unwrap_or_else(|error| panic!("future ledger must open: {error}"));
        future_ledger
            .record(&accounting_entry(
                "future",
                1,
                0,
                "2026-03-01T00:10:00.000Z",
                Cost::Monetary {
                    amount_micros: 999,
                    currency: "USD".to_owned(),
                },
            ))
            .unwrap_or_else(|error| panic!("future row must record: {error}"));
        let future_totals = future_ledger
            .totals(
                "future",
                &utc_day("2026-03-01"),
                &utc_timestamp("2026-03-01T00:00:00.000Z"),
                &utc_timestamp("2026-03-01T00:00:59.999Z"),
            )
            .unwrap_or_else(|error| panic!("future totals must query: {error}"));
        assert_eq!(future_totals.trailing_session_micros_usd, 0);
        assert_eq!(future_totals.trailing_all_sessions_micros_usd, 0);
        assert_eq!(future_totals.session_micros_usd, 0);
    }

    #[test]
    fn accounting_serializes_concurrent_session_writers() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        drop(
            AccountingLedger::open(root.path())
                .unwrap_or_else(|error| panic!("ledger schema must initialize: {error}")),
        );
        let first_root = root.path().to_owned();
        let second_root = root.path().to_owned();
        let first = std::thread::spawn(move || {
            let ledger = AccountingLedger::open(&first_root)
                .unwrap_or_else(|error| panic!("first ledger must open: {error}"));
            for sequence in 0..32 {
                ledger
                    .record(&accounting_entry(
                        "concurrent-a",
                        sequence + 1,
                        sequence,
                        "2026-04-01T00:00:00.000Z",
                        Cost::Monetary {
                            amount_micros: 1,
                            currency: "USD".to_owned(),
                        },
                    ))
                    .unwrap_or_else(|error| panic!("first writer must record: {error}"));
            }
        });
        let second = std::thread::spawn(move || {
            let ledger = AccountingLedger::open(&second_root)
                .unwrap_or_else(|error| panic!("second ledger must open: {error}"));
            for sequence in 0..32 {
                ledger
                    .record(&accounting_entry(
                        "concurrent-b",
                        sequence + 1,
                        sequence,
                        "2026-04-01T00:00:01.000Z",
                        Cost::AiCredits {
                            credits_micros: 2,
                            nominal_amount_micros: None,
                            currency: None,
                        },
                    ))
                    .unwrap_or_else(|error| panic!("second writer must record: {error}"));
            }
        });
        first
            .join()
            .unwrap_or_else(|payload| std::panic::resume_unwind(payload));
        second
            .join()
            .unwrap_or_else(|payload| std::panic::resume_unwind(payload));

        let totals = AccountingLedger::open(root.path())
            .and_then(|ledger| {
                ledger.totals(
                    "concurrent-a",
                    &utc_day("2026-04-01"),
                    &utc_timestamp("2026-04-01T00:00:00.000Z"),
                    &utc_timestamp("2026-04-01T00:00:59.999Z"),
                )
            })
            .unwrap_or_else(|error| panic!("concurrent totals must query: {error}"));
        assert_eq!(totals.session_micros_usd, 32);
        assert_eq!(totals.day_micros_usd, 32);
        assert_eq!(totals.day_ai_credit_micros, 64);
        assert_eq!(totals.trailing_all_sessions_micros_usd, 32);
        assert_eq!(totals.trailing_all_sessions_ai_credit_micros, 64);
    }

    #[test]
    fn event_envelope_tolerates_additive_fields() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let log = SessionEventLog::open(root.path(), "future-fields")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        std::fs::write(
            log.path(),
            br#"{"schema_version":1,"sequence":"0","event":{"kind":"user","text":"hello","future_event_field":true},"future_envelope_field":{"nested":1}}
"#,
        )
        .unwrap_or_else(|error| panic!("future fixture must write: {error}"));
        let events = log
            .load::<FixtureEvent>()
            .unwrap_or_else(|error| panic!("additive fields must load: {error}"));
        assert_eq!(events[0].event.text, "hello");
    }

    #[test]
    fn persisted_sequence_is_authoritative_decimal_string() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let mut log = SessionEventLog::open(root.path(), "sequence")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        assert_eq!(log.last_sequence(), None);
        let persisted = log
            .append(FixtureEvent {
                kind: "user".to_owned(),
                text: "hello".to_owned(),
            })
            .unwrap_or_else(|error| panic!("event must persist: {error}"));
        assert_eq!(persisted.sequence, SequenceId(0));
        let raw = std::fs::read_to_string(log.path())
            .unwrap_or_else(|error| panic!("log must read: {error}"));
        assert!(raw.contains("\"sequence\":\"0\""));
        assert!(!raw.contains("\"sequence\":0"));
        assert_eq!(log.last_sequence(), Some(SequenceId(0)));
        let before = raw;
        assert!(
            log.append_expected(
                SequenceId(2),
                FixtureEvent {
                    kind: "assistant".to_owned(),
                    text: "must not write".to_owned(),
                }
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(log.path())
                .unwrap_or_else(|error| panic!("unchanged log must read: {error}")),
            before
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_session_log_has_one_process_writer() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let log = SessionEventLog::open(root.path(), "single-writer")
            .unwrap_or_else(|error| panic!("first writer must open: {error}"));
        assert!(SessionEventLog::open(root.path(), "single-writer").is_err());
        drop(log);
        SessionEventLog::open(root.path(), "single-writer")
            .unwrap_or_else(|error| panic!("writer lock must release: {error:?}"));
    }

    #[cfg(unix)]
    #[test]
    fn session_log_rejects_symlink_escape_components() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let outside = tempdir().unwrap_or_else(|error| panic!("outside must create: {error}"));
        std::fs::create_dir_all(root.path().join("sessions/file-link"))
            .unwrap_or_else(|error| panic!("session directory must create: {error}"));
        let outside_log = outside.path().join("outside.jsonl");
        std::fs::write(&outside_log, b"outside")
            .unwrap_or_else(|error| panic!("outside log must write: {error}"));
        symlink(
            &outside_log,
            root.path().join("sessions/file-link/events.jsonl"),
        )
        .unwrap_or_else(|error| panic!("event symlink must create: {error}"));
        assert!(SessionEventLog::open(root.path(), "file-link").is_err());
        assert_eq!(
            std::fs::read(&outside_log)
                .unwrap_or_else(|error| panic!("outside log must read: {error}")),
            b"outside"
        );

        symlink(outside.path(), root.path().join("sessions/directory-link"))
            .unwrap_or_else(|error| panic!("directory symlink must create: {error}"));
        assert!(SessionEventLog::open(root.path(), "directory-link").is_err());
        assert!(!outside.path().join("events.jsonl").exists());
    }

    #[cfg(unix)]
    #[test]
    fn session_log_fifo_child() {
        let Some(root) = std::env::var_os("ROTTWEILER_TEST_FIFO_SESSION_ROOT") else {
            return;
        };
        let result = SessionEventLog::open(std::path::Path::new(&root), "fifo");
        assert!(matches!(
            result,
            Err(super::SessionStoreError::UnsafeEventFileType)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_event_log_is_rejected_in_a_non_hanging_subprocess() {
        use std::{process::Command, thread, time::Duration};

        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let directory = root.path().join("sessions/fifo");
        std::fs::create_dir_all(&directory)
            .unwrap_or_else(|error| panic!("session directory must create: {error}"));
        assert!(
            Command::new("mkfifo")
                .arg(directory.join("events.jsonl"))
                .status()
                .unwrap_or_else(|error| panic!("mkfifo must run: {error}"))
                .success()
        );
        let executable = std::env::current_exe()
            .unwrap_or_else(|error| panic!("test executable must resolve: {error}"));
        let mut child = Command::new(executable)
            .arg("--exact")
            .arg("session::tests::session_log_fifo_child")
            .arg("--nocapture")
            .env("ROTTWEILER_TEST_FIFO_SESSION_ROOT", root.path())
            .spawn()
            .unwrap_or_else(|error| panic!("FIFO test child must spawn: {error}"));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = child
                .try_wait()
                .unwrap_or_else(|error| panic!("FIFO test child must poll: {error}"))
            {
                assert!(status.success(), "FIFO test child failed: {status}");
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("opening a FIFO event log blocked for more than three seconds");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn derived_index_rebuild_replaces_stale_rows() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let stale = SessionProjection {
            summary: SessionSummary {
                id: "stale".to_owned(),
                title: "old".to_owned(),
                updated_unix_ms: 1,
                cost_micros: 0,
            },
            transcript: "obsolete".to_owned(),
            projected_through: Some(SequenceId(0)),
        };
        SessionIndex::open(root.path())
            .and_then(|index| index.upsert(&stale))
            .unwrap_or_else(|error| panic!("stale row must write: {error}"));
        let current = SessionProjection {
            summary: SessionSummary {
                id: "current".to_owned(),
                title: "new".to_owned(),
                updated_unix_ms: 2,
                cost_micros: 1,
            },
            transcript: "authoritative".to_owned(),
            projected_through: Some(SequenceId(u64::MAX)),
        };
        let accounting = accounting_entry(
            "current",
            1,
            u64::MAX,
            "2026-07-10T00:00:00.000Z",
            Cost::Monetary {
                amount_micros: 17,
                currency: "USD".to_owned(),
            },
        );
        let rebuilt = SessionIndex::rebuild(
            root.path(),
            std::slice::from_ref(&current),
            std::slice::from_ref(&accounting),
        )
        .unwrap_or_else(|error| panic!("index must rebuild: {error}"));
        assert!(rebuilt.get("stale").unwrap_or(None).is_none());
        assert_eq!(
            rebuilt.get("current").unwrap_or(None),
            Some(current.summary)
        );
        assert_eq!(
            rebuilt
                .projection_status("current", Some(SequenceId(u64::MAX)))
                .unwrap_or_else(|error| panic!("watermark must survive: {error}")),
            ProjectionStatus::Current
        );
        assert_eq!(
            AccountingLedger::open(root.path())
                .and_then(|ledger| ledger.entries_for_session("current"))
                .unwrap_or_else(|error| panic!("rebuilt accounting must query: {error}")),
            vec![accounting]
        );
    }

    #[test]
    fn read_only_search_never_creates_or_mutates_index_artifacts() {
        let absent = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        assert!(SessionIndex::search_read_only(absent.path(), "needle", 10).is_err());
        assert!(
            std::fs::read_dir(absent.path())
                .unwrap_or_else(|error| panic!("list absent root: {error}"))
                .next()
                .is_none()
        );

        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let projection = SessionProjection {
            summary: SessionSummary {
                id: "searchable".to_owned(),
                title: "Needle session".to_owned(),
                updated_unix_ms: 7,
                cost_micros: 0,
            },
            transcript: "deterministic needle transcript".to_owned(),
            projected_through: Some(SequenceId(0)),
        };
        SessionIndex::open(root.path())
            .and_then(|index| index.upsert(&projection))
            .unwrap_or_else(|error| panic!("seed index: {error}"));
        let index_path = root.path().join("index.sqlite");
        let before =
            std::fs::read(&index_path).unwrap_or_else(|error| panic!("read index: {error}"));
        let wal_path = root.path().join("index.sqlite-wal");
        let shm_path = root.path().join("index.sqlite-shm");
        let wal_before = std::fs::read(&wal_path).ok();
        let shm_before = std::fs::read(&shm_path).ok();
        let found = SessionIndex::search_read_only(root.path(), "needle", 10)
            .unwrap_or_else(|error| panic!("read-only search: {error}"));
        assert_eq!(found, vec![projection.summary]);
        assert!(
            SessionIndex::search_read_only(root.path(), "\" OR ( needle", 10)
                .unwrap_or_else(|error| panic!("punctuation search: {error}"))
                .is_empty()
        );
        assert_eq!(
            std::fs::read(&index_path).unwrap_or_else(|error| panic!("reread index: {error}")),
            before
        );
        assert_eq!(std::fs::read(&wal_path).ok(), wal_before);
        assert_eq!(std::fs::read(&shm_path).ok(), shm_before);
    }

    #[test]
    fn read_only_search_sees_committed_wal_rows_without_mutating_artifacts() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let index =
            SessionIndex::open(root.path()).unwrap_or_else(|error| panic!("seed index: {error}"));
        let writer = index
            .connection()
            .unwrap_or_else(|error| panic!("held writer: {error}"));
        writer
            .execute_batch("PRAGMA wal_autocheckpoint=0;")
            .unwrap_or_else(|error| panic!("disable autocheckpoint: {error}"));
        let projection = SessionProjection {
            summary: SessionSummary {
                id: "wal-fresh".to_owned(),
                title: "Fresh WAL needle".to_owned(),
                updated_unix_ms: 11,
                cost_micros: 0,
            },
            transcript: "committed only in the held writer WAL".to_owned(),
            projected_through: Some(SequenceId(4)),
        };
        upsert_projection(&writer, &projection)
            .unwrap_or_else(|error| panic!("WAL projection: {error}"));

        let index_path = root.path().join("index.sqlite");
        let wal_path = root.path().join("index.sqlite-wal");
        let shm_path = root.path().join("index.sqlite-shm");
        let before = [
            std::fs::read(&index_path).unwrap_or_else(|error| panic!("main db: {error}")),
            std::fs::read(&wal_path).unwrap_or_else(|error| panic!("WAL: {error}")),
            std::fs::read(&shm_path).unwrap_or_else(|error| panic!("SHM: {error}")),
        ];
        assert!(
            !before[1].is_empty(),
            "fixture must retain committed WAL bytes"
        );

        let found = SessionIndex::search_read_only(root.path(), "needle", 10)
            .unwrap_or_else(|error| panic!("fresh read-only search: {error}"));
        assert_eq!(found, vec![projection.summary]);
        assert_eq!(
            [
                std::fs::read(&index_path).unwrap_or_else(|error| panic!("main db after: {error}")),
                std::fs::read(&wal_path).unwrap_or_else(|error| panic!("WAL after: {error}")),
                std::fs::read(&shm_path).unwrap_or_else(|error| panic!("SHM after: {error}")),
            ],
            before
        );
        drop(writer);
    }

    #[test]
    fn read_only_search_rejects_an_oversized_sparse_main_index_before_copying() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        SessionIndex::open(root.path()).unwrap_or_else(|error| panic!("seed index: {error}"));
        let index_path = root.path().join("index.sqlite");
        OpenOptions::new()
            .write(true)
            .open(&index_path)
            .and_then(|file| file.set_len(MAX_SEARCH_INDEX_BYTES + 1))
            .unwrap_or_else(|error| panic!("make sparse oversized index: {error}"));

        assert!(matches!(
            SessionIndex::search_read_only(root.path(), "needle", 10),
            Err(SessionStoreError::SessionIndexSnapshotTooLarge {
                component: "index.sqlite",
                max_bytes: MAX_SEARCH_INDEX_BYTES,
            })
        ));
    }

    #[test]
    fn read_only_search_rejects_an_oversized_sparse_wal_before_copying() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        SessionIndex::open(root.path()).unwrap_or_else(|error| panic!("seed index: {error}"));
        let wal_path = root.path().join("index.sqlite-wal");
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&wal_path)
            .and_then(|file| file.set_len(MAX_SEARCH_INDEX_WAL_BYTES + 1))
            .unwrap_or_else(|error| panic!("make sparse oversized WAL: {error}"));

        assert!(matches!(
            SessionIndex::search_read_only(root.path(), "needle", 10),
            Err(SessionStoreError::SessionIndexSnapshotTooLarge {
                component: "index.sqlite-wal",
                max_bytes: MAX_SEARCH_INDEX_WAL_BYTES,
            })
        ));
    }

    #[test]
    fn bounded_history_read_rejects_bytes_and_event_count_before_returning_data() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let mut log = SessionEventLog::open(root.path(), "bounded")
            .unwrap_or_else(|error| panic!("open log: {error}"));
        log.append(FixtureEvent {
            kind: "fixture".to_owned(),
            text: "bounded payload".to_owned(),
        })
        .unwrap_or_else(|error| panic!("append event: {error}"));
        drop(log);
        assert!(matches!(
            SessionEventLog::load_existing_bounded::<FixtureEvent>(root.path(), "bounded", 1, 10),
            Err(SessionStoreError::EventLogTooLarge { .. })
        ));
        assert!(matches!(
            SessionEventLog::load_existing_bounded::<FixtureEvent>(
                root.path(),
                "bounded",
                1024 * 1024,
                0,
            ),
            Err(SessionStoreError::EventCountTooLarge { max_events: 0 })
        ));
        let expected_bytes = std::fs::metadata(root.path().join("sessions/bounded/events.jsonl"))
            .unwrap_or_else(|error| panic!("event metadata: {error}"))
            .len();
        let (events, descriptor_bytes) = SessionEventLog::load_existing_bounded_with_size::<
            FixtureEvent,
        >(root.path(), "bounded", 1024 * 1024, 10)
        .unwrap_or_else(|error| panic!("bounded descriptor read: {error}"));
        assert_eq!(events.len(), 1);
        assert_eq!(descriptor_bytes, expected_bytes);
    }

    #[test]
    fn paged_history_streams_logs_beyond_twenty_thousand_events() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let mut log = SessionEventLog::open(root.path(), "paged-many")
            .unwrap_or_else(|error| panic!("open log: {error}"));
        log.append_batch((0..20_050).map(|index| FixtureEvent {
            kind: "fixture".to_owned(),
            text: format!("event-{index}"),
        }))
        .unwrap_or_else(|error| panic!("append events: {error}"));
        drop(log);

        let page = SessionEventLog::load_existing_page::<FixtureEvent>(
            root.path(),
            "paged-many",
            Some(SequenceId(19_999)),
            SessionEventPageLimits {
                max_page_events: 25,
                max_page_bytes: 1024 * 1024,
                max_scan_events: 25_000,
                max_scan_bytes: 64 * 1024 * 1024,
                max_line_bytes: 64 * 1024,
            },
        )
        .unwrap_or_else(|error| panic!("paged read: {error}"));
        assert_eq!(page.events.len(), 25);
        assert_eq!(page.events[0].sequence, SequenceId(20_000));
        assert_eq!(page.next_cursor, Some(SequenceId(20_024)));
        assert_eq!(page.total_events, 20_050);
        assert_eq!(page.tail_sequence, Some(SequenceId(20_049)));
        assert_eq!(page.events_before_page, 20_000);
        assert_eq!(page.events_after_page, 25);
        assert!(page.has_more);
    }

    #[test]
    fn paged_history_streams_logs_beyond_eight_megabytes_with_bounded_lines() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let mut log = SessionEventLog::open(root.path(), "paged-large")
            .unwrap_or_else(|error| panic!("open log: {error}"));
        let payload = "x".repeat(1024 * 1024);
        log.append_batch((0..9).map(|_| FixtureEvent {
            kind: "fixture".to_owned(),
            text: payload.clone(),
        }))
        .unwrap_or_else(|error| panic!("append events: {error}"));
        drop(log);

        let limits = SessionEventPageLimits {
            max_page_events: 1,
            max_page_bytes: 2 * 1024 * 1024,
            max_line_bytes: 2 * 1024 * 1024,
            max_scan_bytes: 16 * 1024 * 1024,
            max_scan_events: 100,
        };
        let page = SessionEventLog::load_existing_page::<FixtureEvent>(
            root.path(),
            "paged-large",
            None,
            limits,
        )
        .unwrap_or_else(|error| panic!("paged read: {error}"));
        assert!(page.total_bytes > 8 * 1024 * 1024);
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.total_events, 9);
        assert_eq!(page.events_after_page, 8);
        assert!(page.has_more);

        assert!(matches!(
            SessionEventLog::load_existing_page::<FixtureEvent>(
                root.path(),
                "paged-large",
                None,
                SessionEventPageLimits {
                    max_line_bytes: 1024,
                    ..limits
                },
            ),
            Err(SessionStoreError::EventRecordTooLarge {
                max_line_bytes: 1024
            })
        ));
    }

    #[test]
    fn paged_history_cursor_walk_has_exact_tail_and_truncation_metadata() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let mut log = SessionEventLog::open(root.path(), "paged-cursor")
            .unwrap_or_else(|error| panic!("open log: {error}"));
        log.append_batch((0..7).map(|index| FixtureEvent {
            kind: "fixture".to_owned(),
            text: format!("event-{index}"),
        }))
        .unwrap_or_else(|error| panic!("append events: {error}"));
        drop(log);
        let limits = SessionEventPageLimits {
            max_page_events: 3,
            max_page_bytes: 1024 * 1024,
            max_line_bytes: 64 * 1024,
            max_scan_bytes: 1024 * 1024,
            max_scan_events: 100,
        };
        let mut cursor = None;
        let mut seen = Vec::new();
        loop {
            let page = SessionEventLog::load_existing_page::<FixtureEvent>(
                root.path(),
                "paged-cursor",
                cursor,
                limits,
            )
            .unwrap_or_else(|error| panic!("paged read: {error}"));
            seen.extend(page.events.iter().map(|event| event.sequence));
            cursor = page.next_cursor;
            if !page.has_more {
                assert_eq!(page.total_events, 7);
                assert_eq!(page.tail_sequence, Some(SequenceId(6)));
                assert_eq!(page.events_after_page, 0);
                break;
            }
        }
        assert_eq!(seen, (0..7).map(SequenceId).collect::<Vec<_>>());
        let tail = SessionEventLog::load_existing_page::<FixtureEvent>(
            root.path(),
            "paged-cursor",
            cursor,
            limits,
        )
        .unwrap_or_else(|error| panic!("tail read: {error}"));
        assert!(tail.events.is_empty());
        assert_eq!(tail.next_cursor, Some(SequenceId(6)));
        assert_eq!(tail.events_before_page, 7);
        assert!(!tail.has_more);
    }

    #[test]
    fn paged_history_validates_sequences_before_and_after_the_requested_cursor() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let mut log = SessionEventLog::open(root.path(), "paged-corrupt")
            .unwrap_or_else(|error| panic!("open log: {error}"));
        log.append_batch((0..4).map(|index| FixtureEvent {
            kind: "fixture".to_owned(),
            text: format!("event-{index}"),
        }))
        .unwrap_or_else(|error| panic!("append events: {error}"));
        let path = log.path().to_path_buf();
        drop(log);
        let mut lines = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read fixture: {error}"))
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut corrupt: serde_json::Value = serde_json::from_str(&lines[1])
            .unwrap_or_else(|error| panic!("decode fixture: {error}"));
        corrupt["sequence"] = serde_json::json!("9");
        lines[1] = serde_json::to_string(&corrupt)
            .unwrap_or_else(|error| panic!("encode fixture: {error}"));
        std::fs::write(&path, format!("{}\n", lines.join("\n")))
            .unwrap_or_else(|error| panic!("write corruption: {error}"));

        assert!(matches!(
            SessionEventLog::load_existing_page::<FixtureEvent>(
                root.path(),
                "paged-corrupt",
                Some(SequenceId(2)),
                SessionEventPageLimits::default(),
            ),
            Err(SessionStoreError::CorruptEvent(
                "non-contiguous event sequence"
            ))
        ));
    }

    #[test]
    fn paged_history_rejects_concurrent_descriptor_mutation_before_returning_data() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let mut log = SessionEventLog::open(root.path(), "paged-mutated")
            .unwrap_or_else(|error| panic!("open log: {error}"));
        log.append(FixtureEvent {
            kind: "fixture".to_owned(),
            text: "first".to_owned(),
        })
        .unwrap_or_else(|error| panic!("append event: {error}"));
        let path = log.path().to_path_buf();
        drop(log);
        #[cfg(unix)]
        let file = super::open_existing_session_file(root.path(), "paged-mutated")
            .unwrap_or_else(|error| panic!("open descriptor: {error}"));
        #[cfg(not(unix))]
        let file = super::open_existing_session_file_portable(root.path(), "paged-mutated")
            .unwrap_or_else(|error| panic!("open descriptor: {error}"));
        let result = super::load_event_page_with_hook::<FixtureEvent, _>(
            &file,
            None,
            SessionEventPageLimits::default(),
            || {
                let envelope = EventEnvelope {
                    schema_version: super::EVENT_SCHEMA_VERSION,
                    sequence: SequenceId(1),
                    event: FixtureEvent {
                        kind: "fixture".to_owned(),
                        text: "concurrent".to_owned(),
                    },
                };
                let mut append = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap_or_else(|error| panic!("open append: {error}"));
                serde_json::to_writer(&mut append, &envelope)
                    .unwrap_or_else(|error| panic!("append JSON: {error}"));
                append
                    .write_all(b"\n")
                    .unwrap_or_else(|error| panic!("append newline: {error}"));
                append
                    .sync_all()
                    .unwrap_or_else(|error| panic!("sync append: {error}"));
            },
        );
        assert!(matches!(
            result,
            Err(SessionStoreError::EventFileChangedDuringRead)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_history_read_rejects_multi_link_event_files() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let log = SessionEventLog::open(root.path(), "linked")
            .unwrap_or_else(|error| panic!("open log: {error}"));
        let link = root.path().join("linked-copy.jsonl");
        std::fs::hard_link(log.path(), &link)
            .unwrap_or_else(|error| panic!("hard link fixture: {error}"));
        drop(log);
        assert!(matches!(
            SessionEventLog::load_existing_bounded_with_size::<FixtureEvent>(
                root.path(),
                "linked",
                1024,
                10,
            ),
            Err(SessionStoreError::UnsafeEventFileType)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_search_rejects_symlink_and_hardlink_indexes() {
        use std::os::unix::fs::symlink;

        let target = tempdir().unwrap_or_else(|error| panic!("target tempdir: {error}"));
        SessionIndex::open(target.path())
            .unwrap_or_else(|error| panic!("seed target index: {error}"));
        let target_index = target.path().join("index.sqlite");

        let linked = tempdir().unwrap_or_else(|error| panic!("linked tempdir: {error}"));
        symlink(&target_index, linked.path().join("index.sqlite"))
            .unwrap_or_else(|error| panic!("index symlink: {error}"));
        assert!(SessionIndex::search_read_only(linked.path(), "needle", 10).is_err());

        let hard = tempdir().unwrap_or_else(|error| panic!("hardlink tempdir: {error}"));
        std::fs::hard_link(&target_index, hard.path().join("index.sqlite"))
            .unwrap_or_else(|error| panic!("index hardlink: {error}"));
        assert!(SessionIndex::search_read_only(hard.path(), "needle", 10).is_err());
    }

    use std::{fs::OpenOptions, io::Write};
}
