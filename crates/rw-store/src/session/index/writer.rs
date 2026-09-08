//! One checked connection per retained index owner; query cancellation stays on read handles.
use super::super::{SessionStoreError, sqlite_schema, sqlite_snapshot::validate_read_only_index};
use rusqlite::{Connection, OpenFlags};
use std::{
    fs::{self, File, OpenOptions},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, TryLockError},
    time::{Duration, Instant},
};

const WRITER_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(super) struct IndexWriter {
    // Close SQLite before releasing the descriptors which qualify its namespace.
    state: Mutex<WriterState>,
    root: PathBuf,
    root_file: File,
    database_file: File,
}
#[derive(Debug)]
struct WriterState {
    connection: Connection,
    schema_version: i64,
}

pub(super) struct WriterGuard<'a>(MutexGuard<'a, WriterState>);
impl Deref for WriterGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Self::Target {
        &self.0.connection
    }
}
impl DerefMut for WriterGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0.connection
    }
}

impl WriterGuard<'_> {
    pub(super) fn transaction(&mut self) -> Result<rusqlite::Transaction<'_>, SessionStoreError> {
        self.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
    }

    pub(super) fn transaction_with_behavior(
        &mut self,
        behavior: rusqlite::TransactionBehavior,
    ) -> Result<rusqlite::Transaction<'_>, SessionStoreError> {
        let expected = self.0.schema_version;
        let transaction = self.0.connection.transaction_with_behavior(behavior)?;
        // A different process can change schema between admission and BEGIN.
        let actual: i64 = transaction.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
        if actual != expected {
            sqlite_schema::validate_accounting(&transaction)?;
            sqlite_schema::validate_sessions(&transaction)?;
        }
        Ok(transaction)
    }
}

impl IndexWriter {
    pub(super) fn open(root: &Path, reset_search: bool) -> Result<Self, SessionStoreError> {
        let _open = tracing::trace_span!(target: "rw_performance", "search.writer_open").entered();
        fs::create_dir_all(root)?;
        let root = fs::canonicalize(root)?;
        let root_file = File::open(&root)?;
        let path = root.join("index.sqlite");
        let database_file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => File::open(&path)?,
            Err(error) => return Err(error.into()),
        };
        if !same_identity(
            &validate_read_only_index(&path)?,
            &database_file.metadata()?,
        ) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let mut owner = Self {
            root,
            root_file,
            database_file,
            state: Mutex::new(WriterState {
                connection,
                schema_version: 0,
            }),
        };
        owner.validate_identity()?;
        let state = owner
            .state
            .get_mut()
            .map_err(|_| SessionStoreError::IndexWriterUnavailable)?;
        initialize(&mut state.connection, reset_search)?;
        state.schema_version = state
            .connection
            .query_row("PRAGMA schema_version", [], |row| row.get(0))?;
        owner.validate_identity()?;
        Ok(owner)
    }

    pub(super) fn acquire(&self) -> Result<WriterGuard<'_>, SessionStoreError> {
        let wait = tracing::trace_span!(target: "rw_performance", "search.writer_wait").entered();
        let deadline = Instant::now() + WRITER_WAIT;
        let mut guard = loop {
            match self.state.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::Poisoned(_)) => {
                    return Err(SessionStoreError::IndexWriterUnavailable);
                }
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(SessionStoreError::IndexWriterUnavailable);
                }
            }
        };
        drop(wait);
        self.validate_identity()?;
        let version = guard
            .connection
            .query_row("PRAGMA schema_version", [], |row| row.get(0))?;
        if version != guard.schema_version {
            sqlite_schema::validate_accounting(&guard.connection)?;
            sqlite_schema::validate_sessions(&guard.connection)?;
            guard.schema_version = version;
        }
        Ok(WriterGuard(guard))
    }

    fn validate_identity(&self) -> Result<(), SessionStoreError> {
        let root = fs::symlink_metadata(&self.root)?;
        let database = validate_read_only_index(&self.root.join("index.sqlite"))?;
        if !root.is_dir()
            || !same_identity(&root, &self.root_file.metadata()?)
            || !same_identity(&database, &self.database_file.metadata()?)
        {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        Ok(())
    }
}

#[cfg(unix)]
fn same_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}
#[cfg(not(unix))]
fn same_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.created()
        .ok()
        .zip(right.created().ok())
        .is_some_and(|(left, right)| left == right)
}

fn initialize(connection: &mut Connection, reset_search: bool) -> Result<(), SessionStoreError> {
    sqlite_schema::validate_accounting(connection)?;
    if !reset_search {
        sqlite_schema::validate_sessions(connection)?;
    }
    sqlite_schema::configure_connection(connection)?;
    // The page-cache target excludes SQLite scratch and WAL frames pinned by read snapshots.
    connection
        .execute_batch("PRAGMA cache_size=-1024; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;")?;
    sqlite_schema::ensure_accounting_schema(connection)?;
    if reset_search {
        let transaction = connection.transaction()?;
        transaction.execute_batch("DROP TRIGGER IF EXISTS search_documents_ai; DROP TRIGGER IF EXISTS search_documents_ad; DROP TRIGGER IF EXISTS search_documents_au; DROP TABLE IF EXISTS sessions_fts; DROP TABLE IF EXISTS search_documents; DROP TABLE IF EXISTS sessions; DROP TABLE IF EXISTS search_invocations;")?;
        sqlite_schema::create_sessions_schema(&transaction)?;
        transaction.commit()?;
    } else {
        sqlite_schema::ensure_sessions_schema(connection)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
