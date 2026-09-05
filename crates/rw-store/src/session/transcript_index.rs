//! Descriptor-native storage for core-owned transcript projections (ADR-030).

use super::{
    derived_database::{DerivedDatabase, DerivedDatabaseError, IoCounters},
    exclusive_lock::ExclusiveFileLock,
    journal::{JournalAdvance, JournalPrefixIdentity, JournalReadView},
};
use redb::{
    Database, ReadableDatabase as _, ReadableTable as _, ReadableTableMetadata as _,
    TableDefinition,
};
use rw_types::SequenceId;
use std::{
    fs::File,
    sync::{Arc, atomic::Ordering},
};
use thiserror::Error;

/// Maximum retained opaque payload in one transcript row.
pub const MAX_ROW_BYTES: usize = 32 * 1024 - 512;
/// Maximum opaque recovery state retained between index batches.
pub const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;
/// Maximum row mutations in one atomic projection batch.
pub const MAX_BATCH_ROWS: usize = 128;
/// Maximum charged row bytes in one transaction, including moved payloads.
pub const MAX_BATCH_BYTES: usize = 1024 * 1024;
/// Maximum retained result payload for one page.
pub const MAX_PAGE_BYTES: usize = 1024 * 1024;
/// Maximum rows returned by one page.
pub const MAX_PAGE_ROWS: usize = 64;
const MAX_KEY_BYTES: usize = 256;
const CACHE_BYTES: usize = 4 * 1024 * 1024;
const MAX_DATABASE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
type StoredRow<'a> = (&'a str, u64, u64, Option<u64>, &'a [u8]);
type StoredHead<'a> = (u32, u64, u64, &'a [u8], &'a [u8], bool);
const ROWS: TableDefinition<u64, StoredRow<'_>> = TableDefinition::new("transcript_rows_v1");
const KEYS: TableDefinition<&str, u64> = TableDefinition::new("transcript_keys_v1");
const TURNS: TableDefinition<(u64, u64), ()> = TableDefinition::new("transcript_turns_v1");
const BINDINGS: TableDefinition<&str, &str> = TableDefinition::new("transcript_bindings_v1");
const CHANGES: TableDefinition<(u64, u64), ()> = TableDefinition::new("transcript_changes_v1");
const SOURCES: TableDefinition<u64, u64> = TableDefinition::new("transcript_sources_v1");
const HEAD: TableDefinition<u8, StoredHead<'_>> = TableDefinition::new("transcript_head_v1");

