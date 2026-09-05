//! Durable at-most-once admission for host mutations, independent of client connections.
//!
//! An admitted row is never removed automatically. If effects and completion are
//! separated by a crash, the row remains indeterminate and cannot authorize rerun.
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use rw_types::{CommandOutcome, EngineEvent, RequestId};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::Path, time::Duration};

const MAX_RECEIPT_BYTES: usize = 16 * 1024 * 1024;
const APPLICATION_ID: u32 = 0x52574f50;

#[derive(Debug, thiserror::Error)]
pub enum ReceiptError {
    #[error("command receipt I/O failed")]
    Io(#[from] std::io::Error),
    #[error("command receipt database failed")]
    Sql(#[from] rusqlite::Error),
    #[error("command receipt encoding failed")]
    Json(#[from] serde_json::Error),
    #[error("command receipt contract is invalid")]
    Invalid,
    #[error("operation identity was reused for another command")]
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandReceipt {
    pub outcome: CommandOutcome,
    pub events: Vec<EngineEvent>,
}

#[derive(Debug)]
pub enum ReceiptAdmission {
    Admitted,
    Indeterminate,
    Completed(CommandReceipt),
}

pub struct CommandReceipts {
    connection: Connection,
}
impl CommandReceipts {
    /// Opens private authoritative receipts and rejects foreign database schemas.
    /// # Errors
    /// Rejects unsafe storage, invalid schema, and unavailable SQLite state.
    pub fn open(path: &Path) -> Result<Self, ReceiptError> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(file) => {
                file.sync_all()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(ReceiptError::Invalid);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o077 != 0 || metadata.nlink() != 1 {
                return Err(ReceiptError::Invalid);
            }
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(Duration::from_secs(2))?;
        connection.pragma_update(None, "cache_size", -2048)?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let mut store = Self { connection };
        store.initialize()?;
        #[cfg(unix)]
        fs::File::open(path.parent().ok_or(ReceiptError::Invalid)?)?.sync_all()?;
        Ok(store)
    }

    fn initialize(&mut self) -> Result<(), ReceiptError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let application_id: u32 =
            transaction.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let tables: i64 = transaction.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if application_id == 0 && tables == 0 {
            transaction.execute_batch("CREATE TABLE command_receipts (operation_id TEXT PRIMARY KEY NOT NULL, fingerprint TEXT NOT NULL, completion BLOB) STRICT;")?;
            transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
        } else if application_id != APPLICATION_ID || tables != 1 {
            return Err(ReceiptError::Invalid);
        }
        let definition: String = transaction.query_row(
            "SELECT sql FROM sqlite_schema WHERE name='command_receipts'",
            [],
            |row| row.get(0),
        )?;
        if definition
            != "CREATE TABLE command_receipts (operation_id TEXT PRIMARY KEY NOT NULL, fingerprint TEXT NOT NULL, completion BLOB) STRICT"
        {
            return Err(ReceiptError::Invalid);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Persists admission before any mutation. A duplicate pending row never reruns.
    /// # Errors
    /// Rejects identity substitution, malformed inputs, or failed durable admission.
    pub fn admit(
        &mut self,
        operation: &RequestId,
        fingerprint: &str,
    ) -> Result<ReceiptAdmission, ReceiptError> {
        validate(operation, fingerprint)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, Option<Vec<u8>>)> = transaction.query_row(
            "SELECT fingerprint, CASE WHEN length(completion)<=?2 THEN completion ELSE NULL END FROM command_receipts WHERE operation_id=?1",
            params![operation.0, 16 * 1024 * 1024_i64], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        if let Some((stored, completion)) = existing {
            if stored != fingerprint {
                return Err(ReceiptError::Conflict);
            }
            return match completion {
                Some(bytes) => Ok(ReceiptAdmission::Completed(serde_json::from_slice(&bytes)?)),
                None => Ok(ReceiptAdmission::Indeterminate),
            };
        }
        transaction.execute(
            "INSERT INTO command_receipts(operation_id,fingerprint,completion) VALUES (?1,?2,NULL)",
            params![operation.0, fingerprint],
        )?;
        transaction.commit()?;
        Ok(ReceiptAdmission::Admitted)
    }

    /// Stores the correlated outcome after effects settle; never changes identity.
    /// # Errors
    /// Rejects unknown admissions, conflicting outcomes, or oversized encoding.
    pub fn complete(
        &mut self,
        operation: &RequestId,
        fingerprint: &str,
        receipt: &CommandReceipt,
    ) -> Result<(), ReceiptError> {
        validate(operation, fingerprint)?;
        let mut encoded = BoundedWriter(Vec::new());
        serde_json::to_writer(&mut encoded, receipt)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute("UPDATE command_receipts SET completion=?3 WHERE operation_id=?1 AND fingerprint=?2 AND completion IS NULL", params![operation.0, fingerprint, encoded.0])?;
        if changed != 1 {
            return Err(ReceiptError::Conflict);
        }
        transaction.commit()?;
        Ok(())
    }
}
fn validate(operation: &RequestId, fingerprint: &str) -> Result<(), ReceiptError> {
    if !operation.is_valid()
        || fingerprint.len() != 64
        || !fingerprint.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err(ReceiptError::Invalid);
    }
    Ok(())
}
struct BoundedWriter(Vec<u8>);
impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let required = self
            .0
            .len()
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_RECEIPT_BYTES)
            .ok_or_else(|| std::io::Error::other("receipt byte limit"))?;
        if required > self.0.capacity() {
            let target = required
                .max(self.0.capacity().max(4096).saturating_mul(2))
                .min(MAX_RECEIPT_BYTES);
            self.0
                .try_reserve_exact(target - self.0.len())
                .map_err(std::io::Error::other)?;
            if self.0.capacity() > MAX_RECEIPT_BYTES {
                return Err(std::io::Error::other("receipt capacity limit"));
            }
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    fn fingerprint() -> String {
        "a".repeat(64)
    }
    #[test]
    fn admission_and_completion_survive_connections_and_restart() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("operations.sqlite");
        let id = RequestId("operation".into());
        let mut first = CommandReceipts::open(&path).expect("open");
        let mut second = CommandReceipts::open(&path).expect("second");
        assert!(matches!(
            first.admit(&id, &fingerprint()).expect("admit"),
            ReceiptAdmission::Admitted
        ));
        assert!(matches!(
            second.admit(&id, &fingerprint()).expect("duplicate"),
            ReceiptAdmission::Indeterminate
        ));
        assert!(matches!(
            second.admit(&id, &"b".repeat(64)),
            Err(ReceiptError::Conflict)
        ));
        drop(first);
        let mut restarted = CommandReceipts::open(&path).expect("restart");
        assert!(matches!(
            restarted.admit(&id, &fingerprint()).expect("pending"),
            ReceiptAdmission::Indeterminate
        ));
        restarted
            .complete(
                &id,
                &fingerprint(),
                &CommandReceipt {
                    outcome: CommandOutcome::Accepted {},
                    events: Vec::new(),
                },
            )
            .expect("complete");
        assert!(matches!(
            second.admit(&id, &fingerprint()).expect("receipt"),
            ReceiptAdmission::Completed(CommandReceipt {
                outcome: CommandOutcome::Accepted {},
                ..
            })
        ));
    }
    #[test]
    fn foreign_database_is_rejected_without_rewriting_it() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("operations.sqlite");
        let store = CommandReceipts::open(&path).expect("open");
        store
            .connection
            .execute_batch("ALTER TABLE command_receipts ADD COLUMN extra TEXT")
            .expect("foreign schema");
        drop(store);
        assert!(matches!(
            CommandReceipts::open(&path),
            Err(ReceiptError::Invalid)
        ));
    }
}
