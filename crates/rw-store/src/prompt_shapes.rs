//! Indexed authoritative request shapes. Conversation bodies never enter this store.
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::{fs, path::Path, time::Duration};

pub const MAX_PROFILE_BYTES: usize = 4 * 1024 * 1024;
const APPLICATION_ID: u32 = 0x5257_5053;
const PROFILES: &str = "CREATE TABLE profiles (id BLOB PRIMARY KEY NOT NULL CHECK(length(id)=32), body BLOB NOT NULL CHECK(length(body)<=4194304)) STRICT";
const REQUESTS: &str = "CREATE TABLE requests (source BLOB PRIMARY KEY NOT NULL CHECK(length(source)=8), turn BLOB NOT NULL CHECK(length(turn)=8), profile BLOB NOT NULL REFERENCES profiles(id), fingerprint BLOB NOT NULL CHECK(length(fingerprint)=32)) STRICT";
const TURN_INDEX: &str = "CREATE INDEX requests_turn ON requests(turn, source DESC)";

#[cfg(test)]
mod tests;

#[derive(Debug, thiserror::Error)]
pub enum PromptShapeError {
    #[error("prompt shape storage I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("prompt shape database failed: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("prompt shape storage contract is invalid")]
    Invalid,
}

#[derive(Debug)]
pub struct StoredPromptShape {
    pub source: u64,
    pub turn: u64,
    pub profile: Vec<u8>,
    pub fingerprint: [u8; 32],
}

#[derive(Debug)]
pub struct PromptShapeStore {
    connection: Connection,
}
impl PromptShapeStore {
    /// Open one exact, private schema with a bounded page cache and no lifetime scan.
    /// # Errors
    /// Rejects unsafe files, foreign schemas, and unavailable storage.
    pub fn open(path: &Path) -> Result<Self, PromptShapeError> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(file) => file.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(PromptShapeError::Invalid);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o077 != 0 || metadata.nlink() != 1 {
                return Err(PromptShapeError::Invalid);
            }
        }
        let parent = path.parent().ok_or(PromptShapeError::Invalid)?;
        let path =
            fs::canonicalize(parent)?.join(path.file_name().ok_or(PromptShapeError::Invalid)?);
        let mut connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(Duration::from_secs(2))?;
        connection.pragma_update(None, "cache_size", -256)?;
        connection.pragma_update(None, "mmap_size", 0)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id: u32 = transaction.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let objects: i64 = transaction.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if objects == 0 && id == 0 {
            transaction.execute_batch(PROFILES)?;
            transaction.execute_batch(REQUESTS)?;
            transaction.execute_batch(TURN_INDEX)?;
            transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
        } else {
            if id != APPLICATION_ID || objects != 3 {
                return Err(PromptShapeError::Invalid);
            }
            for (name, expected) in [
                ("profiles", PROFILES),
                ("requests", REQUESTS),
                ("requests_turn", TURN_INDEX),
            ] {
                let actual: String = transaction.query_row(
                    "SELECT sql FROM sqlite_schema WHERE name=?1",
                    [name],
                    |row| row.get(0),
                )?;
                if actual != expected {
                    return Err(PromptShapeError::Invalid);
                }
            }
        }
        transaction.commit()?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(Self { connection })
    }

    /// Commit the first request shape at one immutable canonical source boundary.
    /// # Errors
    /// Rejects oversized profiles, source substitution, and failed durable writes.
    pub fn record(
        &mut self,
        turn: u64,
        source: u64,
        profile: &[u8],
        fingerprint: [u8; 32],
    ) -> Result<(), PromptShapeError> {
        if profile.len() > MAX_PROFILE_BYTES {
            return Err(PromptShapeError::Invalid);
        }
        let id = *blake3::hash(profile).as_bytes();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT OR IGNORE INTO profiles(id,body) VALUES (?1,?2)",
            params![id.as_slice(), profile],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO requests(source,turn,profile,fingerprint) VALUES (?1,?2,?3,?4)",
            params![
                source.to_be_bytes().as_slice(),
                turn.to_be_bytes().as_slice(),
                id.as_slice(),
                fingerprint.as_slice()
            ],
        )?;
        let matched: bool = transaction.query_row(
            "SELECT turn=?2 AND profile=?3 AND fingerprint=?4 FROM requests WHERE source=?1",
            params![
                source.to_be_bytes().as_slice(),
                turn.to_be_bytes().as_slice(),
                id.as_slice(),
                fingerprint.as_slice()
            ],
            |row| row.get(0),
        )?;
        if !matched {
            return Err(PromptShapeError::Invalid);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Read the newest physical request for a turn, or the newest request overall.
    /// Consumers must match `source` against their canonical history boundary.
    /// # Errors
    /// Rejects corrupt references, hashes, allocation limits, and failed reads.
    pub fn read(
        &self,
        turn: Option<u64>,
        source: Option<u64>,
    ) -> Result<Option<StoredPromptShape>, PromptShapeError> {
        let sql = if source.is_some() {
            "SELECT source,turn,profile,fingerprint FROM requests WHERE source=?1"
        } else if turn.is_some() {
            "SELECT source,turn,profile,fingerprint FROM requests WHERE turn=?1 ORDER BY source DESC LIMIT 1"
        } else {
            "SELECT source,turn,profile,fingerprint FROM requests ORDER BY source DESC LIMIT 1"
        };
        let mut statement = self.connection.prepare_cached(sql)?;
        let value = source.or(turn).map(u64::to_be_bytes);
        let mut rows = if let Some(value) = value {
            statement.query([value.as_slice()])?
        } else {
            statement.query([])?
        };
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let decode = |index| -> Result<[u8; 8], PromptShapeError> {
            row.get_ref(index)?
                .as_blob()
                .map_err(|_| PromptShapeError::Invalid)?
                .try_into()
                .map_err(|_| PromptShapeError::Invalid)
        };
        let source = u64::from_be_bytes(decode(0)?);
        let actual_turn = u64::from_be_bytes(decode(1)?);
        if turn.is_some_and(|turn| turn != actual_turn) {
            return Err(PromptShapeError::Invalid);
        }
        let id: [u8; 32] = row
            .get_ref(2)?
            .as_blob()
            .map_err(|_| PromptShapeError::Invalid)?
            .try_into()
            .map_err(|_| PromptShapeError::Invalid)?;
        let fingerprint: [u8; 32] = row
            .get_ref(3)?
            .as_blob()
            .map_err(|_| PromptShapeError::Invalid)?
            .try_into()
            .map_err(|_| PromptShapeError::Invalid)?;
        let mut profile = self.connection.prepare_cached("SELECT CASE WHEN length(body)<=4194304 THEN body ELSE NULL END FROM profiles WHERE id=?1")?;
        let body = profile
            .query_row([id.as_slice()], |row| {
                let bytes = row.get_ref(0)?.as_blob()?;
                if bytes.len() > MAX_PROFILE_BYTES {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                Ok(bytes.to_vec())
            })
            .optional()?
            .ok_or(PromptShapeError::Invalid)?;
        if blake3::hash(&body).as_bytes() != &id {
            return Err(PromptShapeError::Invalid);
        }
        Ok(Some(StoredPromptShape {
            source,
            turn: actual_turn,
            profile: body,
            fingerprint,
        }))
    }
}