/// Index failures never change the authoritative journal.
#[derive(Debug, Error)]
pub enum TranscriptIndexError {
    /// Unsafe descriptor, corrupt metadata or a violated ordinal invariant.
    #[error("invalid transcript index: {0}")]
    Invalid(&'static str),
    /// The derived representation belongs to a different semantic projection version.
    #[error("transcript projection version {actual} does not match {expected}")]
    IncompatibleVersion { expected: u32, actual: u32 },
    /// The caller exceeded an allocation or transaction bound.
    #[error("transcript index limit exceeded: {0}")]
    Limit(&'static str),
    /// Another index owner is currently working on this session.
    #[error("transcript index is busy")]
    Busy,
    /// A mutation raced a newer published prefix.
    #[error("transcript projection prefix changed")]
    Stale,
    /// A bounded rebuild is in progress and no complete view is published.
    #[error("transcript projection is rebuilding")]
    Rebuilding,
    /// Filesystem failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Embedded index storage failure.
    #[error(transparent)]
    Storage(#[from] redb::Error),
    /// Raw prefix validation failed.
    #[error(transparent)]
    Journal(#[from] super::SessionStoreError),
}

impl From<DerivedDatabaseError> for TranscriptIndexError {
    fn from(error: DerivedDatabaseError) -> Self {
        match error {
            DerivedDatabaseError::Invalid(message) => Self::Invalid(message),
            DerivedDatabaseError::Busy => Self::Busy,
            DerivedDatabaseError::Io(error) => Self::Io(error),
            DerivedDatabaseError::Storage(error) => Self::Storage(error),
            DerivedDatabaseError::Journal(error) => Self::Journal(error),
        }
    }
}

/// Core-owned checkpoint and the exact prefix it has completely interpreted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptIndexHead {
    /// Semantic projection version; incompatible versions rebuild.
    pub version: u32,
    /// Structural ordering generation, independent of row revisions.
    pub generation: u64,
    /// Exact authoritative prefix represented by the projection.
    pub prefix: JournalPrefixIdentity,
    /// Opaque, bounded semantic reducer checkpoint.
    pub state: Vec<u8>,
    /// Whether ordinal repair/rebuild is in progress.
    pub rebuilding: bool,
    /// Number of retained logical rows, read in constant time.
    pub total_rows: u64,
}

/// An opaque semantic row with indexed identity and ordering metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptIndexRow {
    /// Dense zero-based order in a complete generation.
    pub ordinal: u64,
    /// Stable, core-assigned source identity.
    pub key: String,
    /// Durable event that originally created the item.
    pub source: SequenceId,
    /// Latest durable event that changed this item's content/associations.
    pub revision: SequenceId,
    /// Owning turn, when rewind semantics apply to this item.
    pub agent_turn: Option<u64>,
    /// Core-owned bounded semantic preview and source references.
    pub payload: Vec<u8>,
}

/// A bounded atomic change to the derived row catalog.
#[derive(Clone, Debug)]
pub enum TranscriptIndexMutation {
    /// Insert the next ordinal or replace the content of an existing identity.
    Put(TranscriptIndexRow),
    /// Delete an identity during a rebuild.
    Delete(String),
    /// Bind a core-owned entity identity to its current stable row.
    Bind { binding: String, key: String },
    /// Repair one ordinal during a rebuild without decoding its payload.
    Move { key: String, ordinal: u64 },
}

impl TranscriptIndexMutation {
    /// Conservative retained-byte charge used by the atomic batch admission limit.
    #[must_use]
    pub fn charged_bytes(&self) -> usize {
        match self {
            Self::Put(row) => row.payload.len() + row.key.len() + 48,
            Self::Delete(key) => key.len() + 48,
            Self::Bind { binding, key } => binding.len() + key.len() + 48,
            Self::Move { .. } => MAX_ROW_BYTES + MAX_KEY_BYTES + 48,
        }
    }
}

/// A page and the precise complete view from which it was read.
#[derive(Debug)]
pub struct TranscriptIndexPage {
    /// Current semantic projection watermark.
    pub head: TranscriptIndexHead,
    /// Ascending ordinal rows.
    pub rows: Vec<TranscriptIndexRow>,
    /// Charged retained row bytes, including keys and fixed metadata.
    pub retained_bytes: usize,
}

/// Actual backend I/O; cached reads do not increment these counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TranscriptIndexIo {
    /// Bytes read from the index descriptor.
    pub bytes_read: u64,
    /// Bytes written to the index descriptor.
    pub bytes_written: u64,
    /// Durable flushes requested by index transactions.
    pub syncs: u64,
}

/// One independently locked index owner. Database transactions never escape it.
pub struct TranscriptIndex {
    database: Database,
    counters: Arc<IoCounters>,
    directory: File,
    _lock: ExclusiveFileLock,
}

#[derive(Clone, Copy)]
enum ReadWindow {
    From(u64),
    Before(u64),
}

impl TranscriptIndex {
    /// Open a descriptor-native derived index with a fixed cache. Expensive
    /// automatic repair is disabled; callers explicitly rebuild invalid indexes.
    ///
    /// # Errors
    /// Fails for unsafe files, concurrent ownership, incompatible/corrupt indexes
    /// and authoritative prefix or filesystem failures.
    pub fn open(view: &JournalReadView, version: u32) -> Result<Self, TranscriptIndexError> {
        Self::open_inner(view, version, false)
    }

    /// Discard only the derived index and begin an explicit journal rebuild.
    ///
    /// # Errors
    /// Fails for unsafe descriptors, concurrent ownership or storage errors.
    pub fn rebuild(view: &JournalReadView, version: u32) -> Result<Self, TranscriptIndexError> {
        Self::open_inner(view, version, true)
    }

