//! Atomic publication of the first quota ledger under the workspace writer lock.
use super::{CheckpointBlobStore, CheckpointError, Connection, File, Path, fs};
use crate::{checkpoint::CheckpointOperation, session::AdvisoryFileLock};
use rusqlite::OpenFlags;
use std::{io, time::Duration};

impl CheckpointBlobStore {
    pub(super) fn open_ledger(&self) -> Result<Connection, CheckpointError> {
        let path = self.root.join("quota.sqlite");
        if !path.exists() {
            if fs::read_dir(self.directory())?.next().is_some() {
                return Err(CheckpointError::CorruptBlobQuota);
            }
            self.initialize_ledger(&path)?;
        }
        let connection = configured(&path)?;
        self.validate_ledger(&connection)?;
        connection.execute_batch("CREATE TEMP TABLE protected(digest TEXT PRIMARY KEY);")?;
        Ok(connection)
    }

    // A missing directory is empty only at an authoritative unregistered prefix.
    // Existing writers settle before this lookup; no read-side file is created.
    pub(in crate::checkpoint) fn read_namespace_directory(
        &self,
        namespace: &Path,
        kind: &str,
        operation: &mut CheckpointOperation,
    ) -> Result<Option<fs::ReadDir>, CheckpointError> {
        match super::validate_namespace_directories(namespace) {
            Ok(()) => return Ok(Some(fs::read_dir(namespace.join(kind))?)),
            Err(CheckpointError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        loop {
            let lock = self.existing_read_lock(operation)?;
            let Some(registered) = self.namespace_registered(namespace, lock.is_some())? else {
                // A first publisher installed its lock and ledger after the
                // absent-lock observation. Acquire that owner before querying.
                continue;
            };
            if !registered {
                return Ok(None);
            }
            // The first publisher may have installed these after the initial read.
            super::validate_namespace_directories(namespace)?;
            return Ok(Some(fs::read_dir(namespace.join(kind))?));
        }
    }

    fn existing_read_lock(
        &self,
        operation: &mut CheckpointOperation,
    ) -> Result<Option<AdvisoryFileLock>, CheckpointError> {
        self.existing_read_lock_using(operation, regular_read)
    }

    pub(super) fn existing_read_lock_using(
        &self,
        operation: &mut CheckpointOperation,
        mut read: impl FnMut(&Path) -> Result<File, CheckpointError>,
    ) -> Result<Option<AdvisoryFileLock>, CheckpointError> {
        let path = self.root.join("writer.lock");
        loop {
            operation.check()?;
            let file = match read(&path) {
                Ok(file) => file,
                Err(CheckpointError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                    if !self.root.join("quota.sqlite").try_exists()? {
                        return Ok(None);
                    }
                    // Registration creates the lock before publishing its ledger.
                    // Reopen after observing that ledger to distinguish a first
                    // publisher from deletion of an established authority file.
                    match read(&path) {
                        Ok(file) => file,
                        Err(CheckpointError::Io(error))
                            if error.kind() == io::ErrorKind::NotFound =>
                        {
                            return Err(CheckpointError::CorruptBlobQuota);
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            };
            let before = file.metadata()?;
            match AdvisoryFileLock::try_shared(file) {
                Ok(lock) => {
                    if !crate::checkpoint::same_open_file_identity(
                        &before,
                        &fs::symlink_metadata(&path)?,
                    ) {
                        return Err(CheckpointError::CorruptBlobQuota);
                    }
                    return Ok(Some(lock));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn namespace_registered(
        &self,
        namespace: &Path,
        locked: bool,
    ) -> Result<Option<bool>, CheckpointError> {
        let path = self.root.join("quota.sqlite");
        let file = match regular_read(&path) {
            Ok(file) => file,
            Err(CheckpointError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                match fs::read_dir(self.directory()) {
                    Ok(mut entries) => {
                        if entries.next().transpose()?.is_some() {
                            return Err(CheckpointError::CorruptBlobQuota);
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                return Ok(Some(false));
            }
            Err(error) => return Err(error),
        };
        if !locked {
            return Ok(None);
        }
        if file.metadata()?.len() > 64 * 1024 * 1024 {
            return Err(CheckpointError::CorruptBlobQuota);
        }
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::ZERO)?;
        connection
            .execute_batch("PRAGMA query_only=ON; PRAGMA cache_size=-256; PRAGMA mmap_size=0;")?;
        self.validate_ledger(&connection)?;
        if !crate::checkpoint::same_open_file_identity(
            &file.metadata()?,
            &fs::symlink_metadata(&path)?,
        ) {
            return Err(CheckpointError::CorruptBlobQuota);
        }
        let namespace = namespace
            .to_str()
            .filter(|path| path.len() <= 4096)
            .ok_or(CheckpointError::CorruptBlobQuota)?;
        Ok(Some(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM namespaces WHERE path=?1)",
            [namespace],
            |row| row.get(0),
        )?))
    }

    fn validate_ledger(&self, connection: &Connection) -> Result<(), CheckpointError> {
        let identity: bool = connection.query_row(
            "SELECT version=1 AND lineage=?1 FROM quota WHERE id=1",
            [&self.lineage],
            |row| row.get(0),
        )?;
        let page_size: u32 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        if !identity || page_size != 4096 {
            return Err(CheckpointError::CorruptBlobQuota);
        }
        Ok(())
    }

    fn initialize_ledger(&self, path: &Path) -> Result<(), CheckpointError> {
        let temporary = self.root.join("quota-initialize.sqlite");
        // No blob can have been admitted until the complete ledger was published.
        // Only these fixed initializer files are removable at this crash cut.
        for name in ["quota-initialize.sqlite", "quota-initialize.sqlite-journal"] {
            let abandoned = self.root.join(name);
            match fs::symlink_metadata(&abandoned) {
                Ok(metadata) if metadata.is_file() => fs::remove_file(&abandoned)?,
                Ok(_) => return Err(CheckpointError::CorruptBlobQuota),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let connection = configured(&temporary)?;
        connection.execute_batch("BEGIN IMMEDIATE;
            CREATE TABLE quota(id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL,
                lineage TEXT NOT NULL, dirty INTEGER NOT NULL, staged INTEGER NOT NULL,
                used_bytes INTEGER NOT NULL, blob_count INTEGER NOT NULL);
            CREATE TABLE blobs(digest TEXT PRIMARY KEY, bytes INTEGER NOT NULL CHECK(bytes>=0));
            CREATE TABLE namespaces(path TEXT PRIMARY KEY);
            CREATE TRIGGER blob_added AFTER INSERT ON blobs BEGIN
                UPDATE quota SET used_bytes=used_bytes+new.bytes,blob_count=blob_count+1 WHERE id=1; END;
            CREATE TRIGGER blob_removed AFTER DELETE ON blobs BEGIN
                UPDATE quota SET used_bytes=used_bytes-old.bytes,blob_count=blob_count-1 WHERE id=1; END;")?;
        connection.execute("INSERT INTO quota VALUES(1,1,?1,1,0,0,0)", [&self.lineage])?;
        connection.execute_batch("COMMIT")?;
        connection.close().map_err(|(_, error)| error)?;
        File::open(&temporary)?.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
}

fn configured(path: &Path) -> Result<Connection, CheckpointError> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::ZERO)?;
    connection.execute_batch(
        "PRAGMA page_size=4096; PRAGMA journal_mode=DELETE;
        PRAGMA synchronous=FULL; PRAGMA cache_size=-256; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;
        PRAGMA max_page_count=16384; PRAGMA temp.max_page_count=16384;",
    )?;
    let page_size: u32 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    if page_size != 4096 {
        return Err(CheckpointError::CorruptBlobQuota);
    }
    Ok(connection)
}

fn regular_read(path: &Path) -> Result<File, CheckpointError> {
    #[cfg(unix)]
    let file = File::from(
        rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(io::Error::from)?,
    );
    #[cfg(not(unix))]
    let file = File::open(path)?;
    if !file.metadata()?.is_file() || !fs::symlink_metadata(path)?.is_file() {
        return Err(CheckpointError::CorruptBlobQuota);
    }
    Ok(file)
}
