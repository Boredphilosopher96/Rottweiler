//! Current-schema admission for the shared `SQLite` authority and derived tables.
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
pub(super) const SESSIONS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS sessions(
               id TEXT NOT NULL UNIQUE,
               title TEXT NOT NULL,
               updated_unix_ms INTEGER NOT NULL,
               cost_micros INTEGER NOT NULL,
               turn_count INTEGER NOT NULL DEFAULT 0,
               transcript TEXT NOT NULL,
               projected_sequence TEXT
             );";
pub(super) const SEARCH_SCHEMA: &str = "CREATE VIRTUAL TABLE IF NOT EXISTS sessions_fts USING fts5(
               title,transcript,content='sessions',content_rowid='rowid'
             );
             CREATE TRIGGER IF NOT EXISTS sessions_ai AFTER INSERT ON sessions BEGIN
               INSERT INTO sessions_fts(rowid,title,transcript)
               VALUES (new.rowid,new.title,new.transcript);
             END;
             CREATE TRIGGER IF NOT EXISTS sessions_ad AFTER DELETE ON sessions BEGIN
               INSERT INTO sessions_fts(sessions_fts,rowid,title,transcript)
               VALUES ('delete',old.rowid,old.title,old.transcript);
             END;
             CREATE TRIGGER IF NOT EXISTS sessions_au AFTER UPDATE ON sessions BEGIN
               INSERT INTO sessions_fts(sessions_fts,rowid,title,transcript)
               VALUES ('delete',old.rowid,old.title,old.transcript);
               INSERT INTO sessions_fts(rowid,title,transcript)
               VALUES (new.rowid,new.title,new.transcript);
             END;";

pub(super) fn validate_accounting(connection: &Connection) -> Result<(), SessionStoreError> {
    validate_table(connection, "turn_accounting", ACCOUNTING_SCHEMA)?;
    let extra_unique = connection.query_row("SELECT 1 FROM pragma_index_list('turn_accounting') WHERE \"unique\" != 0 AND origin != 'pk' LIMIT 1", [], |_| Ok(())).optional()?;
    if extra_unique.is_some() {
        return Err(SessionStoreError::UnsupportedSqliteSchema {
            table: "turn_accounting",
        });
    }
    Ok(())
}
pub(super) fn validate_sessions(connection: &Connection) -> Result<(), SessionStoreError> {
    validate_table(connection, "sessions", SESSIONS_SCHEMA)
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
    connection.execute_batch(ACCOUNTING_SCHEMA)?;
    connection.execute_batch("CREATE INDEX IF NOT EXISTS turn_accounting_session_time ON turn_accounting(session_id,emitted_at_utc); CREATE INDEX IF NOT EXISTS turn_accounting_day_time ON turn_accounting(utc_day,emitted_at_utc); CREATE INDEX IF NOT EXISTS turn_accounting_time ON turn_accounting(emitted_at_utc);")?;
    Ok(())
}
pub(super) fn ensure_sessions_schema(connection: &Connection) -> Result<(), SessionStoreError> {
    validate_sessions(connection)?;
    connection.execute_batch(SESSIONS_SCHEMA)?;
    connection.execute_batch(SEARCH_SCHEMA)?;
    Ok(())
}

pub(super) fn open_accounting_connection(path: &Path) -> Result<Connection, SessionStoreError> {
    let connection = Connection::open(path)?;
    validate_accounting(&connection)?;
    configure_connection(&connection)?;
    ensure_accounting_schema(&connection)?;
    Ok(connection)
}