    fn open_inner(
        view: &JournalReadView,
        version: u32,
        reset: bool,
    ) -> Result<Self, TranscriptIndexError> {
        let owner =
            DerivedDatabase::open(view, "transcript", CACHE_BYTES, MAX_DATABASE_BYTES, reset)?;
        let empty = owner.was_empty;
        let index = Self {
            database: owner.database,
            counters: owner.counters,
            directory: owner.directory,
            _lock: owner.lock,
        };
        let read = index.database.begin_read().map_err(storage)?;
        let missing = matches!(
            read.open_table(HEAD),
            Err(redb::TableError::TableDoesNotExist(_))
        );
        drop(read);
        if missing && !empty {
            return Err(TranscriptIndexError::Invalid("missing projection schema"));
        }
        if missing {
            let transaction = index.database.begin_write().map_err(storage)?;
            transaction.open_table(ROWS).map_err(storage)?;
            transaction.open_table(KEYS).map_err(storage)?;
            transaction.open_table(TURNS).map_err(storage)?;
            transaction.open_table(BINDINGS).map_err(storage)?;
            transaction.open_table(CHANGES).map_err(storage)?;
            transaction.open_table(SOURCES).map_err(storage)?;
            let prefix = JournalPrefixIdentity::empty();
            transaction
                .open_table(HEAD)
                .map_err(storage)?
                .insert(0, (version, 0, 0, prefix.digest.as_slice(), &[][..], false))
                .map_err(storage)?;
            transaction.commit().map_err(storage)?;
        }
        let head = index.head()?;
        if head.version != version {
            return Err(TranscriptIndexError::IncompatibleVersion {
                expected: version,
                actual: head.version,
            });
        }
        view.at_prefix(head.prefix)?;
        Ok(index)
    }

    /// Read a constant-sized semantic checkpoint.
    ///
    /// # Errors
    /// Fails for corrupt metadata or storage errors.
    pub fn head(&self) -> Result<TranscriptIndexHead, TranscriptIndexError> {
        read_head(&self.database.begin_read().map_err(storage)?)
    }

    /// Read actual cumulative descriptor work for diagnostics and qualification.
    #[must_use]
    pub fn io_metrics(&self) -> TranscriptIndexIo {
        TranscriptIndexIo {
            bytes_read: self.counters.read.load(Ordering::Relaxed),
            bytes_written: self.counters.written.load(Ordering::Relaxed),
            syncs: self.counters.syncs.load(Ordering::Relaxed),
        }
    }

