//! `SQLite` owns a live read transaction; search never copies the lifetime database.
use super::{SessionStoreError, sqlite_snapshot::validate_read_only_index};
use rusqlite::{Connection, OpenFlags};
use std::{fs, path::Path};

pub(super) fn read_index<T>(
    root: &Path,
    read: impl FnOnce(&Connection) -> Result<T, SessionStoreError>,
) -> Result<T, SessionStoreError> {
    let path = root.join("index.sqlite");
    let before = validate_read_only_index(&path)?;
    let canonical_root = fs::canonicalize(root)?;
    let canonical_path = fs::canonicalize(&path)?;
    if canonical_path.parent() != Some(canonical_root.as_path()) {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    let connection = Connection::open_with_flags(
        canonical_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA query_only=ON; PRAGMA cache_size=-1024; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;")?;
    if !same_index_file(&before, &validate_read_only_index(&path)?) {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    connection.execute_batch("BEGIN DEFERRED")?;
    let result = read(&connection);
    connection.execute_batch("ROLLBACK")?;
    if !same_index_file(&before, &validate_read_only_index(&path)?) {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    result
}

#[cfg(unix)]
fn same_index_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}
#[cfg(not(unix))]
fn same_index_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    super::sqlite_snapshot::same_file_identity(left, right)
}
