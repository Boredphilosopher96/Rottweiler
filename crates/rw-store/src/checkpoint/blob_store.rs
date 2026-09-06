//! One workspace's shared, crash-reconciled checkpoint byte authority.

use super::{CheckpointError, CheckpointOperation, MAX_CAPTURE_FILE_BYTES};
use crate::session::ExclusiveFileLock;
use rusqlite::{Connection, OptionalExtension};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

mod reconcile;
#[cfg(test)]
mod tests;
mod write;

const RETAINED_BYTES: u64 = 960 * 1024 * 1024;
const MAX_BLOBS: u64 = 65_536;
const MAX_NAMESPACES: u64 = 1_024;

/// Shared blob storage for one physical workspace. Opening is metadata-only;
/// the first capture acquires the durable quota authority.
#[derive(Debug)]
pub struct CheckpointBlobStore {
    root: PathBuf,
    storage: PathBuf,
    workspace: PathBuf,
    lineage: String,
    retained_bytes: u64,
}

impl CheckpointBlobStore {
    /// Binds all session checkpoint namespaces to this workspace's shared quota.
    ///
    /// # Errors
    /// Returns an error when the workspace cannot be resolved to a directory.
    pub fn open(storage_root: &Path, workspace: &Path) -> Result<Arc<Self>, CheckpointError> {
        let workspace = fs::canonicalize(workspace)?;
        let lineage = workspace_identity(&workspace)?;
        let storage = std::path::absolute(storage_root)?.join("checkpoint-blobs");
        Ok(Arc::new(Self {
            root: storage.join(&lineage),
            storage,
            workspace,
            lineage,
            retained_bytes: RETAINED_BYTES,
        }))
    }

    pub(super) fn validate_workspace(&self, workspace: &Path) -> Result<(), CheckpointError> {
        if workspace_identity(workspace)? != self.lineage {
            return Err(CheckpointError::BlobWorkspaceMismatch);
        }
        Ok(())
    }

    pub(super) fn storage_path(&self) -> &Path {
        &self.storage
    }

    pub(super) fn directory(&self) -> PathBuf {
        self.root.join("blobs")
    }

    pub(super) fn begin<'a>(
        &'a self,
        namespace: &Path,
        operation: &mut CheckpointOperation,
    ) -> Result<BlobWriteGuard<'a>, CheckpointError> {
        self.validate_workspace(&self.workspace)?;
        fs::create_dir_all(self.directory())?;
        fs::create_dir_all(self.root.join("staging"))?;
        let lock = loop {
            operation.check()?;
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(self.root.join("writer.lock"))?;
            match ExclusiveFileLock::try_acquire(file) {
                Ok(lock) => break lock,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => return Err(error.into()),
            }
        };
        let connection = self.open_ledger()?;
        let mut guard = BlobWriteGuard {
            owner: self,
            connection,
            _lock: lock,
        };
        let dirty: bool =
            guard
                .connection
                .query_row("SELECT dirty FROM quota WHERE id=1", [], |row| row.get(0))?;
        guard
            .connection
            .execute("UPDATE quota SET dirty=1 WHERE id=1", [])?;
        guard.register(namespace)?;
        if dirty {
            guard.clean_unpublished(operation)?;
            guard.reconcile(operation, true)?;
        }
        Ok(guard)
    }

    fn open_ledger(&self) -> Result<Connection, CheckpointError> {
        let path = self.root.join("quota.sqlite");
        let fresh = !path.exists();
        if fresh && fs::read_dir(self.directory())?.next().is_some() {
            return Err(CheckpointError::CorruptBlobQuota);
        }
        let connection = Connection::open(path)?;
        connection.execute_batch("PRAGMA page_size=4096; PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
            PRAGMA cache_size=-256; PRAGMA temp_store=FILE; PRAGMA max_page_count=16384; PRAGMA temp.max_page_count=16384;")?;
        let page_size: u32 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        if page_size != 4096 {
            return Err(CheckpointError::CorruptBlobQuota);
        }
        if fresh {
            connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE quota(id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL,
                    lineage TEXT NOT NULL, dirty INTEGER NOT NULL, staged INTEGER NOT NULL, used_bytes INTEGER NOT NULL, blob_count INTEGER NOT NULL);
                CREATE TABLE blobs(digest TEXT PRIMARY KEY, bytes INTEGER NOT NULL CHECK(bytes>=0));
                CREATE TABLE namespaces(path TEXT PRIMARY KEY);
                CREATE TRIGGER blob_added AFTER INSERT ON blobs BEGIN UPDATE quota SET used_bytes=used_bytes+new.bytes,blob_count=blob_count+1 WHERE id=1; END;
                CREATE TRIGGER blob_removed AFTER DELETE ON blobs BEGIN UPDATE quota SET used_bytes=used_bytes-old.bytes,blob_count=blob_count-1 WHERE id=1; END;")?;
            connection.execute("INSERT INTO quota VALUES(1,1,?1,1,0,0,0)", [&self.lineage])?;
            connection.execute_batch("COMMIT")?;
            File::open(&self.root)?.sync_all()?;
        }
        let identity: (u32, String) =
            connection.query_row("SELECT version,lineage FROM quota WHERE id=1", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        if identity != (1, self.lineage.clone()) {
            return Err(CheckpointError::CorruptBlobQuota);
        }
        connection.execute_batch("CREATE TEMP TABLE protected(digest TEXT PRIMARY KEY);")?;
        Ok(connection)
    }
}

pub(super) struct BlobWriteGuard<'a> {
    owner: &'a CheckpointBlobStore,
    connection: Connection,
    _lock: ExclusiveFileLock,
}

impl BlobWriteGuard<'_> {
    fn register(&self, namespace: &Path) -> Result<(), CheckpointError> {
        let namespace = fs::canonicalize(namespace)?;
        let path = namespace
            .to_str()
            .filter(|path| path.len() <= 4096)
            .ok_or(CheckpointError::CorruptBlobQuota)?;
        let exists = self
            .connection
            .query_row("SELECT 1 FROM namespaces WHERE path=?1", [path], |_| Ok(()))
            .optional()?
            .is_some();
        let count: u64 =
            self.connection
                .query_row("SELECT count(*) FROM namespaces", [], |row| row.get(0))?;
        if !exists && count >= MAX_NAMESPACES {
            return Err(CheckpointError::BlobQuotaExceeded);
        }
        self.connection
            .execute("INSERT OR IGNORE INTO namespaces VALUES(?1)", [path])?;
        Ok(())
    }

    pub(super) fn finish(self) -> Result<(), CheckpointError> {
        self.connection
            .execute("UPDATE quota SET dirty=0,staged=0 WHERE id=1", [])?;
        Ok(())
    }
}

#[cfg(unix)]
fn workspace_identity(workspace: &Path) -> Result<String, CheckpointError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(workspace)?;
    if !metadata.is_dir() {
        return Err(CheckpointError::WorkspaceNotDirectory);
    }
    Ok(
        blake3::hash(format!("{}:{}", metadata.dev(), metadata.ino()).as_bytes())
            .to_hex()
            .to_string(),
    )
}

#[cfg(not(unix))]
fn workspace_identity(workspace: &Path) -> Result<String, CheckpointError> {
    if !workspace.is_dir() {
        return Err(CheckpointError::WorkspaceNotDirectory);
    }
    let path = workspace.to_str().ok_or(CheckpointError::UnsafePath)?;
    Ok(blake3::hash(path.as_bytes()).to_hex().to_string())
}
