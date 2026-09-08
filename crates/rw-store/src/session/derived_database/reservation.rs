//! Reserve the source-bound namespace before allocating a derived database.
use super::{AdvisoryFileLock, DerivedDatabaseError, JournalReadView, open_file};
use std::{fs::File, io, sync::Arc};

pub(crate) struct DerivedReservation {
    pub(crate) directory: File,
    pub(crate) lock: Arc<AdvisoryFileLock>,
    pub(crate) name: String,
}
impl DerivedReservation {
    pub(crate) fn open(view: &JournalReadView, name: &str) -> Result<Self, DerivedDatabaseError> {
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(DerivedDatabaseError::Invalid("database name"));
        }
        let directory = view.derived_directory()?;
        let lock = open_file(&directory, &format!("{name}.lock"))?;
        let lock = AdvisoryFileLock::try_exclusive(lock).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                DerivedDatabaseError::Busy
            } else {
                DerivedDatabaseError::Io(error)
            }
        })?;
        Ok(Self {
            directory,
            lock: Arc::new(lock),
            name: name.to_owned(),
        })
    }

    pub(crate) fn database_exists(&self) -> Result<bool, DerivedDatabaseError> {
        use rustix::fs::{Mode, OFlags};
        let file = match rustix::fs::openat(
            &self.directory,
            format!("{}.redb", self.name),
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(file) => file,
            Err(rustix::io::Errno::NOENT) => return Ok(false),
            Err(error) => return Err(io::Error::from(error).into()),
        };
        let stat = rustix::fs::fstat(&file).map_err(io::Error::from)?;
        if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
            return Err(DerivedDatabaseError::Invalid("unsafe index descriptor"));
        }
        Ok(true)
    }
}
