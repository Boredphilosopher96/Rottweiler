//! Schema admission for the shared `SQLite` authority and derived tables.
use super::SessionStoreError;
use rusqlite::{Connection, OptionalExtension as _};
use std::path::Path;

pub(super) const ACCOUNTING_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS turn_accounting(
           session_id TEXT NOT NULL,
           turn_id TEXT NOT NULL,
           sequence_id TEXT NOT NULL,
           emitted_at_utc TEXT NOT NULL,
           utc_day TEXT NOT NULL,
           attribution_json TEXT NOT NULL,
           usage_json TEXT NOT NULL,
           cost_json TEXT NOT NULL,
           PRIMARY KEY(session_id,sequence_id)
         );";
const ACCOUNTING_PROGRESS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS accounting_progress(
    session_id TEXT NOT NULL PRIMARY KEY,
    next_sequence TEXT NOT NULL,
    digest BLOB NOT NULL CHECK(length(digest)=32)
);";
const ACCOUNTING_TOTALS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS accounting_totals(
    scope TEXT NOT NULL,
    node INTEGER NOT NULL CHECK(node>0 AND node<=562949953421312),
    totals BLOB NOT NULL CHECK(typeof(totals)='blob' AND length(totals)=112),
    PRIMARY KEY(scope,node)
) WITHOUT ROWID;";
const ACCOUNTING_TOTALS_PROGRESS_SCHEMA: &str =
    "CREATE TABLE IF NOT EXISTS accounting_totals_progress(
    id INTEGER NOT NULL PRIMARY KEY CHECK(id=1),
    projected_rowid INTEGER NOT NULL CHECK(projected_rowid>=0)
);";
pub(super) const SESSIONS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS sessions(
    id TEXT NOT NULL UNIQUE CHECK(length(CAST(id AS BLOB))<=128),
    title TEXT NOT NULL CHECK(length(CAST(title AS BLOB))<=4096),
    updated_unix_ms INTEGER NOT NULL,
    cost_micros INTEGER NOT NULL,
    turn_count INTEGER NOT NULL,
    explicit_title INTEGER NOT NULL CHECK(explicit_title IN (0,1)),
    search_complete INTEGER NOT NULL CHECK(search_complete IN (0,1)),
    next_sequence TEXT NOT NULL CHECK(length(next_sequence)<=20),
    source_digest BLOB NOT NULL CHECK(length(source_digest)=32)
);";
pub(super) const DOCUMENTS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS search_documents(
    session_id TEXT NOT NULL,
    kind INTEGER NOT NULL CHECK(kind IN (0,1)),
    agent_turn TEXT NOT NULL,
    sequence_id TEXT NOT NULL,
    part INTEGER NOT NULL CHECK(part>=0),
    body TEXT NOT NULL,
    UNIQUE(session_id,kind,sequence_id,part)
);";
pub(super) const INVOCATIONS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS search_invocations(
    session_id TEXT NOT NULL,
    invocation_id TEXT NOT NULL,
    agent_turn TEXT NOT NULL,
    PRIMARY KEY(session_id,invocation_id)
) WITHOUT ROWID;";
pub(super) const FTS_SCHEMA: &str = "CREATE VIRTUAL TABLE IF NOT EXISTS sessions_fts USING fts5(
    body,content='search_documents',content_rowid='rowid'
);";
pub(super) const SEARCH_SCHEMA: &str = "CREATE INDEX IF NOT EXISTS search_documents_turn ON search_documents(session_id,length(agent_turn),agent_turn);
CREATE TRIGGER IF NOT EXISTS search_documents_ai AFTER INSERT ON search_documents BEGIN
    INSERT INTO sessions_fts(rowid,body) VALUES (new.rowid,new.body);
END;
CREATE TRIGGER IF NOT EXISTS search_documents_ad AFTER DELETE ON search_documents BEGIN
    INSERT INTO sessions_fts(sessions_fts,rowid,body) VALUES ('delete',old.rowid,old.body);
END;
CREATE TRIGGER IF NOT EXISTS search_documents_au AFTER UPDATE ON search_documents BEGIN
    INSERT INTO sessions_fts(sessions_fts,rowid,body) VALUES ('delete',old.rowid,old.body);
    INSERT INTO sessions_fts(rowid,body) VALUES (new.rowid,new.body);
END;";