    /// Apply a bounded atomic projection batch and its complete journal prefix.
    /// Rebuild batches are hidden from page reads until dense ordering is restored.
    ///
    /// # Errors
    /// Fails for stale checkpoints, invalid ordering, limits, or storage errors.
    pub fn apply(
        &mut self,
        advance: &JournalAdvance,
        generation: u64,
        state: &[u8],
        rebuilding: bool,
        mutations: &[TranscriptIndexMutation],
    ) -> Result<(), TranscriptIndexError> {
        use std::os::unix::fs::MetadataExt as _;
        let expected = advance.previous();
        let next = advance.next();
        let incoming = next.derived_directory()?.metadata()?;
        let owned = self.directory.metadata()?;
        if incoming.dev() != owned.dev() || incoming.ino() != owned.ino() {
            return Err(TranscriptIndexError::Invalid("foreign journal"));
        }
        let head = self.head()?;
        if head.prefix != expected
            || next.prefix_identity().next_sequence < expected.next_sequence
            || generation < head.generation
        {
            return Err(TranscriptIndexError::Stale);
        }
        if state.len() > MAX_CHECKPOINT_BYTES || mutations.len() > MAX_BATCH_ROWS {
            return Err(TranscriptIndexError::Limit("batch/checkpoint"));
        }
        let mut charged_bytes = 0;
        for mutation in mutations {
            validate_mutation(mutation, next.prefix_identity().next_sequence)?;
            charged_bytes += mutation.charged_bytes();
            if charged_bytes > MAX_BATCH_BYTES {
                return Err(TranscriptIndexError::Limit("batch bytes"));
            }
        }
        let transaction = self.database.begin_write().map_err(storage)?;
        {
            let mut catalog = RowCatalog::open(&transaction)?;
            for mutation in mutations {
                match mutation {
                    TranscriptIndexMutation::Put(row) => {
                        catalog.put(row, rebuilding || head.rebuilding)?;
                    }
                    TranscriptIndexMutation::Delete(key) => {
                        catalog.delete(key, rebuilding || head.rebuilding)?;
                    }
                    TranscriptIndexMutation::Bind { binding, key } => {
                        if catalog.keys.get(key.as_str()).map_err(storage)?.is_none() {
                            return Err(TranscriptIndexError::Invalid("missing binding row"));
                        }
                        transaction
                            .open_table(BINDINGS)
                            .map_err(storage)?
                            .insert(binding.as_str(), key.as_str())
                            .map_err(storage)?;
                    }
                    TranscriptIndexMutation::Move { key, ordinal } => {
                        catalog.move_row(key, *ordinal, rebuilding || head.rebuilding)?;
                    }
                }
            }
            let rows = &catalog.rows;
            let total = rows.len().map_err(storage)?;
            if !rebuilding && total > 0 {
                let first = rows
                    .first()
                    .map_err(storage)?
                    .ok_or(TranscriptIndexError::Invalid("missing first row"))?
                    .0
                    .value();
                let last = rows
                    .last()
                    .map_err(storage)?
                    .ok_or(TranscriptIndexError::Invalid("missing last row"))?
                    .0
                    .value();
                if first != 0 || last != total - 1 {
                    return Err(TranscriptIndexError::Invalid(
                        "non-dense published ordinals",
                    ));
                }
            }
            let prefix = next.prefix_identity();
            transaction
                .open_table(HEAD)
                .map_err(storage)?
                .insert(
                    0,
                    (
                        head.version,
                        generation,
                        prefix.next_sequence,
                        prefix.digest.as_slice(),
                        state,
                        rebuilding,
                    ),
                )
                .map_err(storage)?;
        }
        transaction.commit().map_err(storage)?;
        Ok(())
    }

