//! Reference validation completes before any unreferenced blob is removed.
use super::{BlobWriteGuard, CheckpointError, CheckpointOperation, MAX_BLOBS};
use crate::checkpoint::{
    CheckpointFileState, CheckpointManifest, CheckpointStore, MAX_CAPTURE_FILE_BYTES,
    REWIND_TRANSACTION_VERSION, RewindPhase, RewindTransaction, is_lower_blake3,
    is_private_temporary, normalize_relative, parse_exact_turn_filename, validate_operation_id,
    validate_rewind_report, validate_session_id,
};
use rusqlite::OptionalExtension;
use std::{
    fs::{self, File},
    io::Read,
    path::Path,
};

impl BlobWriteGuard<'_> {
    pub(super) fn clean_unpublished(
        &self,
        operation: &mut CheckpointOperation,
    ) -> Result<(), CheckpointError> {
        let mut statement = self.connection.prepare("SELECT path FROM namespaces")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let namespace: String = row.get(0)?;
            for kind in ["manifests", "pending"] {
                let root = Path::new(&namespace).join(kind);
                directory(&root)?;
                cleanup_unpublished(&root, operation)?;
                for entry in fs::read_dir(root)? {
                    let entry = entry?;
                    operation.path(&entry.path().to_string_lossy())?;
                    directory(&entry.path())?;
                    cleanup_unpublished(&entry.path(), operation)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn reconcile(
        &mut self,
        operation: &mut CheckpointOperation,
        abandoned: bool,
    ) -> Result<(), CheckpointError> {
        self.connection.execute_batch(
            "DROP TABLE IF EXISTS temp.live;
            DROP TABLE IF EXISTS temp.observed;
            CREATE TEMP TABLE live(digest TEXT PRIMARY KEY,bytes INTEGER NOT NULL);
            CREATE TEMP TABLE observed(digest TEXT PRIMARY KEY,bytes INTEGER NOT NULL);",
        )?;
        self.collect_references(operation)?;
        self.observe_blobs(operation)?;
        // A missing referenced file invalidates the inventory, not its reference.
        let missing: bool = self.connection.query_row(
            "SELECT EXISTS(
            SELECT 1 FROM live LEFT JOIN observed USING(digest)
            WHERE observed.digest IS NULL OR live.bytes!=observed.bytes)",
            [],
            |row| row.get(0),
        )?;
        if missing {
            return Err(CheckpointError::CorruptBlob);
        }
        if abandoned {
            self.clear_staging(operation)?;
        }
        self.remove_unreferenced(operation)?;
        self.connection.execute_batch(
            "BEGIN IMMEDIATE; DELETE FROM blobs;
            INSERT INTO blobs SELECT * FROM observed;
            COMMIT;",
        )?;
        if abandoned {
            self.connection
                .execute("UPDATE quota SET staged=0 WHERE id=1", [])?;
        }
        Ok(())
    }

    fn collect_references(
        &self,
        operation: &mut CheckpointOperation,
    ) -> Result<(), CheckpointError> {
        let mut statement = self
            .connection
            .prepare("SELECT path FROM namespaces ORDER BY path")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let namespace: String = row.get(0)?;
            let namespace = Path::new(&namespace);
            directory(namespace)?;
            let manifests = namespace.join("manifests");
            directory(&manifests)?;
            for session in fs::read_dir(manifests)? {
                let session = session?;
                directory(&session.path())?;
                for entry in fs::read_dir(session.path())? {
                    let entry = entry?;
                    regular(&entry.path())?;
                    operation.path(&entry.path().to_string_lossy())?;
                    let manifest: CheckpointManifest =
                        serde_json::from_slice(&operation.read_metadata(&entry.path())?)?;
                    let turn = parse_exact_turn_filename(&entry.file_name())
                        .ok_or(CheckpointError::CorruptManifest)?;
                    let name = session.file_name();
                    let name = name.to_str().ok_or(CheckpointError::CorruptManifest)?;
                    // A durable fork staging directory also protects its references.
                    let identity = if is_private_temporary(std::ffi::OsStr::new(name)) {
                        &manifest.session_id
                    } else {
                        name
                    };
                    CheckpointStore::validate_manifest(&manifest, identity, turn)?;
                    for state in manifest.files.values() {
                        self.reference(state, operation)?;
                    }
                }
            }
            self.collect_rewinds(namespace, operation)?;
        }
        Ok(())
    }

    fn collect_rewinds(
        &self,
        namespace: &Path,
        operation: &mut CheckpointOperation,
    ) -> Result<(), CheckpointError> {
        let rewinds = namespace.join("rewinds");
        directory(&rewinds)?;
        for entry in fs::read_dir(rewinds)? {
            let entry = entry?;
            if is_private_temporary(&entry.file_name()) {
                continue;
            }
            regular(&entry.path())?;
            operation.path(&entry.path().to_string_lossy())?;
            let transaction: RewindTransaction =
                serde_json::from_slice(&operation.read_metadata(&entry.path())?)?;
            let name = entry.file_name();
            if name.to_str().and_then(|name| name.strip_suffix(".json"))
                != Some(transaction.handle.session_id.as_str())
                || transaction.version != REWIND_TRANSACTION_VERSION
                || transaction.next_step > transaction.steps.len()
                || (transaction.phase == RewindPhase::WorkspaceCommitted
                    && transaction.next_step != transaction.steps.len())
            {
                return Err(CheckpointError::CorruptRewindTransaction);
            }
            validate_session_id(&transaction.handle.session_id)?;
            validate_operation_id(&transaction.handle.operation_id)?;
            validate_rewind_report(&transaction.report)?;
            for step in &transaction.steps {
                if normalize_relative(Path::new(&step.path))? != step.path {
                    return Err(CheckpointError::CorruptRewindTransaction);
                }
                self.reference(&step.state, operation)?;
            }
        }
        Ok(())
    }

    fn reference(
        &self,
        state: &CheckpointFileState,
        operation: &mut CheckpointOperation,
    ) -> Result<(), CheckpointError> {
        operation.path("checkpoint reference")?;
        if let CheckpointFileState::Present {
            blob,
            bytes,
            unix_mode,
        } = state
        {
            if !is_lower_blake3(blob)
                || *bytes > MAX_CAPTURE_FILE_BYTES
                || unix_mode.is_some_and(|mode| mode > 0o7777)
            {
                return Err(CheckpointError::CorruptBlob);
            }
            let old: Option<i64> = self
                .connection
                .query_row("SELECT bytes FROM live WHERE digest=?1", [blob], |row| {
                    row.get(0)
                })
                .optional()?;
            if old
                .map(super::nonnegative)
                .transpose()?
                .is_some_and(|old| old != *bytes)
            {
                return Err(CheckpointError::CorruptBlob);
            }
            self.connection.execute(
                "INSERT OR IGNORE INTO live VALUES(?1,?2)",
                rusqlite::params![blob, super::sql_integer(*bytes)?],
            )?;
        }
        Ok(())
    }

    fn observe_blobs(&self, operation: &mut CheckpointOperation) -> Result<(), CheckpointError> {
        let mut count = 0_u64;
        let mut total = 0_u64;
        for prefix in fs::read_dir(self.owner.directory())? {
            let prefix = prefix?;
            directory(&prefix.path())?;
            let name = prefix.file_name();
            let name = name.to_str().ok_or(CheckpointError::CorruptBlob)?;
            if name.len() != 2
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(CheckpointError::CorruptBlob);
            }
            for entry in fs::read_dir(prefix.path())? {
                let entry = entry?;
                regular(&entry.path())?;
                let digest = entry.file_name();
                let digest = digest.to_str().ok_or(CheckpointError::CorruptBlob)?;
                let bytes = entry.metadata()?.len();
                if !is_lower_blake3(digest)
                    || !digest.starts_with(name)
                    || bytes > MAX_CAPTURE_FILE_BYTES
                {
                    return Err(CheckpointError::CorruptBlob);
                }
                operation.path(digest)?;
                count += 1;
                total = total
                    .checked_add(bytes)
                    .ok_or(CheckpointError::CorruptBlobQuota)?;
                if count > MAX_BLOBS || total > self.owner.retained_bytes {
                    return Err(CheckpointError::CorruptBlobQuota);
                }
                if operation
                    .hash(File::open(entry.path())?.take(bytes + 1))?
                    .to_hex()
                    .as_str()
                    != digest
                {
                    return Err(CheckpointError::CorruptBlob);
                }
                self.connection.execute(
                    "INSERT INTO observed VALUES(?1,?2)",
                    rusqlite::params![digest, super::sql_integer(bytes)?],
                )?;
            }
        }
        Ok(())
    }

    fn remove_unreferenced(
        &self,
        operation: &mut CheckpointOperation,
    ) -> Result<(), CheckpointError> {
        let mut statement = self.connection.prepare("SELECT digest FROM observed
            WHERE digest NOT IN (SELECT digest FROM live) AND digest NOT IN (SELECT digest FROM protected)")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            operation.check()?;
            let digest: String = row.get(0)?;
            let directory = self.owner.directory().join(&digest[..2]);
            fs::remove_file(directory.join(&digest))?;
            File::open(&directory)?.sync_all()?;
        }
        drop(rows);
        drop(statement);
        self.connection.execute(
            "DELETE FROM observed WHERE digest NOT IN (SELECT digest FROM live)
            AND digest NOT IN (SELECT digest FROM protected)",
            [],
        )?;
        Ok(())
    }

    fn clear_staging(&self, operation: &mut CheckpointOperation) -> Result<(), CheckpointError> {
        let directory = self.owner.root.join("staging");
        // Validate the complete directory before removing abandoned owner files.
        let mut count = 0;
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            count += 1;
            if count > 1 {
                return Err(CheckpointError::CorruptBlobQuota);
            }
            regular(&entry.path())?;
            operation.path(&entry.path().to_string_lossy())?;
            if !entry.file_name().to_string_lossy().starts_with("capture-")
                || entry.metadata()?.len() > MAX_CAPTURE_FILE_BYTES
            {
                return Err(CheckpointError::CorruptBlobQuota);
            }
        }
        for entry in fs::read_dir(&directory)? {
            fs::remove_file(entry?.path())?;
        }
        File::open(directory)?.sync_all()?;
        Ok(())
    }
}

