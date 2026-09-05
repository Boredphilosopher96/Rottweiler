//! Rebuildable session listing and full-text search; accounting authority is preserved.
use super::{
    SessionStoreError,
    accounting::{TurnAccountingEntry, insert_accounting_entry, validate_accounting_entry},
    journal_io::validate_session_id,
    sqlite_schema::{self, configure_connection, ensure_accounting_schema},
    sqlite_snapshot::{read_only_index_snapshot, same_file_identity, validate_read_only_index},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};
use rw_types::SequenceId;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// One denormalized row in the session listing/search index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSummary {
    /// Stable session id.
    pub id: String,
    /// Current display title.
    pub title: String,
    /// Caller-supplied deterministic update time in Unix milliseconds.
    pub updated_unix_ms: i64,
    /// Accumulated ordinary micro-dollar equivalent, when applicable.
    pub cost_micros: i64,
    /// Number of accepted user turns represented by this projection.
    pub turn_count: i64,
}

/// One complete, rebuildable projection of a session event log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionProjection {
    /// Listing fields derived from the log.
    pub summary: SessionSummary,
    /// Searchable transcript derived from the same log prefix.
    pub transcript: String,
    /// Last event incorporated into this projection, or `None` for an empty log.
    pub projected_through: Option<SequenceId>,
}

/// Whether a derived index row agrees with the authoritative event log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionStatus {
    /// No row exists for the session.
    Missing,
    /// The row was built through the requested last event.
    Current,
    /// The row exists but was built from a different log prefix.
    Stale {
        /// Last event represented by the row.
        projected_through: Option<SequenceId>,
    },
}

/// `SQLite` projection for session listing and full-text search.
#[derive(Clone, Debug)]
pub struct SessionIndex {
    path: PathBuf,
}

impl SessionIndex {
    /// Opens or creates `index.sqlite` under the storage root.
    ///
    /// # Errors
    ///
    /// Returns an I/O or `SQLite` initialization error.
    pub fn open(root: &Path) -> Result<Self, SessionStoreError> {
        fs::create_dir_all(root)?;
        let index = Self {
            path: root.join("index.sqlite"),
        };
        index.connection()?;
        Ok(index)
    }

    /// Inserts or replaces one derived session projection.
    ///
    /// # Errors
    ///
    /// Returns an invalid-id or `SQLite` error.
    pub fn upsert(&self, projection: &SessionProjection) -> Result<(), SessionStoreError> {
        validate_session_id(&projection.summary.id)?;
        let connection = self.connection()?;
        upsert_projection(&connection, projection)?;
        Ok(())
    }

    /// Removes one disposable projection after its zero-event log was garbage-collected.
    ///
    /// # Errors
    ///
    /// Returns an invalid-id or `SQLite` error.
    pub fn remove(&self, session_id: &str) -> Result<(), SessionStoreError> {
        validate_session_id(session_id)?;
        self.connection()?
            .execute("DELETE FROM sessions WHERE id=?1", [session_id])?;
        Ok(())
    }

