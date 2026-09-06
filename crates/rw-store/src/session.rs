//! Session storage contracts and the owners of journal, accounting, and search state.
mod accounting;
#[cfg(unix)]
mod derived_database;
mod error;
mod event_log;
#[cfg(unix)]
mod file_lock;
#[cfg(unix)]
pub use file_lock::AdvisoryFileLock;
mod index;
mod index_read;
/// Segmented journal storage and bounded read views.
pub mod journal;
mod journal_io;
/// Bounded canonical recovery checkpoints and source-reference index storage.
#[cfg(unix)]
pub mod recovery_index;
/// Provider-call budget admission contracts and durable reservations.
pub mod reservations;
mod sqlite_schema;
mod sqlite_snapshot;
/// Rebuildable, bounded semantic transcript index persistence.
#[cfg(unix)]
pub mod transcript_index;

pub use accounting::{
    AccountingLedger, AccountingTotals, TurnAccountingEntry, UtcDayKey, UtcTimestamp,
};
pub use error::SessionStoreError;
pub use event_log::{SessionEventLog, garbage_collect_empty_sessions};
#[cfg(test)]
use index::upsert_projection;
pub use index::{
    ProjectionStatus, SearchDocumentWriter, SessionIndex, SessionProjection, SessionSummary,
};
#[cfg(not(unix))]
use journal_io::create_checked_directory_portable;
#[cfg(unix)]
use journal_io::open_or_create_directory;
#[cfg(test)]
use journal_io::{EVENT_READ_HOOK, install_append_fault, run_event_read_hook};
use journal_io::{
    EventFileSnapshot, event_file_snapshot, read_opened_file_bounded, sync_event_file,
    truncate_and_sync_event_file, validate_events_from_sequence, validate_session_id,
    verify_event_file_snapshot, write_event_bytes,
};
use rw_types::SequenceId;
use serde::{Deserialize, Serialize};

/// Public JSONL envelope version for durable session events.
pub const SESSION_EVENT_SCHEMA_VERSION: u16 = 1;
const EVENT_SCHEMA_VERSION: u16 = SESSION_EVENT_SCHEMA_VERSION;

/// One versioned event in a session's public JSONL transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

#[cfg(test)]
mod sqlite_schema_tests;
#[cfg(test)]
mod tests;
