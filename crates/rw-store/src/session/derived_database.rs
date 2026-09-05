//! Descriptor-owned derived database machinery shared by semantic projections.

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
    pub(crate) lock: File,
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
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |error| {
                if error == rustix::io::Errno::WOULDBLOCK {
                    DerivedDatabaseError::Busy
                } else {
                    io::Error::from(error).into()
                }
            },
        )?;
        let file = open_file(&directory, &format!("{name}.redb"))?;
        if reset {
            file.set_len(0)?;
            sync_event_file(&file)?;
        }
        let size = file.metadata()?.len();
        let was_empty = size == 0;
        check_extent(max_bytes, 0, size)?;
        let counters = Arc::new(IoCounters::default());
        let backend = BoundedFile {
            inner: redb::backends::FileBackend::new(file).map_err(storage)?,
            counters: Arc::clone(&counters),
            max_bytes,
        };
        let mut builder = Database::builder();
        builder
            .set_cache_size(cache_bytes)
            .set_repair_callback(move |repair| {
                if !was_empty {
                    repair.abort();
                }
            });
        let database = builder.create_with_backend(backend).map_err(storage)?;
        Ok(Self {
            database,
            directory,
            lock,
            counters,
            was_empty,
        })
    }
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
