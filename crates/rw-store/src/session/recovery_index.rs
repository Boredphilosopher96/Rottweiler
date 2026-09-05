//! Descriptor-bound storage for core-owned canonical recovery state and source indexes.

use super::{
    SessionStoreError,
    derived_database::{DerivedDatabase, DerivedDatabaseError},
    journal::{JournalAdvance, JournalPrefixIdentity, JournalReadView},
};
use redb::{ReadableDatabase as _, ReadableTable as _, TableDefinition};
use std::{
    io,
    sync::{Arc, atomic::Ordering},
};
use thiserror::Error;

/// Maximum serialized live recovery checkpoint; historical rows belong in indexes.
pub const MAX_RECOVERY_HEAD_BYTES: usize = 64 * 1024;
/// Maximum opaque metadata in an indexed source/boundary record.
pub const MAX_RECOVERY_ROW_BYTES: usize = 64 * 1024;
/// Maximum row mutations in one recovery transaction.
pub const MAX_RECOVERY_BATCH_ROWS: usize = 128;
/// Maximum charged metadata bytes in a transaction or returned page.
pub const MAX_RECOVERY_BATCH_BYTES: usize = 1024 * 1024;
const CACHE_BYTES: usize = 4 * 1024 * 1024;
const MAX_DATABASE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
type StoredHead<'a> = (u32, u64, &'a [u8], &'a [u8]);
const HEAD: TableDefinition<u8, StoredHead<'_>> = TableDefinition::new("recovery_head_v1");
const LOOKUPS: TableDefinition<(u8, &[u8]), &[u8]> = TableDefinition::new("recovery_lookups_v1");
pub const MAX_RECOVERY_LOOKUP_KEY_BYTES: usize = 1024;
const ROWS: TableDefinition<(u8, u64, u64), &[u8]> = TableDefinition::new("recovery_rows_v1");

/// Core owns namespace meanings; scope identifies an independent generation or index.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RecoveryKey {
    pub namespace: u8,
    pub scope: u64,
    pub ordinal: u64,
}
impl RecoveryKey {
    const fn stored(self) -> (u8, u64, u64) {
        (self.namespace, self.scope, self.ordinal)
    }
}

/// Bounded metadata; canonical message/output bodies remain in the journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryRow {
    pub key: RecoveryKey,
    pub payload: Vec<u8>,
}

/// Exact bounded identity key; payloads are source selectors, never historical bodies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryLookup {
    pub namespace: u8,
    pub key: Vec<u8>,
    pub payload: Vec<u8>,
}

/// One core-owned atomic index change.
#[derive(Clone, Debug)]
pub enum RecoveryMutation {
    Put(RecoveryRow),
    Delete(RecoveryKey),
}

/// Exact raw prefix and bounded control checkpoint published together with index updates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryIndexHead {
    pub version: u32,
    pub prefix: JournalPrefixIdentity,
    pub checkpoint: Vec<u8>,
}

/// Bounded metadata page in ordinal order, with an exclusive continuation cursor.
#[derive(Debug)]
pub struct RecoveryPage {
    pub rows: Vec<RecoveryRow>,
    pub next_cursor: Option<u64>,
    pub has_more: bool,
    pub retained_bytes: usize,
}

/// Actual descriptor I/O, excluding database cache hits.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecoveryIndexIo {
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub syncs: u64,
}

