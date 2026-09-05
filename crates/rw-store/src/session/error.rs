//! Payload-free session storage boundary errors.
use rw_types::SequenceId;
use thiserror::Error;

/// Session log/index failure without transcript contents in diagnostics.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    /// An existing database table does not match the current admitted schema.
    #[error("unsupported SQLite schema for {table}; explicit current-schema recovery is required")]
    UnsupportedSqliteSchema {
        /// Table whose authoritative or derived layout requires explicit recovery.
        table: &'static str,
    },
    /// A session uses the unsupported lifetime-file journal layout.
    #[error("unsupported legacy session journal layout; events.jsonl is not a segmented journal")]
    UnsupportedJournalLayout,
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