fn directory(path: &Path) -> Result<(), CheckpointError> {
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(CheckpointError::CorruptBlobQuota);
    }
    Ok(())
}
fn regular(path: &Path) -> Result<(), CheckpointError> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(CheckpointError::CorruptBlobQuota);
    }
    Ok(())
}

// At most one directory level belongs to a fork's unpublished manifest namespace.
// Validate the bounded contents before removing that namespace.
fn cleanup_unpublished(
    root: &Path,
    operation: &mut CheckpointOperation,
) -> Result<(), CheckpointError> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        operation.path(&entry.path().to_string_lossy())?;
        if !is_private_temporary(&entry.file_name()) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_file() {
            fs::remove_file(entry.path())?;
        } else if metadata.is_dir() {
            for child in fs::read_dir(entry.path())? {
                let child = child?;
                operation.path(&child.path().to_string_lossy())?;
                regular(&child.path())?;
                if parse_exact_turn_filename(&child.file_name()).is_none()
                    && !is_private_temporary(&child.file_name())
                {
                    return Err(CheckpointError::CorruptManifest);
                }
            }
            for child in fs::read_dir(entry.path())? {
                fs::remove_file(child?.path())?;
            }
            fs::remove_dir(entry.path())?;
        } else {
            return Err(CheckpointError::CorruptBlobQuota);
        }
        File::open(root)?.sync_all()?;
    }
    Ok(())
}
