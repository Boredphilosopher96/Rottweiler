//! Private, per-project persistent agent memory.

use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::{
    ffi::OsStrExt as _,
    fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _},
};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt as _;

use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};
use thiserror::Error;

const SCHEMA_VERSION: i64 = 1;
const MEMORY_STORAGE_DIRECTORY: &str = "project-memory-v1";
/// Maximum UTF-8 bytes accepted in one memory entry.
pub const MAX_MEMORY_ENTRY_BYTES: usize = 64 * 1024;
/// Maximum aggregate UTF-8 content bytes retained per project.
pub const MAX_PROJECT_MEMORY_BYTES: usize = 4 * 1024 * 1024;

/// One persistent project-memory record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryEntry {
    /// Monotonic project-local identifier.
    pub id: i64,
    /// Exact user/agent-authored UTF-8 content.
    pub content: String,
}

/// Store bound to a canonical-workspace key beneath host-owned storage.
///
/// The project directory name is a domain-separated `BLAKE3` digest of the
/// canonical workspace path, avoiding both path disclosure and collisions
/// between same-named workspaces. The private directories are mode 0700 and
/// database mode 0600 on Unix. `SQLite` is opened with
/// `SQLITE_OPEN_NOFOLLOW`, uses full durability, and provides process-safe
/// transactional writes. No file is created inside the workspace.
#[derive(Clone, Debug)]
pub struct ProjectMemoryStore {
    workspace: PathBuf,
    database: PathBuf,
}

impl ProjectMemoryStore {
    /// Opens or creates private memory beneath `storage_root`.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid workspace/storage root,
    /// symlinked/private paths, unsafe permissions, or database initialization
    /// failure.
    pub fn open_in(storage_root: &Path, workspace: &Path) -> Result<Self, MemoryError> {
        let workspace = canonical_workspace(workspace)?;
        ensure_directory(storage_root, true)?;
        let storage_root = canonical_storage_root(storage_root)?;
        let memory_root = storage_root.join(MEMORY_STORAGE_DIRECTORY);
        ensure_directory(&memory_root, true)?;
        let project_root = memory_root.join(workspace_storage_key(&workspace));
        ensure_directory(&project_root, true)?;
        let database = project_root.join("memory.sqlite3");
        prepare_database_file(&database)?;
        let store = Self {
            workspace,
            database,
        };
        store.initialize()
    }