#[derive(Debug, Error)]
pub enum RecoveryIndexError {
    #[error("invalid recovery index: {0}")]
    Invalid(&'static str),
    #[error("recovery index limit: {0}")]
    Limit(&'static str),
    #[error("recovery index has another writer")]
    Busy,
    #[error("recovery index prefix changed")]
    Stale,
    #[error("recovery index I/O failed")]
    Io(#[from] io::Error),
    #[error("recovery index storage failed: {0}")]
    Storage(#[from] redb::Error),
    #[error("recovery source failed: {0}")]
    Journal(#[from] SessionStoreError),
}
impl From<DerivedDatabaseError> for RecoveryIndexError {
    fn from(error: DerivedDatabaseError) -> Self {
        match error {
            DerivedDatabaseError::Invalid(reason) => Self::Invalid(reason),
            DerivedDatabaseError::Busy => Self::Busy,
            DerivedDatabaseError::Io(error) => Self::Io(error),
            DerivedDatabaseError::Storage(error) => Self::Storage(error),
            DerivedDatabaseError::Journal(error) => Self::Journal(error),
        }
    }
}
fn storage(error: impl Into<redb::Error>) -> RecoveryIndexError {
    RecoveryIndexError::Storage(error.into())
}

/// Independent source-derived state owners share descriptor and transaction rules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryProjection {
    Conversation,
    Tasks,
}
impl RecoveryProjection {
    const fn directory_name(self) -> &'static str {
        match self {
            Self::Conversation => "recovery",
            Self::Tasks => "tasks",
        }
    }
}

/// Independent canonical recovery owner; display projections use their own database.
pub struct RecoveryIndex {
    owner: Arc<DerivedDatabase>,
}
impl RecoveryIndex {
    /// Open the current recovery schema and validate its exact canonical prefix.
    ///
    /// # Errors
    /// Rejects incompatible, unsafe, corrupt, foreign or concurrently owned state.
    pub fn open(
        view: &JournalReadView,
        projection: RecoveryProjection,
        version: u32,
    ) -> Result<Self, RecoveryIndexError> {
        Self::open_inner(view, projection, version, false)
    }

    /// Explicitly discard only this derived index before a bounded canonical rebuild.
    ///
    /// # Errors
    /// Rejects unsafe descriptors and concurrent writers; canonical data is never changed.
    pub fn rebuild(
        view: &JournalReadView,
        projection: RecoveryProjection,
        version: u32,
    ) -> Result<Self, RecoveryIndexError> {
        Self::open_inner(view, projection, version, true)
    }

    fn open_inner(
        view: &JournalReadView,
        projection: RecoveryProjection,
        version: u32,
        reset: bool,
    ) -> Result<Self, RecoveryIndexError> {
        let owner = DerivedDatabase::open(
            view,
            projection.directory_name(),
            CACHE_BYTES,
            MAX_DATABASE_BYTES,
            reset,
        )?;
        let index = Self {
            owner: Arc::new(owner),
        };
        let read = index.owner.database.begin_read().map_err(storage)?;
        let missing = matches!(
            read.open_table(HEAD),
            Err(redb::TableError::TableDoesNotExist(_))
        );
        drop(read);
        if missing && !index.owner.was_empty {
            return Err(RecoveryIndexError::Invalid("missing schema"));
        }
        if missing {
            let transaction = index.owner.database.begin_write().map_err(storage)?;
            transaction.open_table(ROWS).map_err(storage)?;
            transaction.open_table(LOOKUPS).map_err(storage)?;
            transaction
                .open_table(HEAD)
                .map_err(storage)?
                .insert(
                    0,
                    (
                        version,
                        0,
                        JournalPrefixIdentity::empty().digest.as_slice(),
                        &[][..],
                    ),
                )
                .map_err(storage)?;
            transaction.commit().map_err(storage)?;
        }
        let head = index.head()?;
        if head.version != version {
            return Err(RecoveryIndexError::Invalid("projection version"));
        }
        view.at_prefix(head.prefix)?;
        Ok(index)
    }

    /// Read bounded control state independently of lifetime row count.
    ///
    /// # Errors
    /// Rejects missing/oversized/corrupt head metadata or storage failure.
    pub fn head(&self) -> Result<RecoveryIndexHead, RecoveryIndexError> {
        let read = self.owner.database.begin_read().map_err(storage)?;
        read_head(&read)
    }

    /// Apply a bounded metadata batch with its already-verified journal transition.
    ///
    /// # Errors
    /// Rejects stale/foreign prefixes and resource overflow before modifying the index.
    pub fn apply(
        &mut self,
        advance: &JournalAdvance,
        checkpoint: &[u8],
        mutations: &[RecoveryMutation],
        lookups: &[RecoveryLookup],
    ) -> Result<(), RecoveryIndexError> {
        use std::os::unix::fs::MetadataExt as _;
        let incoming = advance.next().derived_directory()?.metadata()?;
        let owned = self.owner.directory.metadata()?;
        if incoming.dev() != owned.dev() || incoming.ino() != owned.ino() {
            return Err(RecoveryIndexError::Invalid("foreign journal"));
        }
        if checkpoint.len() > MAX_RECOVERY_HEAD_BYTES
            || mutations.len().saturating_add(lookups.len()) > MAX_RECOVERY_BATCH_ROWS
        {
            return Err(RecoveryIndexError::Limit("head/batch rows"));
        }
        let mut charged = checkpoint.len();
        for mutation in mutations {
            let payload = match mutation {
                RecoveryMutation::Put(row) => row.payload.len(),
                RecoveryMutation::Delete(_) => 0,
            };
            if payload > MAX_RECOVERY_ROW_BYTES {
                return Err(RecoveryIndexError::Limit("row bytes"));
            }
            charged += payload + 24;
            if charged > MAX_RECOVERY_BATCH_BYTES {
                return Err(RecoveryIndexError::Limit("batch bytes"));
            }
        }
        for lookup in lookups {
            validate_lookup(&lookup.key, &lookup.payload)?;
            charged = charged
                .saturating_add(lookup.key.len())
                .saturating_add(lookup.payload.len())
                .saturating_add(24);
            if charged > MAX_RECOVERY_BATCH_BYTES {
                return Err(RecoveryIndexError::Limit("batch bytes"));
            }
        }
        let transaction = self.owner.database.begin_write().map_err(storage)?;
        let head = read_head_write(&transaction)?;
        if head.prefix != advance.previous() {
            return Err(RecoveryIndexError::Stale);
        }
        {
            let mut rows = transaction.open_table(ROWS).map_err(storage)?;
            for mutation in mutations {
                match mutation {
                    RecoveryMutation::Put(row) => {
                        rows.insert(row.key.stored(), row.payload.as_slice())
                            .map_err(storage)?;
                    }
                    RecoveryMutation::Delete(key) => {
                        rows.remove(key.stored()).map_err(storage)?;
                    }
                }
            }
        }
        {
            let mut table = transaction.open_table(LOOKUPS).map_err(storage)?;
            for lookup in lookups {
                table
                    .insert(
                        (lookup.namespace, lookup.key.as_slice()),
                        lookup.payload.as_slice(),
                    )
                    .map_err(storage)?;
            }
        }
        let prefix = advance.next().prefix_identity();
        transaction
            .open_table(HEAD)
            .map_err(storage)?
            .insert(
                0,
                (
                    head.version,
                    prefix.next_sequence,
                    prefix.digest.as_slice(),
                    checkpoint,
                ),
            )
            .map_err(storage)?;
        transaction.commit().map_err(storage)?;
        Ok(())
    }

    /// Capture one consistent head and row snapshot, retaining the index lock.
    ///
    /// # Errors
    /// Rejects invalid head metadata or storage failure.
    pub fn read(&self) -> Result<RecoveryReadView, RecoveryIndexError> {
        let read = self.owner.database.begin_read().map_err(storage)?;
        let head = read_head(&read)?;
        Ok(RecoveryReadView {
            read,
            head,
            owner: Arc::clone(&self.owner),
        })
    }

    /// Read physical I/O counters for independent cold-open/read/update qualification.
    #[must_use]
    pub fn io_metrics(&self) -> RecoveryIndexIo {
        RecoveryIndexIo {
            bytes_read: self.owner.counters.read.load(Ordering::Relaxed),
            bytes_written: self.owner.counters.written.load(Ordering::Relaxed),
            syncs: self.owner.counters.syncs.load(Ordering::Relaxed),
        }
    }
}
/// A consistent canonical metadata snapshot across all materialization pages.
/// The independent file lock remains held until the last snapshot is dropped.
pub struct RecoveryReadView {
    read: redb::ReadTransaction,
    head: RecoveryIndexHead,
    owner: Arc<DerivedDatabase>,
}
impl RecoveryReadView {
    /// Exact source prefix and control state belonging to this row snapshot.
    #[must_use]
    pub const fn head(&self) -> &RecoveryIndexHead {
        &self.head
    }

    /// Bind this snapshot to the same session and an exact captured raw prefix.
    /// Capture the index first, then the journal, so later source commits are safe.
    ///
    /// # Errors
    /// Rejects foreign sessions, stale source prefixes and unsafe descriptors.
    pub fn bind_source(
        &self,
        source: &JournalReadView,
    ) -> Result<JournalReadView, RecoveryIndexError> {
        use std::os::unix::fs::MetadataExt as _;
        let incoming = source.derived_directory()?.metadata()?;
        let owned = self.owner.directory.metadata()?;
        if incoming.dev() != owned.dev() || incoming.ino() != owned.ino() {
            return Err(RecoveryIndexError::Invalid("foreign journal"));
        }
        Ok(source.at_prefix(self.head.prefix)?)
    }

    /// Read one bounded source/boundary record by exact key.
    ///
    /// # Errors
    /// Rejects corrupt record lengths or storage failure.
    pub fn get(&self, key: RecoveryKey) -> Result<Option<RecoveryRow>, RecoveryIndexError> {
        let rows = self.read.open_table(ROWS).map_err(storage)?;
        rows.get(key.stored())
            .map_err(storage)?
            .map(|value| decode_row(key, value.value()))
            .transpose()
    }

    /// Read an exact identity selector from this consistent source prefix.
    ///
    /// # Errors
    /// Rejects oversized keys/payloads and storage failures.
    pub fn lookup(&self, namespace: u8, key: &[u8]) -> Result<Option<Vec<u8>>, RecoveryIndexError> {
        validate_lookup(key, &[])?;
        let table = self.read.open_table(LOOKUPS).map_err(storage)?;
        table
            .get((namespace, key))
            .map_err(storage)?
            .map(|value| {
                validate_lookup(key, value.value())?;
                Ok(value.value().to_vec())
            })
            .transpose()
    }

    /// Find the immediately preceding record in one namespace and scope.
    ///
    /// # Errors
    /// Rejects corrupt row lengths and storage failures.
    pub fn last_before(
        &self,
        namespace: u8,
        scope: u64,
        ordinal: u64,
    ) -> Result<Option<RecoveryRow>, RecoveryIndexError> {
        let rows = self.read.open_table(ROWS).map_err(storage)?;
        let mut range = rows
            .range((namespace, scope, 0)..(namespace, scope, ordinal))
            .map_err(storage)?;
        range
            .next_back()
            .map(|entry| {
                let (key, value) = entry.map_err(storage)?;
                let (_, _, ordinal) = key.value();
                decode_row(
                    RecoveryKey {
                        namespace,
                        scope,
                        ordinal,
                    },
                    value.value(),
                )
            })
            .transpose()
    }

    /// Read a bounded ordinal page within one namespace/generation.
    ///
    /// # Errors
    /// Rejects zero/excessive limits, cursor overflow and corrupt metadata.
    pub fn page(
        &self,
        namespace: u8,
        scope: u64,
        after: Option<u64>,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<RecoveryPage, RecoveryIndexError> {
        if max_rows == 0
            || max_rows > MAX_RECOVERY_BATCH_ROWS
            || max_bytes == 0
            || max_bytes > MAX_RECOVERY_BATCH_BYTES
        {
            return Err(RecoveryIndexError::Limit("page limits"));
        }
        let first = after
            .map(|cursor| {
                cursor
                    .checked_add(1)
                    .ok_or(RecoveryIndexError::Limit("cursor overflow"))
            })
            .transpose()?
            .unwrap_or(0);
        let rows = self.read.open_table(ROWS).map_err(storage)?;
        let mut range = rows
            .range((namespace, scope, first)..=(namespace, scope, u64::MAX))
            .map_err(storage)?;
        let mut page = RecoveryPage {
            rows: Vec::new(),
            next_cursor: after,
            has_more: false,
            retained_bytes: 0,
        };
        for entry in &mut range {
            let (key, value) = entry.map_err(storage)?;
            let (_, _, ordinal) = key.value();
            let payload = value.value();
            if payload.len() > MAX_RECOVERY_ROW_BYTES {
                return Err(RecoveryIndexError::Invalid("row bytes"));
            }
            let charged = payload.len() + 24;
            if page.rows.len() == max_rows || page.retained_bytes + charged > max_bytes {
                if page.rows.is_empty() {
                    return Err(RecoveryIndexError::Limit("page cannot fit row"));
                }
                page.has_more = true;
                break;
            }
            page.rows.push(RecoveryRow {
                key: RecoveryKey {
                    namespace,
                    scope,
                    ordinal,
                },
                payload: payload.to_vec(),
            });
            page.next_cursor = Some(ordinal);
            page.retained_bytes += charged;
        }
        Ok(page)
    }
}

fn validate_lookup(key: &[u8], payload: &[u8]) -> Result<(), RecoveryIndexError> {
    if key.is_empty()
        || key.len() > MAX_RECOVERY_LOOKUP_KEY_BYTES
        || payload.len() > MAX_RECOVERY_ROW_BYTES
    {
        return Err(RecoveryIndexError::Limit("lookup key/payload bytes"));
    }
    Ok(())
}
fn decode_row(key: RecoveryKey, payload: &[u8]) -> Result<RecoveryRow, RecoveryIndexError> {
    if payload.len() > MAX_RECOVERY_ROW_BYTES {
        return Err(RecoveryIndexError::Invalid("row bytes"));
    }
    Ok(RecoveryRow {
        key,
        payload: payload.to_vec(),
    })
}
fn decode_head(value: StoredHead<'_>) -> Result<RecoveryIndexHead, RecoveryIndexError> {
    let (version, next_sequence, digest, checkpoint) = value;
    if checkpoint.len() > MAX_RECOVERY_HEAD_BYTES {
        return Err(RecoveryIndexError::Invalid("head bytes"));
    }
    let digest = digest
        .try_into()
        .map_err(|_| RecoveryIndexError::Invalid("prefix digest"))?;
    Ok(RecoveryIndexHead {
        version,
        prefix: JournalPrefixIdentity {
            next_sequence,
            digest,
        },
        checkpoint: checkpoint.to_vec(),
    })
}
fn read_head(read: &redb::ReadTransaction) -> Result<RecoveryIndexHead, RecoveryIndexError> {
    let table = read.open_table(HEAD).map_err(storage)?;
    let value = table
        .get(0)
        .map_err(storage)?
        .ok_or(RecoveryIndexError::Invalid("missing head"))?;
    decode_head(value.value())
}
fn read_head_write(
    write: &redb::WriteTransaction,
) -> Result<RecoveryIndexHead, RecoveryIndexError> {
    let table = write.open_table(HEAD).map_err(storage)?;
    let value = table
        .get(0)
        .map_err(storage)?
        .ok_or(RecoveryIndexError::Invalid("missing head"))?;
    decode_head(value.value())
}

#[cfg(test)]
mod tests;