    /// Atomically replaces every derived row from caller-projected event logs.
    ///
    /// The JSONL logs remain authoritative; callers use this after any missing
    /// or stale watermark is detected.
    ///
    /// # Errors
    ///
    /// Returns an invalid-id or `SQLite` transaction error.
    pub fn replace_all(&self, projections: &[SessionProjection]) -> Result<(), SessionStoreError> {
        for projection in projections {
            validate_session_id(&projection.summary.id)?;
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM sessions", [])?;
        for projection in projections {
            upsert_projection(&transaction, projection)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Rebuilds only derived search tables in one transaction.
    ///
    /// Existing accounting and other authoritative tables remain intact. Supplied
    /// journal accounting is reconciled by identity, never used to erase charges.
    /// A corrupt database or unsupported accounting schema requires explicit recovery.
    ///
    /// # Errors
    /// Returns an invalid projection, accounting conflict, schema or `SQLite` error.
    pub fn rebuild(
        root: &Path,
        projections: &[SessionProjection],
        accounting_entries: &[TurnAccountingEntry],
    ) -> Result<Self, SessionStoreError> {
        fs::create_dir_all(root)?;
        for projection in projections {
            validate_session_id(&projection.summary.id)?;
        }
        for entry in accounting_entries {
            validate_accounting_entry(entry)?;
        }
        let index = Self {
            path: root.join("index.sqlite"),
        };
        let mut connection = Connection::open(&index.path)?;
        sqlite_schema::validate_accounting(&connection)?;
        configure_connection(&connection)?;
        ensure_accounting_schema(&connection)?;
        super::accounting::totals::catch_up(&mut connection)?;
        let transaction = connection.transaction()?;
        transaction.execute_batch("DROP TRIGGER IF EXISTS sessions_ai; DROP TRIGGER IF EXISTS sessions_ad; DROP TRIGGER IF EXISTS sessions_au; DROP TABLE IF EXISTS sessions_fts; DROP TABLE IF EXISTS sessions;")?;
        sqlite_schema::ensure_sessions_schema(&transaction)?;
        for projection in projections {
            upsert_projection(&transaction, projection)?;
        }
        for entry in accounting_entries {
            insert_accounting_entry(&transaction, entry)?;
        }
        transaction.commit()?;
        Ok(index)
    }

    /// Compares one row's projection watermark with the event log's last id.
    ///
    /// # Errors
    ///
    /// Returns an invalid-id, corrupt-watermark, or `SQLite` query error.
    pub fn projection_status(
        &self,
        id: &str,
        log_last_sequence: Option<SequenceId>,
    ) -> Result<ProjectionStatus, SessionStoreError> {
        validate_session_id(id)?;
        let connection = self.connection()?;
        let stored = connection
            .query_row(
                "SELECT projected_sequence FROM sessions WHERE id=?1",
                [id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        let Some(stored) = stored else {
            return Ok(ProjectionStatus::Missing);
        };
        let Some(stored) = stored else {
            return Ok(ProjectionStatus::Stale {
                projected_through: None,
            });
        };
        let projected_through = if stored == "-" {
            None
        } else {
            Some(
                stored
                    .parse::<u64>()
                    .map(SequenceId)
                    .map_err(|_| SessionStoreError::CorruptProjectionWatermark)?,
            )
        };
        if projected_through == log_last_sequence {
            Ok(ProjectionStatus::Current)
        } else {
            Ok(ProjectionStatus::Stale { projected_through })
        }
    }

    fn connection(&self) -> Result<Connection, SessionStoreError> {
        let connection = Connection::open(&self.path)?;
        sqlite_schema::validate_accounting(&connection)?;
        sqlite_schema::validate_sessions(&connection)?;
        configure_connection(&connection)?;
        ensure_accounting_schema(&connection)?;
        sqlite_schema::ensure_sessions_schema(&connection)?;
        Ok(connection)
    }

    /// Lists newest sessions with a deterministic id tie-break.
    ///
    /// # Errors
    ///
    /// Returns a `SQLite` query error.
    pub fn list(&self, limit: usize) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let limit = i64::try_from(limit).map_err(|_| SessionStoreError::LimitOverflow)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,title,updated_unix_ms,cost_micros,turn_count FROM sessions \
             ORDER BY updated_unix_ms DESC,id ASC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], summary_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Searches title and transcript text through `SQLite` FTS5.
    ///
    /// # Errors
    ///
    /// Returns a `SQLite` query error.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).map_err(|_| SessionStoreError::LimitOverflow)?;
        let connection = self.connection()?;
        sqlite_schema::validate_sessions(&connection)?;
        let mut statement = connection.prepare(
            "SELECT s.id,s.title,s.updated_unix_ms,s.cost_micros,s.turn_count \
             FROM sessions_fts f JOIN sessions s ON s.rowid=f.rowid \
             WHERE sessions_fts MATCH ?1 \
             ORDER BY bm25(sessions_fts),s.updated_unix_ms DESC,s.id ASC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![query, limit], summary_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Searches an existing index through a `SQLite` read-only connection.
    ///
    /// # Errors
    ///
    /// Returns an error when the query or result limit is too large, the index
    /// path is unsafe or changes during the read, or the database cannot be
    /// queried through its read-only connection.
    pub fn search_read_only(
        root: &Path,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        if query.len() > 512 {
            return Err(SessionStoreError::SearchQueryTooLarge);
        }
        if limit > 1_001 {
            return Err(SessionStoreError::SearchLimitTooLarge);
        }
        let query = plain_fts_query(query);
        let limit = i64::try_from(limit).map_err(|_| SessionStoreError::LimitOverflow)?;
        let path = root.join("index.sqlite");
        let before = validate_read_only_index(&path)?;
        let canonical_root = fs::canonicalize(root)?;
        let canonical_path = fs::canonicalize(&path)?;
        if canonical_path.parent() != Some(canonical_root.as_path()) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        let snapshot = read_only_index_snapshot(&canonical_root, &before)?;
        let snapshot_path = fs::canonicalize(snapshot.path().join("index.sqlite"))?;
        let connection = Connection::open_with_flags(
            snapshot_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let after = validate_read_only_index(&path)?;
        if !same_file_identity(&before, &after) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        sqlite_schema::validate_sessions(&connection)?;
        let mut statement = connection.prepare(
            "SELECT s.id,s.title,s.updated_unix_ms,s.cost_micros,s.turn_count \
             FROM sessions_fts f JOIN sessions s ON s.rowid=f.rowid \
             WHERE sessions_fts MATCH ?1 \
             ORDER BY bm25(sessions_fts),s.updated_unix_ms DESC,s.id ASC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![query, limit], summary_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Lists newest sessions from a private read-only snapshot of an existing
    /// index. The live database and WAL are never modified by this query.
    ///
    /// # Errors
    ///
    /// Returns an error when the result limit is too large, the index path is
    /// unsafe or changes during the read, or the snapshot cannot be queried.
    pub fn list_read_only(
        root: &Path,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, SessionStoreError> {
        if limit > 1_001 {
            return Err(SessionStoreError::SearchLimitTooLarge);
        }
        let limit = i64::try_from(limit).map_err(|_| SessionStoreError::LimitOverflow)?;
        let path = root.join("index.sqlite");
        let before = validate_read_only_index(&path)?;
        let canonical_root = fs::canonicalize(root)?;
        let canonical_path = fs::canonicalize(&path)?;
        if canonical_path.parent() != Some(canonical_root.as_path()) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        let snapshot = read_only_index_snapshot(&canonical_root, &before)?;
        let snapshot_path = fs::canonicalize(snapshot.path().join("index.sqlite"))?;
        let connection = Connection::open_with_flags(
            snapshot_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let after = validate_read_only_index(&path)?;
        if !same_file_identity(&before, &after) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        sqlite_schema::validate_sessions(&connection)?;
        let mut statement = connection.prepare(
            "SELECT id,title,updated_unix_ms,cost_micros,turn_count FROM sessions \
             ORDER BY updated_unix_ms DESC,id ASC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], summary_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Returns one projection by id.
    ///
    /// # Errors
    ///
    /// Returns an invalid-id or `SQLite` query error.
    pub fn get(&self, id: &str) -> Result<Option<SessionSummary>, SessionStoreError> {
        validate_session_id(id)?;
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id,title,updated_unix_ms,cost_micros,turn_count FROM sessions WHERE id=?1",
                [id],
                summary_from_row,
            )
            .optional()
            .map_err(Into::into)
    }
}

fn plain_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionSummary> {
    Ok(SessionSummary {
        id: row.get(0)?,
        title: row.get(1)?,
        updated_unix_ms: row.get(2)?,
        cost_micros: row.get(3)?,
        turn_count: row.get(4)?,
    })
}

pub(super) fn upsert_projection(
    connection: &Connection,
    projection: &SessionProjection,
) -> Result<(), SessionStoreError> {
    connection.execute(
        "INSERT INTO sessions(\
           id,title,updated_unix_ms,cost_micros,turn_count,transcript,projected_sequence\
         ) VALUES (?1,?2,?3,?4,?5,?6,?7) \
         ON CONFLICT(id) DO UPDATE SET title=excluded.title, \
         updated_unix_ms=excluded.updated_unix_ms, \
         cost_micros=excluded.cost_micros, turn_count=excluded.turn_count, \
         transcript=excluded.transcript, \
         projected_sequence=excluded.projected_sequence",
        params![
            projection.summary.id,
            projection.summary.title,
            projection.summary.updated_unix_ms,
            projection.summary.cost_micros,
            projection.summary.turn_count,
            projection.transcript,
            projection
                .projected_through
                .map_or_else(|| "-".to_owned(), |sequence| sequence.0.to_string())
        ],
    )?;
    Ok(())
}
