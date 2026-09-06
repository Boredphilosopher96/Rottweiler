//! Rebuildable session listing and full-text search; accounting authority is preserved.
use super::{
    SessionStoreError,
    index_read::read_index,
    journal::JournalPrefixIdentity,
    journal_io::validate_session_id,
    sqlite_schema::{self, configure_connection, ensure_accounting_schema},
};
use rusqlite::{Connection, OptionalExtension as _, params};
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

/// Bounded listing state for one exact journal prefix. Search documents live in `SQLite`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionProjection {
    /// Listing fields derived from the log.
    pub summary: SessionSummary,
    /// Whether a title event selected the displayed title.
    pub explicit_title: bool,
    /// All documents through the source captured by the projector are present.
    pub complete: bool,
    /// Exact journal prefix incorporated into this projection.
    pub source: JournalPrefixIdentity,
    /// Exact bounded input claims folded through the same source watermark.
    pub input_claims: Vec<u8>,
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

    /// Atomically writes bounded listing metadata and its title document.
    /// # Errors
    /// Rejects malformed identities and failed `SQLite` writes.
    pub fn upsert(&self, projection: &SessionProjection) -> Result<(), SessionStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        upsert_projection(&transaction, projection)?;
        transaction.commit()?;
        Ok(())
    }

    /// Removes one disposable projection and its searchable documents.
    /// # Errors
    /// Rejects malformed identities and failed `SQLite` writes.
    pub fn remove(&self, session_id: &str) -> Result<(), SessionStoreError> {
        validate_session_id(session_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "DELETE FROM search_documents WHERE session_id=?1",
            [session_id],
        )?;
        transaction.execute(
            "DELETE FROM search_invocations WHERE session_id=?1",
            [session_id],
        )?;
        transaction.execute("DELETE FROM sessions WHERE id=?1", [session_id])?;
        transaction.commit()?;
        Ok(())
    }

    /// Clears rebuildable search state without touching accounting or reservations.
    /// # Errors
    /// Rejects unsupported authoritative schemas and failed `SQLite` writes.
    pub fn reset_derived(root: &Path) -> Result<Self, SessionStoreError> {
        fs::create_dir_all(root)?;
        let index = Self {
            path: root.join("index.sqlite"),
        };
        let mut connection = Connection::open(&index.path)?;
        sqlite_schema::validate_accounting(&connection)?;
        configure_connection(&connection)?;
        ensure_accounting_schema(&connection)?;
        let transaction = connection.transaction()?;
        transaction.execute_batch("DROP TRIGGER IF EXISTS search_documents_ai; DROP TRIGGER IF EXISTS search_documents_ad; DROP TRIGGER IF EXISTS search_documents_au; DROP TABLE IF EXISTS sessions_fts; DROP TABLE IF EXISTS search_documents; DROP TABLE IF EXISTS sessions; DROP TABLE IF EXISTS search_invocations;")?;
        sqlite_schema::create_sessions_schema(&transaction)?;
        transaction.commit()?;
        Ok(index)
    }

    /// Reads bounded metadata and its exact source identity.
    /// # Errors
    /// Rejects corrupt identities and failed `SQLite` reads.
    pub fn projection(&self, id: &str) -> Result<Option<SessionProjection>, SessionStoreError> {
        validate_session_id(id)?;
        read_projection(&self.connection()?, id)
    }

    /// Compares one row's source watermark with the authoritative log.
    /// # Errors
    /// Rejects malformed identities, watermarks and failed `SQLite` reads.
    pub fn projection_status(
        &self,
        id: &str,
        last: Option<SequenceId>,
    ) -> Result<ProjectionStatus, SessionStoreError> {
        let Some(projection) = self.projection(id)? else {
            return Ok(ProjectionStatus::Missing);
        };
        let projected_through = projection
            .source
            .next_sequence
            .checked_sub(1)
            .map(SequenceId);
        Ok(if projected_through == last {
            ProjectionStatus::Current
        } else {
            ProjectionStatus::Stale { projected_through }
        })
    }

    /// Publishes one bounded source page and its documents under a source CAS.
    /// # Errors
    /// Rejects a changed source cursor, malformed documents and failed writes.
    pub fn apply_page(
        &self,
        expected: Option<JournalPrefixIdentity>,
        projection: &SessionProjection,
        apply: impl FnOnce(&SearchDocumentWriter<'_>) -> Result<(), SessionStoreError>,
    ) -> Result<(), SessionStoreError> {
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let actual =
            read_projection(&transaction, &projection.summary.id)?.map(|value| value.source);
        if actual != expected {
            return Err(SessionStoreError::CorruptProjectionWatermark);
        }
        let writer = SearchDocumentWriter {
            connection: &transaction,
            session: &projection.summary.id,
        };
        apply(&writer)?;
        upsert_projection(&transaction, projection)?;
        transaction.commit()?;
        Ok(())
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
             WHERE search_complete=1 ORDER BY updated_unix_ms DESC,id ASC LIMIT ?1",
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
        query_search(&self.connection()?, query, limit)
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
        read_index(root, |connection| query_search(connection, query, limit))
    }

    /// Lists newest sessions using a live `SQLite` read transaction.
    /// `SQLite` may maintain its transient WAL coordination files; stored rows are read-only.
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
        read_index(root, |connection| {
            sqlite_schema::validate_sessions(connection)?;
            let mut statement = connection.prepare(
                "SELECT id,title,updated_unix_ms,cost_micros,turn_count FROM sessions WHERE search_complete=1 ORDER BY updated_unix_ms DESC,id ASC LIMIT ?1",
            )?;
            let rows = statement.query_map([limit], summary_from_row)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
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

fn query_search(
    connection: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<SessionSummary>, SessionStoreError> {
    if query.len() > 512 {
        return Err(SessionStoreError::SearchQueryTooLarge);
    }
    if limit > 1001 {
        return Err(SessionStoreError::SearchLimitTooLarge);
    }
    let terms = query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    sqlite_schema::validate_sessions(connection)?;
    let sets = (1..=terms.len()).map(|number| format!("SELECT d.session_id FROM sessions_fts JOIN search_documents d ON d.rowid=sessions_fts.rowid WHERE sessions_fts MATCH ?{number}")).collect::<Vec<_>>().join(" INTERSECT ");
    let sql = format!(
        "SELECT s.id,s.title,s.updated_unix_ms,s.cost_micros,s.turn_count FROM sessions s JOIN ({sets}) matching ON matching.session_id=s.id WHERE s.search_complete=1 ORDER BY s.updated_unix_ms DESC,s.id ASC LIMIT ?{}",
        terms.len() + 1
    );
    let mut arguments = terms
        .into_iter()
        .map(rusqlite::types::Value::Text)
        .collect::<Vec<_>>();
    arguments.push(rusqlite::types::Value::Integer(
        i64::try_from(limit).map_err(|_| SessionStoreError::LimitOverflow)?,
    ));
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(arguments), summary_from_row)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
    validate_session_id(&projection.summary.id)?;
    if projection.summary.title.len() > 4096 {
        return Err(SessionStoreError::SearchDocumentTooLarge { max_bytes: 4096 });
    }
    let claims = &projection.input_claims;
    if claims.is_empty() || claims.len() > rw_types::input_claims::MAX_INPUT_CLAIM_CHECKPOINT_BYTES
    {
        return Err(SessionStoreError::CorruptEvent(
            "input claim checkpoint bytes",
        ));
    }
    connection.execute("INSERT INTO sessions(id,title,updated_unix_ms,cost_micros,turn_count,explicit_title,search_complete,next_sequence,source_digest,input_claims) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(id) DO UPDATE SET title=excluded.title,updated_unix_ms=excluded.updated_unix_ms,cost_micros=excluded.cost_micros,turn_count=excluded.turn_count,explicit_title=excluded.explicit_title,search_complete=excluded.search_complete,next_sequence=excluded.next_sequence,source_digest=excluded.source_digest,input_claims=excluded.input_claims", params![projection.summary.id,projection.summary.title,projection.summary.updated_unix_ms,projection.summary.cost_micros,projection.summary.turn_count,projection.explicit_title,projection.complete,projection.source.next_sequence.to_string(),projection.source.digest.as_slice(),claims])?;
    connection.execute("INSERT INTO search_documents(session_id,kind,agent_turn,sequence_id,part,body) VALUES(?1,0,'0','0',0,?2) ON CONFLICT(session_id,kind,sequence_id,part) DO UPDATE SET body=excluded.body WHERE body<>excluded.body", params![projection.summary.id,projection.summary.title])?;
    Ok(())
}

fn read_projection(
    connection: &Connection,
    id: &str,
) -> Result<Option<SessionProjection>, SessionStoreError> {
    let row = connection.query_row("SELECT id,title,updated_unix_ms,cost_micros,turn_count,explicit_title,search_complete,next_sequence,source_digest,input_claims FROM sessions WHERE id=?1", [id], |row| {
        Ok((summary_from_row(row)?, row.get::<_,bool>(5)?, row.get::<_,bool>(6)?, row.get::<_,String>(7)?, row.get::<_,Vec<u8>>(8)?, bounded_claim_checkpoint(row)?))
    }).optional()?;
    row.map(
        |(summary, explicit_title, complete, next, digest, claims)| {
            let next_sequence = next
                .parse::<u64>()
                .map_err(|_| SessionStoreError::CorruptProjectionWatermark)?;
            if next_sequence.to_string() != next {
                return Err(SessionStoreError::CorruptProjectionWatermark);
            }
            let digest = digest
                .try_into()
                .map_err(|_| SessionStoreError::CorruptProjectionWatermark)?;
            Ok(SessionProjection {
                input_claims: claims,
                summary,
                explicit_title,
                complete,
                source: JournalPrefixIdentity {
                    next_sequence,
                    digest,
                },
            })
        },
    )
    .transpose()
}

/// Transaction-scoped document writer. Each body borrows already-admitted source data.
pub struct SearchDocumentWriter<'a> {
    connection: &'a Connection,
    session: &'a str,
}
impl SearchDocumentWriter<'_> {
    /// Adds one source field, with exact turn/sequence identity for replay and rewind.
    /// # Errors
    /// Rejects an oversized body and failed `SQLite` writes.
    pub fn text(
        &self,
        agent_turn: u64,
        sequence: SequenceId,
        part: u32,
        text: &str,
    ) -> Result<(), SessionStoreError> {
        if text.len() > 16 * 1024 * 1024 {
            return Err(SessionStoreError::SearchDocumentTooLarge {
                max_bytes: 16 * 1024 * 1024,
            });
        }
        self.connection.execute("INSERT INTO search_documents(session_id,kind,agent_turn,sequence_id,part,body) VALUES(?1,1,?2,?3,?4,?5)", params![self.session,agent_turn.to_string(),sequence.0.to_string(),part,text])?;
        Ok(())
    }
    /// Records the host-owned invocation whose result may add searchable fields.
    /// # Errors
    /// Rejects duplicate identities and failed `SQLite` writes.
    pub fn start_tool(&self, invocation: &str, agent_turn: u64) -> Result<(), SessionStoreError> {
        self.connection.execute(
            "INSERT INTO search_invocations(session_id,invocation_id,agent_turn) VALUES(?1,?2,?3)",
            params![self.session, invocation, agent_turn.to_string()],
        )?;
        Ok(())
    }

    /// Consumes a live invocation; discarded or already settled calls have no result authority.
    /// # Errors
    /// Rejects a contradictory turn or failed `SQLite` access.
    pub fn finish_tool(
        &self,
        invocation: &str,
        agent_turn: u64,
    ) -> Result<bool, SessionStoreError> {
        let turn: Option<String> = self.connection.query_row("SELECT agent_turn FROM search_invocations WHERE session_id=?1 AND invocation_id=?2", params![self.session,invocation], |row| row.get(0)).optional()?;
        let Some(turn) = turn else {
            return Ok(false);
        };
        if turn != agent_turn.to_string() {
            return Err(SessionStoreError::CorruptEvent(
                "search invocation turn mismatch",
            ));
        }
        self.connection.execute(
            "DELETE FROM search_invocations WHERE session_id=?1 AND invocation_id=?2",
            params![self.session, invocation],
        )?;
        Ok(true)
    }

    /// Removes documents belonging to discarded agent turns in the same transaction.
    /// # Errors
    /// Returns a `SQLite` write failure.
    pub fn rewind(&self, through: u64) -> Result<(), SessionStoreError> {
        let turn = through.to_string();
        self.connection.execute("DELETE FROM search_documents WHERE session_id=?1 AND kind=1 AND (length(agent_turn)>length(?2) OR (length(agent_turn)=length(?2) AND agent_turn>?2))", params![self.session,turn])?;
        self.connection.execute("DELETE FROM search_invocations WHERE session_id=?1 AND (length(agent_turn)>length(?2) OR (length(agent_turn)=length(?2) AND agent_turn>?2))", params![self.session,turn])?;
        Ok(())
    }
}

fn bounded_claim_checkpoint(row: &rusqlite::Row<'_>) -> rusqlite::Result<Vec<u8>> {
    let bytes = row.get_ref(9)?.as_blob().map_err(|_| {
        rusqlite::Error::InvalidColumnType(9, "input_claims".into(), rusqlite::types::Type::Blob)
    })?;
    if bytes.is_empty() || bytes.len() > rw_types::input_claims::MAX_INPUT_CLAIM_CHECKPOINT_BYTES {
        return Err(rusqlite::Error::InvalidColumnType(
            9,
            "input_claims".into(),
            rusqlite::types::Type::Blob,
        ));
    }
    Ok(bytes.to_vec())
}