    /// Opens existing private project memory without creating any state.
    ///
    /// This is suitable for fresh-session context assembly: an absent memory
    /// database returns `None` and leaves host storage and the workspace
    /// untouched.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid roots, unsafe storage paths or permissions,
    /// and malformed/unsupported databases.
    pub fn open_existing_in(
        storage_root: &Path,
        workspace: &Path,
    ) -> Result<Option<Self>, MemoryError> {
        let workspace = canonical_workspace(workspace)?;
        let storage_root = match fs::symlink_metadata(storage_root) {
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(MemoryError::Io {
                    operation: "inspect project-memory storage root",
                    path: storage_root.to_path_buf(),
                    source,
                });
            }
            Ok(_) => canonical_storage_root(storage_root)?,
        };
        let memory_root = storage_root.join(MEMORY_STORAGE_DIRECTORY);
        if !validate_optional_private_directory(&memory_root)? {
            return Ok(None);
        }
        let project_root = memory_root.join(workspace_storage_key(&workspace));
        if !validate_optional_private_directory(&project_root)? {
            return Ok(None);
        }
        let database = project_root.join("memory.sqlite3");
        match fs::symlink_metadata(&database) {
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(MemoryError::Io {
                    operation: "inspect private memory database",
                    path: database,
                    source,
                });
            }
            Ok(_) => validate_database_file(&database)?,
        }
        let store = Self {
            workspace,
            database,
        };
        store.initialize().map(Some)
    }

    fn initialize(self) -> Result<Self, MemoryError> {
        let connection = self.connect()?;
        connection.execute_batch(
            "PRAGMA journal_mode=DELETE;
             PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS metadata (
                 key TEXT PRIMARY KEY NOT NULL,
                 value INTEGER NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS memory_entries (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 content TEXT NOT NULL CHECK(length(CAST(content AS BLOB)) > 0)
             ) STRICT;",
        )?;
        let version = connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        match version {
            None => {
                connection.execute(
                    "INSERT INTO metadata(key, value) VALUES ('schema_version', ?1)",
                    [SCHEMA_VERSION],
                )?;
            }
            Some(version) if version == SCHEMA_VERSION => {}
            Some(version) => return Err(MemoryError::UnsupportedSchema { version }),
        }
        drop(connection);
        Ok(self)
    }

    /// Canonical project root owning this memory.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Private database path, exposed for diagnostics only.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database
    }

    /// Appends one durable entry and returns its project-local id.
    ///
    /// # Errors
    ///
    /// Rejects empty/oversized content and aggregate quota overflow.
    pub fn write(&self, content: impl Into<String>) -> Result<MemoryEntry, MemoryError> {
        let content = content.into();
        if content.trim().is_empty() {
            return Err(MemoryError::EmptyEntry);
        }
        if content.len() > MAX_MEMORY_ENTRY_BYTES {
            return Err(MemoryError::EntryTooLarge {
                bytes: content.len(),
                limit: MAX_MEMORY_ENTRY_BYTES,
            });
        }
        let mut connection = self.connect()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: i64 = transaction.query_row(
            "SELECT COALESCE(SUM(length(CAST(content AS BLOB))), 0) FROM memory_entries",
            [],
            |row| row.get(0),
        )?;
        let current = usize::try_from(current).unwrap_or(usize::MAX);
        let proposed = current.saturating_add(content.len());
        if proposed > MAX_PROJECT_MEMORY_BYTES {
            return Err(MemoryError::ProjectQuotaExceeded {
                bytes: proposed,
                limit: MAX_PROJECT_MEMORY_BYTES,
            });
        }
        transaction.execute(
            "INSERT INTO memory_entries(content) VALUES (?1)",
            params![content],
        )?;
        let id = transaction.last_insert_rowid();
        let content = transaction.query_row(
            "SELECT content FROM memory_entries WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        Ok(MemoryEntry { id, content })
    }

    /// Reads one entry by id.
    ///
    /// # Errors
    ///
    /// Returns storage failures; a missing id is represented as `None`.
    pub fn read(&self, id: i64) -> Result<Option<MemoryEntry>, MemoryError> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT id, content FROM memory_entries WHERE id = ?1",
                [id],
                |row| {
                    Ok(MemoryEntry {
                        id: row.get(0)?,
                        content: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(MemoryError::from)
    }

    /// Lists all entries in stable insertion order.
    ///
    /// # Errors
    ///
    /// Returns database read failures.
    pub fn list(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        let connection = self.connect()?;
        let mut statement =
            connection.prepare("SELECT id, content FROM memory_entries ORDER BY id ASC")?;
        statement
            .query_map([], |row| {
                Ok(MemoryEntry {
                    id: row.get(0)?,
                    content: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(MemoryError::from)
    }

    /// Clears every project-memory entry transactionally and returns the count.
    ///
    /// # Errors
    ///
    /// Returns database write failures.
    pub fn clear(&self) -> Result<usize, MemoryError> {
        let connection = self.connect()?;
        let removed = connection.execute("DELETE FROM memory_entries", [])?;
        connection.execute_batch("VACUUM")?;
        Ok(removed)
    }

    fn connect(&self) -> Result<Connection, MemoryError> {
        validate_database_file(&self.database)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = Connection::open_with_flags(&self.database, flags)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "secure_delete", "ON")?;
        Ok(connection)
    }
}

fn canonical_workspace(workspace: &Path) -> Result<PathBuf, MemoryError> {
    let workspace = fs::canonicalize(workspace).map_err(|source| MemoryError::Io {
        operation: "canonicalize memory workspace",
        path: workspace.to_path_buf(),
        source,
    })?;
    if !workspace.is_dir() {
        return Err(MemoryError::WorkspaceNotDirectory);
    }
    Ok(workspace)
}

fn canonical_storage_root(storage_root: &Path) -> Result<PathBuf, MemoryError> {
    validate_private_directory(storage_root)?;
    fs::canonicalize(storage_root).map_err(|source| MemoryError::Io {
        operation: "canonicalize project-memory storage root",
        path: storage_root.to_path_buf(),
        source,
    })
}

fn workspace_storage_key(workspace: &Path) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rottweiler-project-memory-v1\0");
    #[cfg(unix)]
    hasher.update(workspace.as_os_str().as_bytes());
    #[cfg(windows)]
    for unit in workspace.as_os_str().encode_wide() {
        hasher.update(&unit.to_le_bytes());
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(workspace.to_string_lossy().as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn validate_optional_private_directory(path: &Path) -> Result<bool, MemoryError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            validate_private_directory(path)?;
            Ok(true)
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(MemoryError::Io {
            operation: "inspect private memory directory",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn validate_private_directory(path: &Path) -> Result<(), MemoryError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| MemoryError::Io {
        operation: "inspect private memory directory",
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(MemoryError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o700 {
            return Err(MemoryError::UnsafePermissions {
                path: path.to_path_buf(),
                mode,
                required: 0o700,
            });
        }
    }
    Ok(())
}

fn ensure_directory(path: &Path, private: bool) -> Result<(), MemoryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(MemoryError::UnsafePath {
                    path: path.to_path_buf(),
                });
            }
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            if private {
                builder.mode(0o700);
            }
            builder.create(path).map_err(|source| MemoryError::Io {
                operation: "create memory directory",
                path: path.to_path_buf(),
                source,
            })?;
        }
        Err(source) => {
            return Err(MemoryError::Io {
                operation: "inspect memory directory",
                path: path.to_path_buf(),
                source,
            });
        }
    }
    if private {
        validate_private_directory(path)?;
    }
    Ok(())
}

fn prepare_database_file(path: &Path) -> Result<(), MemoryError> {
    match fs::symlink_metadata(path) {
        Ok(_) => validate_database_file(path),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            options
                .open(path)
                .and_then(|file| file.sync_all())
                .map_err(|source| MemoryError::Io {
                    operation: "create private memory database",
                    path: path.to_path_buf(),
                    source,
                })?;
            validate_database_file(path)
        }
        Err(source) => Err(MemoryError::Io {
            operation: "inspect private memory database",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn validate_database_file(path: &Path) -> Result<(), MemoryError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| MemoryError::Io {
        operation: "inspect private memory database",
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(MemoryError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(MemoryError::UnsafePermissions {
                path: path.to_path_buf(),
                mode,
                required: 0o600,
            });
        }
    }
    Ok(())
}

/// Safe project-memory failure.
#[derive(Debug, Error)]
pub enum MemoryError {
    /// Workspace root must be a directory.
    #[error("project-memory workspace is not a directory")]
    WorkspaceNotDirectory,
    /// Private state paths may not be symlinks or special files.
    #[error("project memory has an unsafe path: {path}")]
    UnsafePath { path: PathBuf },
    /// Private state permissions must not expose memory to other users.
    #[error("project memory path {path} has mode {mode:o}; required {required:o}")]
    UnsafePermissions {
        path: PathBuf,
        mode: u32,
        required: u32,
    },
    /// Empty/whitespace-only memories are not useful durable records.
    #[error("project memory entry must not be empty")]
    EmptyEntry,
    /// One entry exceeded its bound.
    #[error("project memory entry is {bytes} bytes; limit is {limit}")]
    EntryTooLarge { bytes: usize, limit: usize },
    /// Aggregate project memory exceeded its bound.
    #[error("project memory would be {bytes} bytes; limit is {limit}")]
    ProjectQuotaExceeded { bytes: usize, limit: usize },
    /// Future storage formats fail closed rather than being misread.
    #[error("unsupported project-memory schema version {version}")]
    UnsupportedSchema { version: i64 },
    /// Sanitized filesystem failure.
    #[error("failed to {operation} at {path}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Sanitized database failure.
    #[error("project-memory database operation failed")]
    Database(#[from] rusqlite::Error),
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    use tempfile::{TempDir, tempdir};

    use super::{MEMORY_STORAGE_DIRECTORY, MemoryError, ProjectMemoryStore, workspace_storage_key};

    fn storage_dir() -> TempDir {
        let directory = tempdir().unwrap_or_else(|error| panic!("storage: {error}"));
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("storage mode: {error}"));
        directory
    }

    #[test]
    fn write_read_list_reopen_and_clear_are_project_local() {
        let first = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let second = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let storage = storage_dir();
        let store = ProjectMemoryStore::open_in(storage.path(), first.path())
            .unwrap_or_else(|error| panic!("memory opens: {error}"));
        let one = store
            .write("Prefer focused tests")
            .unwrap_or_else(|error| panic!("memory writes: {error}"));
        let two = store
            .write("The API is provider-neutral")
            .unwrap_or_else(|error| panic!("memory writes: {error}"));
        assert_eq!(
            store
                .read(one.id)
                .unwrap_or_else(|error| panic!("memory reads: {error}")),
            Some(one.clone())
        );
        assert_eq!(
            store
                .list()
                .unwrap_or_else(|error| panic!("memory lists: {error}")),
            vec![one, two]
        );

        let reopened = ProjectMemoryStore::open_in(storage.path(), first.path())
            .unwrap_or_else(|error| panic!("memory reopens: {error}"));
        assert_eq!(
            reopened
                .list()
                .unwrap_or_else(|error| panic!("memory lists: {error}"))
                .len(),
            2
        );
        let other = ProjectMemoryStore::open_in(storage.path(), second.path())
            .unwrap_or_else(|error| panic!("other memory opens: {error}"));
        assert!(
            other
                .list()
                .unwrap_or_else(|error| panic!("other memory lists: {error}"))
                .is_empty()
        );
        assert_eq!(
            reopened
                .clear()
                .unwrap_or_else(|error| panic!("memory clears: {error}")),
            2
        );
        assert!(
            reopened
                .list()
                .unwrap_or_else(|error| panic!("memory lists: {error}"))
                .is_empty()
        );
    }

    #[test]
    fn empty_entries_are_rejected() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let storage = storage_dir();
        let store = ProjectMemoryStore::open_in(storage.path(), root.path())
            .unwrap_or_else(|error| panic!("memory opens: {error}"));
        assert!(matches!(store.write("  \n"), Err(MemoryError::EmptyEntry)));
    }

    #[test]
    fn clear_removes_content_from_the_database_file() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let storage = storage_dir();
        let store = ProjectMemoryStore::open_in(storage.path(), root.path())
            .unwrap_or_else(|error| panic!("memory opens: {error}"));
        let canary = "private-memory-clear-canary-4f137b";
        store
            .write(canary)
            .unwrap_or_else(|error| panic!("memory writes: {error}"));
        assert_eq!(
            store
                .clear()
                .unwrap_or_else(|error| panic!("memory clears: {error}")),
            1
        );
        let database = fs::read(store.database_path())
            .unwrap_or_else(|error| panic!("database reads: {error}"));
        assert!(
            !database
                .windows(canary.len())
                .any(|window| window == canary.as_bytes())
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_state_has_restrictive_permissions() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let storage = storage_dir();
        let store = ProjectMemoryStore::open_in(storage.path(), root.path())
            .unwrap_or_else(|error| panic!("memory opens: {error}"));
        let directory = store
            .database_path()
            .parent()
            .unwrap_or_else(|| panic!("database has parent"));
        assert_eq!(
            fs::metadata(directory)
                .unwrap_or_else(|error| panic!("private directory metadata: {error}"))
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(store.database_path())
                .unwrap_or_else(|error| panic!("database metadata: {error}"))
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_private_directory_is_rejected() {
        let workspace = tempdir().unwrap_or_else(|error| panic!("workspace: {error}"));
        let storage = storage_dir();
        let outside = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        std::os::unix::fs::symlink(
            outside.path(),
            storage.path().join(MEMORY_STORAGE_DIRECTORY),
        )
        .unwrap_or_else(|error| panic!("symlink: {error}"));
        assert!(matches!(
            ProjectMemoryStore::open_in(storage.path(), workspace.path()),
            Err(MemoryError::UnsafePath { .. })
        ));
    }

    #[test]
    fn concurrent_writers_are_serialized_without_lost_entries() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let storage = storage_dir();
        let store = ProjectMemoryStore::open_in(storage.path(), root.path())
            .unwrap_or_else(|error| panic!("memory opens: {error}"));
        let threads = (0..8)
            .map(|index| {
                let store = store.clone();
                std::thread::spawn(move || {
                    store
                        .write(format!("entry {index}"))
                        .unwrap_or_else(|error| panic!("concurrent write: {error}"));
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread
                .join()
                .unwrap_or_else(|_| panic!("writer thread joins"));
        }
        assert_eq!(
            store
                .list()
                .unwrap_or_else(|error| panic!("memory lists: {error}"))
                .len(),
            8
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_database_is_rejected() {
        let workspace = tempdir().unwrap_or_else(|error| panic!("workspace: {error}"));
        let storage = storage_dir();
        let outside = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let memory_root = storage.path().join(MEMORY_STORAGE_DIRECTORY);
        fs::create_dir(&memory_root).unwrap_or_else(|error| panic!("memory root: {error}"));
        fs::set_permissions(&memory_root, fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("memory root mode: {error}"));
        let workspace = fs::canonicalize(workspace.path())
            .unwrap_or_else(|error| panic!("canonical workspace: {error}"));
        let private = memory_root.join(workspace_storage_key(&workspace));
        fs::create_dir(&private).unwrap_or_else(|error| panic!("project memory dir: {error}"));
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("project memory mode: {error}"));
        let outside_file = outside.path().join("memory.sqlite3");
        fs::write(&outside_file, "outside").unwrap_or_else(|error| panic!("outside file: {error}"));
        std::os::unix::fs::symlink(&outside_file, private.join("memory.sqlite3"))
            .unwrap_or_else(|error| panic!("database symlink: {error}"));
        assert!(matches!(
            ProjectMemoryStore::open_in(storage.path(), &workspace),
            Err(MemoryError::UnsafePath { .. })
        ));
    }

    #[test]
    fn host_storage_is_collision_resistant_and_workspace_stays_clean() {
        let storage = storage_dir();
        let first_parent = tempdir().unwrap_or_else(|error| panic!("parent: {error}"));
        let second_parent = tempdir().unwrap_or_else(|error| panic!("parent: {error}"));
        let first = first_parent.path().join("same-name");
        let second = second_parent.path().join("same-name");
        fs::create_dir(&first).unwrap_or_else(|error| panic!("first workspace: {error}"));
        fs::create_dir(&second).unwrap_or_else(|error| panic!("second workspace: {error}"));

        let first_store = ProjectMemoryStore::open_in(storage.path(), &first)
            .unwrap_or_else(|error| panic!("first memory: {error}"));
        let second_store = ProjectMemoryStore::open_in(storage.path(), &second)
            .unwrap_or_else(|error| panic!("second memory: {error}"));
        assert_ne!(first_store.database_path(), second_store.database_path());
        assert_eq!(
            first_store
                .database_path()
                .parent()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().len()),
            Some(64)
        );
        first_store
            .write("first only")
            .unwrap_or_else(|error| panic!("first write: {error}"));
        assert!(
            second_store
                .list()
                .unwrap_or_else(|error| panic!("second list: {error}"))
                .is_empty()
        );
        assert!(!first.join(".rottweiler").exists());
        assert!(!second.join(".rottweiler").exists());
    }

    #[test]
    fn opening_absent_memory_for_context_is_read_only() {
        let storage = storage_dir();
        let workspace = tempdir().unwrap_or_else(|error| panic!("workspace: {error}"));
        assert!(
            ProjectMemoryStore::open_existing_in(storage.path(), workspace.path())
                .unwrap_or_else(|error| panic!("existing lookup: {error}"))
                .is_none()
        );
        assert!(!storage.path().join(MEMORY_STORAGE_DIRECTORY).exists());
        assert!(!workspace.path().join(".rottweiler").exists());
    }
}
