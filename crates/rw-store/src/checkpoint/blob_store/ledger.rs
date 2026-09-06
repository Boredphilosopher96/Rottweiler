//! Atomic publication of the first quota ledger under the workspace writer lock.
use super::{CheckpointBlobStore, CheckpointError, Connection, File, Path, fs};

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
    connection.execute_batch(
        "PRAGMA page_size=4096; PRAGMA journal_mode=DELETE;
        PRAGMA synchronous=FULL; PRAGMA cache_size=-256; PRAGMA temp_store=FILE;
        PRAGMA max_page_count=16384; PRAGMA temp.max_page_count=16384;",
    )?;
    let page_size: u32 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    if page_size != 4096 {
        return Err(CheckpointError::CorruptBlobQuota);
    }
    Ok(connection)
}