    /// Fetch a bounded ordinal window with no offset scan or historical count.
    ///
    /// # Errors
    /// Fails for invalid limits, incomplete generations or corrupt rows.
    pub fn page(
        &self,
        first: u64,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<TranscriptIndexPage, TranscriptIndexError> {
        self.read_page(ReadWindow::From(first), max_rows, max_bytes, false)
    }

    /// Read the rows immediately before an exclusive ordinal, including a byte-bounded tail.
    ///
    /// # Errors
    /// Fails for invalid limits, incomplete generations or corrupt rows.
    pub fn page_ending_before(
        &self,
        end: u64,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<TranscriptIndexPage, TranscriptIndexError> {
        self.read_page(ReadWindow::Before(end), max_rows, max_bytes, false)
    }

    /// Read a bounded working window while core repairs a hidden generation.
    ///
    /// # Errors
    /// Fails for invalid limits or corrupt rows. These rows must never be served to clients.
    pub fn maintenance_page(
        &self,
        first: u64,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<TranscriptIndexPage, TranscriptIndexError> {
        self.read_page(ReadWindow::From(first), max_rows, max_bytes, true)
    }

    fn read_page(
        &self,
        window: ReadWindow,
        max_rows: usize,
        max_bytes: usize,
        maintenance: bool,
    ) -> Result<TranscriptIndexPage, TranscriptIndexError> {
        if max_rows == 0 || max_rows > MAX_PAGE_ROWS || max_bytes == 0 || max_bytes > MAX_PAGE_BYTES
        {
            return Err(TranscriptIndexError::Limit("page"));
        }
        let read = self.database.begin_read().map_err(storage)?;
        let head = read_head(&read)?;
        if head.rebuilding && !maintenance {
            return Err(TranscriptIndexError::Rebuilding);
        }
        let table = read.open_table(ROWS).map_err(storage)?;
        let mut rows = Vec::with_capacity(max_rows);
        let mut retained_bytes = 0;
        let bounds = match window {
            ReadWindow::From(first) => {
                (std::ops::Bound::Included(first), std::ops::Bound::Unbounded)
            }
            ReadWindow::Before(end) => (std::ops::Bound::Unbounded, std::ops::Bound::Excluded(end)),
        };
        let mut range = table.range::<u64>(bounds).map_err(storage)?;
        for _ in 0..max_rows {
            let entry = match window {
                ReadWindow::From(_) => range.next(),
                ReadWindow::Before(_) => range.next_back(),
            };
            let Some(entry) = entry else { break };
            let (ordinal, value) = entry.map_err(storage)?;
            let row = owned_row(ordinal.value(), value.value())?;
            let charge = row.payload.len() + row.key.len() + 48;
            if charge > max_bytes - retained_bytes {
                if rows.is_empty() {
                    return Err(TranscriptIndexError::Limit("page cannot fit one row"));
                }
                break;
            }
            retained_bytes += charge;
            rows.push(row);
        }
        if matches!(window, ReadWindow::Before(_)) {
            rows.reverse();
        }
        Ok(TranscriptIndexPage {
            head,
            rows,
            retained_bytes,
        })
    }

    /// Resolve an item identity without traversing earlier rows.
    ///
    /// # Errors
    /// Fails for oversized keys, corrupt rows or an incomplete generation.
    pub fn row(&self, key: &str) -> Result<Option<TranscriptIndexRow>, TranscriptIndexError> {
        if key.len() > MAX_KEY_BYTES {
            return Err(TranscriptIndexError::Limit("key"));
        }
        let read = self.database.begin_read().map_err(storage)?;
        if read_head(&read)?.rebuilding {
            return Err(TranscriptIndexError::Rebuilding);
        }
        let keys = read.open_table(KEYS).map_err(storage)?;
        let Some(ordinal) = keys.get(key).map_err(storage)? else {
            return Ok(None);
        };
        let table = read.open_table(ROWS).map_err(storage)?;
        let value = table
            .get(ordinal.value())
            .map_err(storage)?
            .ok_or(TranscriptIndexError::Invalid("missing keyed row"))?;
        Ok(Some(owned_row(ordinal.value(), value.value())?))
    }
    /// Resolve the latest row bound to a mutable semantic entity.
    /// A binding whose row was removed by rewind resolves to no row.
    ///
    /// # Errors
    /// Fails for invalid keys, corrupt storage or an incomplete generation.
    pub fn bound_row(
        &self,
        binding: &str,
    ) -> Result<Option<TranscriptIndexRow>, TranscriptIndexError> {
        validate_key(binding)?;
        let read = self.database.begin_read().map_err(storage)?;
        if read_head(&read)?.rebuilding {
            return Err(TranscriptIndexError::Rebuilding);
        }
        let bindings = read.open_table(BINDINGS).map_err(storage)?;
        let Some(key) = bindings.get(binding).map_err(storage)? else {
            return Ok(None);
        };
        read_keyed_row(&read, key.value())
    }

    /// Select at most one bounded batch of rows removed by an agent-turn rewind.
    ///
    /// # Errors
    /// Fails for invalid limits or corrupt storage.
    pub fn rows_after_turn(
        &self,
        turn: u64,
        max_rows: usize,
    ) -> Result<Vec<TranscriptIndexRow>, TranscriptIndexError> {
        if max_rows == 0 || max_rows > MAX_PAGE_ROWS {
            return Err(TranscriptIndexError::Limit("rewind rows"));
        }
        let Some(first) = turn.checked_add(1) else {
            return Ok(Vec::new());
        };
        let read = self.database.begin_read().map_err(storage)?;
        let turns = read.open_table(TURNS).map_err(storage)?;
        let rows = read.open_table(ROWS).map_err(storage)?;
        let mut found = Vec::with_capacity(max_rows);
        let mut bytes = 0;
        for entry in turns.range((first, 0)..).map_err(storage)?.take(max_rows) {
            let (key, _) = entry.map_err(storage)?;
            let ordinal = key.value().1;
            let value = rows
                .get(ordinal)
                .map_err(storage)?
                .ok_or(TranscriptIndexError::Invalid("missing turn row"))?;
            let row = owned_row(ordinal, value.value())?;
            bytes += row.payload.len() + row.key.len() + 48;
            if bytes > MAX_PAGE_BYTES {
                break;
            }
            found.push(row);
        }
        Ok(found)
    }

    /// Seek the surviving row at or immediately before a removed source anchor.
    ///
    /// # Errors
    /// Fails for an incomplete generation or corrupt storage.
    pub fn at_or_before_source(
        &self,
        source: SequenceId,
    ) -> Result<Option<TranscriptIndexRow>, TranscriptIndexError> {
        let read = self.database.begin_read().map_err(storage)?;
        if read_head(&read)?.rebuilding {
            return Err(TranscriptIndexError::Rebuilding);
        }
        let sources = read.open_table(SOURCES).map_err(storage)?;
        let mut range = sources.range(..=source.0).map_err(storage)?;
        let Some(entry) = range.next_back() else {
            return Ok(None);
        };
        let (_, ordinal) = entry.map_err(storage)?;
        let rows = read.open_table(ROWS).map_err(storage)?;
        let value = rows
            .get(ordinal.value())
            .map_err(storage)?
            .ok_or(TranscriptIndexError::Invalid("missing source row"))?;
        Ok(Some(owned_row(ordinal.value(), value.value())?))
    }

    /// Read bounded invalidations for previously published rows, excluding appends.
    /// `None` requires a whole-cache invalidation because the result exceeded the cap.
    ///
    /// # Errors
    /// Fails for invalid limits, an incomplete generation or corrupt storage.
    pub fn changed_keys(
        &self,
        after: SequenceId,
        max_rows: usize,
    ) -> Result<Option<Vec<String>>, TranscriptIndexError> {
        if max_rows == 0 || max_rows > MAX_PAGE_ROWS {
            return Err(TranscriptIndexError::Limit("invalidations"));
        }
        let Some(first) = after.0.checked_add(1) else {
            return Ok(Some(Vec::new()));
        };
        let read = self.database.begin_read().map_err(storage)?;
        if read_head(&read)?.rebuilding {
            return Err(TranscriptIndexError::Rebuilding);
        }
        let changes = read.open_table(CHANGES).map_err(storage)?;
        let rows = read.open_table(ROWS).map_err(storage)?;
        let mut found = Vec::with_capacity(max_rows);
        for entry in changes
            .range((first, 0)..)
            .map_err(storage)?
            .take(max_rows + 1)
        {
            if found.len() == max_rows {
                return Ok(None);
            }
            let (key, _) = entry.map_err(storage)?;
            let value = rows
                .get(key.value().1)
                .map_err(storage)?
                .ok_or(TranscriptIndexError::Invalid("missing changed row"))?;
            found.push(value.value().0.to_owned());
        }
        Ok(Some(found))
    }
}

fn read_keyed_row(
    read: &redb::ReadTransaction,
    key: &str,
) -> Result<Option<TranscriptIndexRow>, TranscriptIndexError> {
    let keys = read.open_table(KEYS).map_err(storage)?;
    let Some(ordinal) = keys.get(key).map_err(storage)? else {
        return Ok(None);
    };
    let rows = read.open_table(ROWS).map_err(storage)?;
    let value = rows
        .get(ordinal.value())
        .map_err(storage)?
        .ok_or(TranscriptIndexError::Invalid("missing keyed row"))?;
    Ok(Some(owned_row(ordinal.value(), value.value())?))
}

struct RowCatalog<'a> {
    rows: redb::Table<'a, u64, StoredRow<'static>>,
    keys: redb::Table<'a, &'static str, u64>,
    turns: redb::Table<'a, (u64, u64), ()>,
    changes: redb::Table<'a, (u64, u64), ()>,
    sources: redb::Table<'a, u64, u64>,
}
impl<'a> RowCatalog<'a> {
    fn open(transaction: &'a redb::WriteTransaction) -> Result<Self, TranscriptIndexError> {
        Ok(Self {
            rows: transaction.open_table(ROWS).map_err(storage)?,
            keys: transaction.open_table(KEYS).map_err(storage)?,
            turns: transaction.open_table(TURNS).map_err(storage)?,
            changes: transaction.open_table(CHANGES).map_err(storage)?,
            sources: transaction.open_table(SOURCES).map_err(storage)?,
        })
    }
    fn put(
        &mut self,
        row: &TranscriptIndexRow,
        rebuilding: bool,
    ) -> Result<(), TranscriptIndexError> {
        let prior = self
            .keys
            .get(row.key.as_str())
            .map_err(storage)?
            .map(|value| value.value());
        if let Some(ordinal) = prior {
            let old = self
                .rows
                .get(ordinal)
                .map_err(storage)?
                .ok_or(TranscriptIndexError::Invalid("missing keyed row"))?;
            let (_, source, revision, turn, payload) = old.value();
            if row.ordinal != ordinal
                || source != row.source.0
                || revision > row.revision.0
                || turn != row.agent_turn
                || (revision == row.revision.0 && payload != row.payload.as_slice())
            {
                return Err(TranscriptIndexError::Invalid(
                    "row identity/revision changed",
                ));
            }
            if revision > source {
                self.changes.remove((revision, ordinal)).map_err(storage)?;
            }
        } else {
            if self.sources.get(row.source.0).map_err(storage)?.is_some() {
                return Err(TranscriptIndexError::Invalid("duplicate row source"));
            }
            self.sources
                .insert(row.source.0, row.ordinal)
                .map_err(storage)?;
            if !rebuilding && row.ordinal != self.rows.len().map_err(storage)? {
                return Err(TranscriptIndexError::Invalid("non-dense append"));
            }
            if self.rows.get(row.ordinal).map_err(storage)?.is_some() {
                return Err(TranscriptIndexError::Invalid("occupied ordinal"));
            }
            self.keys
                .insert(row.key.as_str(), row.ordinal)
                .map_err(storage)?;
            if let Some(turn) = row.agent_turn {
                self.turns
                    .insert((turn, row.ordinal), ())
                    .map_err(storage)?;
            }
        }
        if row.revision > row.source {
            self.changes
                .insert((row.revision.0, row.ordinal), ())
                .map_err(storage)?;
        }
        self.rows
            .insert(
                row.ordinal,
                (
                    row.key.as_str(),
                    row.source.0,
                    row.revision.0,
                    row.agent_turn,
                    row.payload.as_slice(),
                ),
            )
            .map_err(storage)?;

        Ok(())
    }
    fn delete(&mut self, key: &str, rebuilding: bool) -> Result<(), TranscriptIndexError> {
        if !rebuilding {
            return Err(TranscriptIndexError::Invalid("delete outside rebuild"));
        }
        let ordinal = self
            .keys
            .remove(key)
            .map_err(storage)?
            .map(|value| value.value());
        if let Some(ordinal) = ordinal {
            let old = self
                .rows
                .remove(ordinal)
                .map_err(storage)?
                .ok_or(TranscriptIndexError::Invalid("missing deleted row"))?;
            let (_, source, revision, _, _) = old.value();
            self.sources.remove(source).map_err(storage)?;
            if revision > source {
                self.changes.remove((revision, ordinal)).map_err(storage)?;
            }
            if let Some(turn) = old.value().3 {
                self.turns.remove((turn, ordinal)).map_err(storage)?;
            }
        }

        Ok(())
    }
    fn move_row(
        &mut self,
        key: &str,
        ordinal: u64,
        rebuilding: bool,
    ) -> Result<(), TranscriptIndexError> {
        if !rebuilding {
            return Err(TranscriptIndexError::Invalid("move outside rebuild"));
        }
        if self.rows.get(ordinal).map_err(storage)?.is_some() {
            return Err(TranscriptIndexError::Invalid("occupied moved ordinal"));
        }
        let previous = self
            .keys
            .get(key)
            .map_err(storage)?
            .ok_or(TranscriptIndexError::Invalid("missing moved key"))?
            .value();
        let old = self
            .rows
            .remove(previous)
            .map_err(storage)?
            .ok_or(TranscriptIndexError::Invalid("missing moved row"))?;
        let moved = owned_row(previous, old.value())?;
        drop(old);
        self.sources
            .insert(moved.source.0, ordinal)
            .map_err(storage)?;
        if moved.revision > moved.source {
            self.changes
                .remove((moved.revision.0, previous))
                .map_err(storage)?;
            self.changes
                .insert((moved.revision.0, ordinal), ())
                .map_err(storage)?;
        }
        if let Some(turn) = moved.agent_turn {
            self.turns.remove((turn, previous)).map_err(storage)?;
            self.turns.insert((turn, ordinal), ()).map_err(storage)?;
        }
        self.rows
            .insert(
                ordinal,
                (
                    moved.key.as_str(),
                    moved.source.0,
                    moved.revision.0,
                    moved.agent_turn,
                    moved.payload.as_slice(),
                ),
            )
            .map_err(storage)?;
        self.keys.insert(key, ordinal).map_err(storage)?;

        Ok(())
    }
}

fn read_head(read: &redb::ReadTransaction) -> Result<TranscriptIndexHead, TranscriptIndexError> {
    let table = read.open_table(HEAD).map_err(storage)?;
    let value = table
        .get(0)
        .map_err(storage)?
        .ok_or(TranscriptIndexError::Invalid("missing head"))?;
    let (version, generation, next_sequence, digest, state, rebuilding) = value.value();
    if state.len() > MAX_CHECKPOINT_BYTES {
        return Err(TranscriptIndexError::Limit("checkpoint"));
    }
    let digest = digest
        .try_into()
        .map_err(|_| TranscriptIndexError::Invalid("prefix digest"))?;
    let total_rows = read
        .open_table(ROWS)
        .map_err(storage)?
        .len()
        .map_err(storage)?;
    Ok(TranscriptIndexHead {
        version,
        generation,
        prefix: JournalPrefixIdentity {
            next_sequence,
            digest,
        },
        state: state.to_vec(),
        rebuilding,
        total_rows,
    })
}

fn owned_row(
    ordinal: u64,
    value: StoredRow<'_>,
) -> Result<TranscriptIndexRow, TranscriptIndexError> {
    let (key, source, revision, agent_turn, payload) = value;
    if key.len() > MAX_KEY_BYTES || payload.len() > MAX_ROW_BYTES {
        return Err(TranscriptIndexError::Limit("stored row"));
    }
    Ok(TranscriptIndexRow {
        ordinal,
        key: key.to_owned(),
        source: SequenceId(source),
        revision: SequenceId(revision),
        agent_turn,
        payload: payload.to_vec(),
    })
}

fn storage(error: impl Into<redb::Error>) -> TranscriptIndexError {
    TranscriptIndexError::Storage(error.into())
}

fn validate_mutation(
    mutation: &TranscriptIndexMutation,
    next: u64,
) -> Result<(), TranscriptIndexError> {
    let key = match mutation {
        TranscriptIndexMutation::Put(row) => {
            if row.payload.len() > MAX_ROW_BYTES || row.ordinal > i64::MAX as u64 {
                return Err(TranscriptIndexError::Limit("row"));
            }
            if row.source.0 > row.revision.0 || row.revision.0 >= next {
                return Err(TranscriptIndexError::Invalid(
                    "row source/revision outside prefix",
                ));
            }
            &row.key
        }
        TranscriptIndexMutation::Delete(key) => key,
        TranscriptIndexMutation::Bind { binding, key } => {
            validate_key(binding)?;
            key
        }
        TranscriptIndexMutation::Move { key, ordinal } => {
            if *ordinal > i64::MAX as u64 {
                return Err(TranscriptIndexError::Limit("ordinal"));
            }
            key
        }
    };
    validate_key(key)
}

fn validate_key(key: &str) -> Result<(), TranscriptIndexError> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES || key.chars().any(char::is_control) {
        return Err(TranscriptIndexError::Limit("key"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
