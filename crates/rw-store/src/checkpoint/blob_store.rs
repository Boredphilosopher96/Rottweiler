//! One workspace's shared, crash-reconciled checkpoint byte authority.

use super::{CheckpointError, CheckpointOperation, MAX_CAPTURE_FILE_BYTES};
use crate::session::AdvisoryFileLock;
use rusqlite::{Connection, OptionalExtension};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

mod ledger;
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
        let lock = self.lock_references(operation)?;
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
    // Reference publications use the same exclusion as GC, without opening the
    // quota ledger or changing blob-accounting state.
    pub(super) fn lock_references(
        &self,
        operation: &mut CheckpointOperation,
    ) -> Result<AdvisoryFileLock, CheckpointError> {
        self.validate_workspace(&self.workspace)?;
        super::create_directory_durable(&self.directory())?;
        super::create_directory_durable(&self.root.join("staging"))?;
        loop {
            operation.check()?;
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(self.root.join("writer.lock"))?;
            match AdvisoryFileLock::try_exclusive(file) {
                Ok(lock) => return Ok(lock),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

pub(super) struct BlobWriteGuard<'a> {
    owner: &'a CheckpointBlobStore,
    connection: Connection,
    _lock: AdvisoryFileLock,
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
        let count: i64 =
            self.connection
                .query_row("SELECT count(*) FROM namespaces", [], |row| row.get(0))?;
        if !exists && nonnegative(count)? >= MAX_NAMESPACES {
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

fn nonnegative(value: i64) -> Result<u64, CheckpointError> {
    u64::try_from(value).map_err(|_| CheckpointError::CorruptBlobQuota)
}
fn sql_integer(value: u64) -> Result<i64, CheckpointError> {
    i64::try_from(value).map_err(|_| CheckpointError::CorruptBlobQuota)
}
