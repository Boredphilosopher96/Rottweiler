//! An exact empty prefix owns only its namespace; the first mutation opens the cache.
use super::{
    CACHE_BYTES, HEAD, LOOKUPS, MAX_DATABASE_BYTES, ROWS, RecoveryIndexError, RecoveryIndexHead,
    RecoveryIndexIo, RecoveryProjection, read_head, storage,
};
use crate::session::{
    derived_database::{DerivedDatabase, DerivedReservation},
    journal::{JournalPrefixIdentity, JournalReadView},
};
use redb::ReadableDatabase as _;
use std::sync::{Arc, atomic::Ordering};

#[derive(Clone)]
pub(super) enum RecoveryOwner {
    Empty {
        reservation: Arc<DerivedReservation>,
        version: u32,
    },
    Stored(Arc<DerivedDatabase>),
}
impl RecoveryOwner {
    pub(super) fn open(
        view: &JournalReadView,
        projection: RecoveryProjection,
        version: u32,
        reset: bool,
    ) -> Result<Self, RecoveryIndexError> {
        let reservation = Arc::new(DerivedReservation::open(view, projection.directory_name())?);
        if view.prefix_identity() == JournalPrefixIdentity::empty()
            && !reservation.database_exists()?
        {
            return Ok(Self::Empty {
                reservation,
                version,
            });
        }
        Ok(Self::Stored(open_database(
            &reservation,
            view,
            version,
            reset,
        )?))
    }
    pub(super) fn initialize(
        &mut self,
        source: &JournalReadView,
    ) -> Result<(), RecoveryIndexError> {
        if let Self::Empty {
            reservation,
            version,
        } = self
        {
            let owner = open_database(reservation, source, *version, false)?;
            *self = Self::Stored(owner);
        }
        Ok(())
    }
    pub(super) fn stored(&self) -> Result<&Arc<DerivedDatabase>, RecoveryIndexError> {
        match self {
            Self::Stored(owner) => Ok(owner),
            Self::Empty { .. } => Err(RecoveryIndexError::Invalid("uninitialized cache")),
        }
    }
    pub(super) fn directory(&self) -> &std::fs::File {
        match self {
            Self::Empty { reservation, .. } => &reservation.directory,
            Self::Stored(owner) => &owner.directory,
        }
    }
    pub(super) fn empty_head(&self) -> Option<RecoveryIndexHead> {
        match self {
            Self::Empty { version, .. } => Some(RecoveryIndexHead {
                version: *version,
                prefix: JournalPrefixIdentity::empty(),
                checkpoint: Vec::new(),
            }),
            Self::Stored(_) => None,
        }
    }
    pub(super) fn io_metrics(&self) -> RecoveryIndexIo {
        match self {
            Self::Empty { .. } => RecoveryIndexIo::default(),
            Self::Stored(owner) => RecoveryIndexIo {
                bytes_read: owner.counters.read.load(Ordering::Relaxed),
                bytes_written: owner.counters.written.load(Ordering::Relaxed),
                syncs: owner.counters.syncs.load(Ordering::Relaxed),
            },
        }
    }
}
fn open_database(
    reservation: &DerivedReservation,
    view: &JournalReadView,
    version: u32,
    reset: bool,
) -> Result<Arc<DerivedDatabase>, RecoveryIndexError> {
    let owner = Arc::new(DerivedDatabase::open_reserved(
        reservation,
        CACHE_BYTES,
        MAX_DATABASE_BYTES,
        reset,
    )?);
    let read = owner.database.begin_read().map_err(storage)?;
    let missing = matches!(
        read.open_table(HEAD),
        Err(redb::TableError::TableDoesNotExist(_))
    );
    drop(read);
    if missing && !owner.was_empty {
        return Err(RecoveryIndexError::Invalid("missing schema"));
    }
    if missing {
        let transaction = owner.database.begin_write().map_err(storage)?;
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
    let head = read_head(&owner.database.begin_read().map_err(storage)?)?;
    if head.version != version {
        return Err(RecoveryIndexError::Invalid("projection version"));
    }
    view.at_prefix(head.prefix)?;
    Ok(owner)
}
