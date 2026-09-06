//! Descriptor-owned derived database machinery shared by semantic projections.

use super::exclusive_lock::ExclusiveFileLock;
use super::{journal::JournalReadView, sync_event_file};
use redb::{Database, StorageBackend};
use std::{
    fs::File,
    io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum DerivedDatabaseError {
    #[error("invalid derived database: {0}")]
    Invalid(&'static str),
    #[error("derived database is busy")]
    Busy,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Storage(#[from] redb::Error),
    #[error(transparent)]
    Journal(#[from] super::SessionStoreError),
}

pub(crate) struct DerivedDatabase {
    pub(crate) database: Database,
    pub(crate) directory: File,
    pub(crate) lock: ExclusiveFileLock,
    pub(crate) counters: Arc<IoCounters>,
    pub(crate) was_empty: bool,
}

impl DerivedDatabase {
    pub(crate) fn open(
        view: &JournalReadView,
        name: &str,
        cache_bytes: usize,
        max_bytes: u64,
        reset: bool,
    ) -> Result<Self, DerivedDatabaseError> {
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || cache_bytes == 0
            || max_bytes == 0
        {
            return Err(DerivedDatabaseError::Invalid("database name or limits"));
        }
        let directory = view.derived_directory()?;
        let lock = open_file(&directory, &format!("{name}.lock"))?;
        let lock = ExclusiveFileLock::try_acquire(lock).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                DerivedDatabaseError::Busy
            } else {
                DerivedDatabaseError::Io(error)
            }
        })?;
        let file = open_file(&directory, &format!("{name}.redb"))?;
        if reset {
            file.set_len(0)?;
            sync_event_file(&file)?;
        }
        let size = file.metadata()?.len();
        let mut was_empty = size == 0;
        check_extent(max_bytes, 0, size)?;
        let counters = Arc::new(IoCounters::default());
        let database = match create_database(file.try_clone()?, cache_bytes, max_bytes, &counters) {
            Ok(database) => database,
            Err(DerivedDatabaseError::Storage(redb::Error::RepairAborted)) if !was_empty => {
                // The journal owns every projection fact. A crashed database must
                // not force an unbounded redb repair scan or prevent session resume.
                // Keep the independent writer lock and reset the same verified
                // descriptor, never reopen an untrusted replacement path.
                file.set_len(0)?;
                sync_event_file(&file)?;
                was_empty = true;
                create_database(file, cache_bytes, max_bytes, &counters)?
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            database,
            directory,
            lock,
            counters,
            was_empty,
        })
    }
}

fn create_database(
    file: File,
    cache_bytes: usize,
    max_bytes: u64,
    counters: &Arc<IoCounters>,
) -> Result<Database, DerivedDatabaseError> {
    let backend = BoundedFile {
        inner: redb::backends::FileBackend::new(file).map_err(storage)?,
        counters: Arc::clone(counters),
        max_bytes,
    };
    Database::builder()
        .set_cache_size(cache_bytes)
        .set_repair_callback(redb::RepairSession::abort)
        .create_with_backend(backend)
        .map_err(storage)
}

fn storage(error: impl Into<redb::Error>) -> DerivedDatabaseError {
    DerivedDatabaseError::Storage(error.into())
}

#[derive(Debug, Default)]
pub(crate) struct IoCounters {
    pub(crate) read: AtomicU64,
    pub(crate) written: AtomicU64,
    pub(crate) syncs: AtomicU64,
}

#[derive(Debug)]
struct BoundedFile {
    max_bytes: u64,
    inner: redb::backends::FileBackend,
    counters: Arc<IoCounters>,
}
impl StorageBackend for BoundedFile {
    fn len(&self) -> io::Result<u64> {
        self.inner.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        check_extent(self.max_bytes, offset, out.len() as u64)?;
        self.inner.read(offset, out)?;
        self.counters
            .read
            .fetch_add(out.len() as u64, Ordering::Relaxed);
        Ok(())
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        check_extent(self.max_bytes, 0, len)?;
        self.inner.set_len(len)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data()?;
        self.counters.syncs.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        check_extent(self.max_bytes, offset, data.len() as u64)?;
        self.inner.write(offset, data)?;
        self.counters
            .written
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(())
    }
    fn close(&self) -> io::Result<()> {
        self.inner.close()
    }
}

fn check_extent(max_bytes: u64, offset: u64, len: u64) -> io::Result<()> {
    if offset.checked_add(len).is_none_or(|end| end > max_bytes) {
        return Err(io::Error::other("derived database size limit"));
    }
    Ok(())
}

fn open_file(directory: &File, name: &str) -> Result<File, DerivedDatabaseError> {
    use rustix::fs::{Mode, OFlags};
    let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let fd = match rustix::fs::openat(directory, name, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => {
            let fd = rustix::fs::openat(
                directory,
                name,
                flags | OFlags::CREATE | OFlags::EXCL,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(io::Error::from)?;
            sync_event_file(directory)?;
            fd
        }
        Err(error) => return Err(io::Error::from(error).into()),
    };
    let file = File::from(fd);
    let stat = rustix::fs::fstat(&file).map_err(io::Error::from)?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
        return Err(DerivedDatabaseError::Invalid("unsafe index descriptor"));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use redb::ReadableDatabase as _;

    const CRASH_TABLE: redb::TableDefinition<u64, u64> = redb::TableDefinition::new("crash_marker");

    #[test]
    fn unclean_projection_is_reset_under_its_original_writer_lock() {
        const CHILD_ROOT: &str = "ROTTWEILER_DERIVED_CRASH_FIXTURE";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let log = super::super::SessionEventLog::open(std::path::Path::new(&root), "crash")
                .expect("child journal");
            let owner = DerivedDatabase::open(
                &log.read_view(),
                "recovery",
                1024 * 1024,
                16 * 1024 * 1024,
                false,
            )
            .expect("child projection");
            let transaction = owner.database.begin_write().expect("transaction");
            transaction
                .open_table(CRASH_TABLE)
                .expect("table")
                .insert(1, 2)
                .expect("insert");
            transaction.commit().expect("durable index transaction");
            // Exit without running redb's clean-shutdown destructors.
            std::process::exit(0);
        }
        let root = tempfile::tempdir().expect("root");
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "session::derived_database::tests::unclean_projection_is_reset_under_its_original_writer_lock", "--nocapture"])
            .env(CHILD_ROOT, root.path())
            .status().expect("crashed projection process");
        assert!(status.success());
        let log =
            super::super::SessionEventLog::open(root.path(), "crash").expect("source survives");
        let view = log.read_view();
        let owner = DerivedDatabase::open(&view, "recovery", 1024 * 1024, 16 * 1024 * 1024, false)
            .expect("unclean projection resets for bounded source catch-up");
        assert!(owner.was_empty);
        let read = owner.database.begin_read().expect("reader");
        assert!(matches!(
            read.open_table(CRASH_TABLE),
            Err(redb::TableError::TableDoesNotExist(_))
        ));
        assert!(matches!(
            DerivedDatabase::open(&view, "recovery", 1024 * 1024, 16 * 1024 * 1024, false),
            Err(DerivedDatabaseError::Busy)
        ));
    }

    #[test]
    fn owner_drop_releases_its_lock_even_when_a_fork_inherited_the_description() {
        let root = tempfile::tempdir().expect("root");
        let journal = super::super::SessionEventLog::open(root.path(), "session").expect("journal");
        let view = journal.read_view();
        let owner =
            DerivedDatabase::open(&view, "transcript", 1024 * 1024, 16 * 1024 * 1024, false)
                .expect("owner");
        // A duplicated descriptor shares exactly the flock description inherited
        // across fork, including the interval before CLOEXEC can close the child FD.
        let inherited = owner
            .lock
            .descriptor()
            .try_clone()
            .expect("inherited description");
        drop(owner);
        let replacement =
            DerivedDatabase::open(&view, "transcript", 1024 * 1024, 16 * 1024 * 1024, false)
                .expect("owner settlement must release the lock before close");
        drop(inherited);
        assert!(
            matches!(
                DerivedDatabase::open(&view, "transcript", 1024 * 1024, 16 * 1024 * 1024, false),
                Err(DerivedDatabaseError::Busy)
            ),
            "closing inherited descriptors cannot release the replacement's lock"
        );
        drop(replacement);
    }

    #[test]
    fn derived_database_owners_have_independent_locks_and_bounded_extents() {
        let root = tempfile::tempdir().expect("root");
        let journal = super::super::SessionEventLog::open(root.path(), "session").expect("journal");
        let view = journal.read_view();
        let transcript =
            DerivedDatabase::open(&view, "transcript", 1024 * 1024, 16 * 1024 * 1024, false)
                .expect("transcript");
        let recovery =
            DerivedDatabase::open(&view, "recovery", 1024 * 1024, 16 * 1024 * 1024, false)
                .expect("independent recovery");
        assert!(matches!(
            DerivedDatabase::open(&view, "recovery", 1024 * 1024, 16 * 1024 * 1024, false),
            Err(DerivedDatabaseError::Busy)
        ));
        assert!(check_extent(1024, 1020, 5).is_err());
        assert!(check_extent(u64::MAX, u64::MAX, 1).is_err());
        drop(recovery);
        drop(transcript);
    }
}