pub(super) fn validate_accounting(connection: &Connection) -> Result<(), SessionStoreError> {
    validate_table(connection, "turn_accounting", ACCOUNTING_SCHEMA)?;
    validate_table(connection, "accounting_totals", ACCOUNTING_TOTALS_SCHEMA)?;
    validate_table(
        connection,
        "accounting_totals_progress",
        ACCOUNTING_TOTALS_PROGRESS_SCHEMA,
    )?;
    validate_table(
        connection,
        "accounting_progress",
        ACCOUNTING_PROGRESS_SCHEMA,
    )?;
    let extra_unique = connection.query_row("SELECT 1 FROM pragma_index_list('turn_accounting') WHERE \"unique\" != 0 AND origin != 'pk' LIMIT 1", [], |_| Ok(())).optional()?;
    if extra_unique.is_some() {
        return Err(SessionStoreError::UnsupportedSqliteSchema {
            table: "turn_accounting",
        });
    }
    Ok(())
}
pub(super) fn validate_sessions(connection: &Connection) -> Result<(), SessionStoreError> {
    validate_table(connection, "sessions", SESSIONS_SCHEMA)?;
    validate_table(connection, "search_documents", DOCUMENTS_SCHEMA)?;
    validate_table(connection, "sessions_fts", FTS_SCHEMA)?;
    validate_table(connection, "search_invocations", INVOCATIONS_SCHEMA)?;
    let count: u32 = connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('sessions','search_documents','sessions_fts','search_invocations')", [], |row| row.get(0))?;
    if count != 0 && count != 4 {
        return Err(SessionStoreError::UnsupportedSqliteSchema {
            table: "search_documents",
        });
    }
    Ok(())
}
pub(super) fn validate_table(
    connection: &Connection,
    table: &'static str,
    expected: &str,
) -> Result<(), SessionStoreError> {
    let existing = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(existing) = existing
        && normalized_schema(&existing) != normalized_schema(expected)
    {
        return Err(SessionStoreError::UnsupportedSqliteSchema { table });
    }
    Ok(())
}
fn normalized_schema(schema: &str) -> String {
    schema
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != ';')
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .replace("ifnotexists", "")
}

pub(super) fn configure_connection(connection: &Connection) -> Result<(), SessionStoreError> {
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
    Ok(())
}
pub(super) fn ensure_accounting_schema(connection: &Connection) -> Result<(), SessionStoreError> {
    validate_accounting(connection)?;
    connection.execute_batch("SAVEPOINT rw_accounting_schema")?;
    match create_accounting_schema(connection) {
        Ok(()) => connection
            .execute_batch("RELEASE rw_accounting_schema")
            .map_err(SessionStoreError::from),
        Err(error) => {
            connection
                .execute_batch("ROLLBACK TO rw_accounting_schema; RELEASE rw_accounting_schema")?;
            Err(error)
        }
    }
}
fn create_accounting_schema(connection: &Connection) -> Result<(), SessionStoreError> {
    connection.execute_batch(ACCOUNTING_SCHEMA)?;
    connection.execute_batch(ACCOUNTING_PROGRESS_SCHEMA)?;
    let totals_tables: i64 = connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('accounting_totals','accounting_totals_progress')", [], |row| row.get(0))?;
    if totals_tables == 1 {
        return Err(SessionStoreError::CorruptAccountingTotals);
    }
    if totals_tables == 0 {
        connection.execute_batch(ACCOUNTING_TOTALS_SCHEMA)?;
        connection.execute_batch(ACCOUNTING_TOTALS_PROGRESS_SCHEMA)?;
        connection.execute(
            "INSERT INTO accounting_totals_progress(id,projected_rowid) VALUES(1,0)",
            [],
        )?;
    }
    connection.execute_batch("CREATE INDEX IF NOT EXISTS turn_accounting_session_time ON turn_accounting(session_id,emitted_at_utc); CREATE INDEX IF NOT EXISTS turn_accounting_day_time ON turn_accounting(utc_day,emitted_at_utc); CREATE INDEX IF NOT EXISTS turn_accounting_time ON turn_accounting(emitted_at_utc);")?;
    Ok(())
}
pub(super) fn ensure_sessions_schema(connection: &Connection) -> Result<(), SessionStoreError> {
    validate_sessions(connection)?;
    connection.execute_batch("SAVEPOINT rw_search_schema")?;
    let result = (|| {
        connection.execute_batch(SESSIONS_SCHEMA)?;
        connection.execute_batch(DOCUMENTS_SCHEMA)?;
        connection.execute_batch(INVOCATIONS_SCHEMA)?;
        connection.execute_batch(FTS_SCHEMA)?;
        connection.execute_batch(SEARCH_SCHEMA)?;
        Ok::<(), SessionStoreError>(())
    })();
    match result {
        Ok(()) => connection
            .execute_batch("RELEASE rw_search_schema")
            .map_err(Into::into),
        Err(error) => {
            connection.execute_batch("ROLLBACK TO rw_search_schema; RELEASE rw_search_schema")?;
            Err(error)
        }
    }
}

pub(super) fn open_accounting_connection(path: &Path) -> Result<Connection, SessionStoreError> {
    let connection = Connection::open(path)?;
    validate_accounting(&connection)?;
    configure_connection(&connection)?;
    ensure_accounting_schema(&connection)?;
    Ok(connection)
}
