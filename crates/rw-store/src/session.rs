//! Crash-safe append-only session logs and the derived `SQLite` session index.

use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::FileExt as _;
#[cfg(not(unix))]
use std::{
    fs::OpenOptions,
    io::{Read as _, Seek as _, SeekFrom},
};

use rusqlite::{Connection, OptionalExtension, params};
use rw_types::SequenceId;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

const EVENT_SCHEMA_VERSION: u16 = 1;
static INDEX_TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One versioned event in a session's public JSONL transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope<T> {
    /// Event-log schema version.
    pub schema_version: u16,
    /// Contiguous zero-based sequence within the session.
    pub sequence: SequenceId,
    /// Provider- and UI-neutral event payload.
    pub event: T,
}

/// Append-only event log for one session actor.
#[derive(Debug)]
pub struct SessionEventLog {
    path: PathBuf,
    next_sequence: u64,
    file: File,
}

impl SessionEventLog {
    /// Opens or creates `sessions/<id>/events.jsonl`, repairing only an
    /// incomplete final record left by a killed writer.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe id, I/O failure, corrupt complete record,
    /// schema mismatch, or non-contiguous sequence.
    pub fn open(root: &Path, session_id: &str) -> Result<Self, SessionStoreError> {
        validate_session_id(session_id)?;
        let directory = root.join("sessions").join(session_id);
        let path = directory.join("events.jsonl");

        #[cfg(unix)]
        let file = open_session_file(root, session_id)?;
        #[cfg(not(unix))]
        let file = open_session_file_portable(root, &directory, &path)?;

        ensure_regular_file(&file)?;
        lock_writer(&file)?;
        let next_sequence = recover_and_validate(&file)?;
        Ok(Self {
            path,
            next_sequence,
            file,
        })
    }

    /// Durable path of this session's event log.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sequence assigned to the next appended event.
    #[must_use]
    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Last persisted sequence, distinguishing an empty log from event zero.
    #[must_use]
    pub const fn last_sequence(&self) -> Option<SequenceId> {
        if self.next_sequence == 0 {
            None
        } else {
            Some(SequenceId(self.next_sequence - 1))
        }
    }

    /// Appends one event and synchronizes it before returning.
    ///
    /// # Errors
    ///
    /// Returns a serialization, sequence-overflow, or durable-write error.
    pub fn append<T: Serialize>(
        &mut self,
        event: T,
    ) -> Result<EventEnvelope<T>, SessionStoreError> {
        let mut appended = self.append_batch([event])?;
        appended.pop().ok_or(SessionStoreError::CorruptEvent(
            "append produced no persisted event",
        ))
    }

    /// Appends only when a caller's expected id is exactly the next id.
    ///
    /// This is the fail-closed adapter for engines which allocate the id before
    /// persistence. Prefer [`Self::append`] and broadcast its returned envelope
    /// when the log can be the sole authority.
    ///
    /// # Errors
    ///
    /// Returns a sequence mismatch before serializing or writing any bytes.
    pub fn append_expected<T: Serialize>(
        &mut self,
        expected: SequenceId,
        event: T,
    ) -> Result<EventEnvelope<T>, SessionStoreError> {
        if expected != SequenceId(self.next_sequence) {
            return Err(SessionStoreError::UnexpectedEventSequence {
                expected: SequenceId(self.next_sequence),
                actual: expected,
            });
        }
        self.append(event)
    }

    /// Serializes a batch before writing, then appends it under the existing
    /// writer lock and performs one durable synchronization. A killed partial
    /// tail is removed on the next open.
    ///
    /// # Errors
    ///
    /// Returns a serialization, sequence-overflow, or durable-write error.
    pub fn append_batch<T: Serialize>(
        &mut self,
        events: impl IntoIterator<Item = T>,
    ) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        let events = events.into_iter().collect::<Vec<_>>();
        let count = u64::try_from(events.len()).map_err(|_| SessionStoreError::SequenceOverflow)?;
        self.next_sequence
            .checked_add(count)
            .ok_or(SessionStoreError::SequenceOverflow)?;
        let mut bytes = Vec::new();
        let mut envelopes = Vec::with_capacity(events.len());
        for (offset, event) in events.into_iter().enumerate() {
            let offset = u64::try_from(offset).map_err(|_| SessionStoreError::SequenceOverflow)?;
            let envelope = EventEnvelope {
                schema_version: EVENT_SCHEMA_VERSION,
                sequence: SequenceId(
                    self.next_sequence
                        .checked_add(offset)
                        .ok_or(SessionStoreError::SequenceOverflow)?,
                ),
                event,
            };
            serde_json::to_writer(&mut bytes, &envelope)?;
            bytes.push(b'\n');
            envelopes.push(envelope);
        }
        if bytes.is_empty() {
            return Ok(envelopes);
        }
        self.file.write_all(&bytes)?;
        self.file.flush()?;
        sync_event_file(&self.file)?;
        self.next_sequence += count;
        Ok(envelopes)
    }

    /// Backward-compatible name for appending one durably synchronized turn.
    ///
    /// # Errors
    ///
    /// Returns a serialization, sequence-overflow, or durable-write error.
    pub fn append_turn<T: Serialize>(
        &mut self,
        events: impl IntoIterator<Item = T>,
    ) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        self.append_batch(events)
    }

    /// Loads every complete event after validating version and sequence.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O, JSON, schema, or sequence corruption.
    pub fn load<T: DeserializeOwned>(&self) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        load_events(&self.file)
    }
}

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

    /// Recreates a missing or corrupt derived database from authoritative logs.
    ///
    /// A temporary database is fully populated and synchronized before it
    /// replaces the old index. No event log is modified.
    ///
    /// # Errors
    ///
    /// Returns an invalid projection, I/O, or `SQLite` error.
    pub fn rebuild(
        root: &Path,
        projections: &[SessionProjection],
    ) -> Result<Self, SessionStoreError> {
        fs::create_dir_all(root)?;
        for projection in projections {
            validate_session_id(&projection.summary.id)?;
        }
        let temporary = root.join(format!(
            ".index-{}-{}.sqlite",
            std::process::id(),
            INDEX_TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        remove_if_exists(&temporary)?;
        let temporary_index = Self {
            path: temporary.clone(),
        };
        remove_if_exists(&sidecar_path(&temporary, "-wal"))?;
        remove_if_exists(&sidecar_path(&temporary, "-shm"))?;
        temporary_index.replace_all(projections)?;
        {
            let connection = temporary_index.connection()?;
            connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        }
        remove_if_exists(&sidecar_path(&temporary, "-wal"))?;
        remove_if_exists(&sidecar_path(&temporary, "-shm"))?;
        let final_path = root.join("index.sqlite");
        remove_if_exists(&sidecar_path(&final_path, "-wal"))?;
        remove_if_exists(&sidecar_path(&final_path, "-shm"))?;
        remove_if_exists(&final_path)?;
        fs::rename(&temporary, &final_path)?;
        File::open(root)?.sync_all()?;
        let rebuilt = Self { path: final_path };
        rebuilt.connection()?;
        Ok(rebuilt)
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
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS sessions(
               id TEXT NOT NULL UNIQUE,
               title TEXT NOT NULL,
               updated_unix_ms INTEGER NOT NULL,
               cost_micros INTEGER NOT NULL,
               transcript TEXT NOT NULL,
               projected_sequence TEXT
             );",
        )?;
        ensure_projection_column(&connection)?;
        connection.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS sessions_fts USING fts5(
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
             END;",
        )?;
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
            "SELECT id,title,updated_unix_ms,cost_micros FROM sessions \
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
        let mut statement = connection.prepare(
            "SELECT s.id,s.title,s.updated_unix_ms,s.cost_micros \
             FROM sessions_fts f JOIN sessions s ON s.rowid=f.rowid \
             WHERE sessions_fts MATCH ?1 \
             ORDER BY bm25(sessions_fts),s.updated_unix_ms DESC,s.id ASC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![query, limit], summary_from_row)?;
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
                "SELECT id,title,updated_unix_ms,cost_micros FROM sessions WHERE id=?1",
                [id],
                summary_from_row,
            )
            .optional()
            .map_err(Into::into)
    }
}

fn summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionSummary> {
    Ok(SessionSummary {
        id: row.get(0)?,
        title: row.get(1)?,
        updated_unix_ms: row.get(2)?,
        cost_micros: row.get(3)?,
    })
}

fn upsert_projection(
    connection: &Connection,
    projection: &SessionProjection,
) -> Result<(), SessionStoreError> {
    connection.execute(
        "INSERT INTO sessions(\
           id,title,updated_unix_ms,cost_micros,transcript,projected_sequence\
         ) VALUES (?1,?2,?3,?4,?5,?6) \
         ON CONFLICT(id) DO UPDATE SET title=excluded.title, \
         updated_unix_ms=excluded.updated_unix_ms, \
         cost_micros=excluded.cost_micros, transcript=excluded.transcript, \
         projected_sequence=excluded.projected_sequence",
        params![
            projection.summary.id,
            projection.summary.title,
            projection.summary.updated_unix_ms,
            projection.summary.cost_micros,
            projection.transcript,
            projection
                .projected_through
                .map_or_else(|| "-".to_owned(), |sequence| sequence.0.to_string())
        ],
    )?;
    Ok(())
}

fn ensure_projection_column(connection: &Connection) -> Result<(), SessionStoreError> {
    let mut statement = connection.prepare("PRAGMA table_info(sessions)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|column| column == "projected_sequence") {
        connection.execute(
            "ALTER TABLE sessions ADD COLUMN projected_sequence TEXT",
            [],
        )?;
    }
    Ok(())
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn remove_if_exists(path: &Path) -> Result<(), SessionStoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn recover_and_validate(file: &File) -> Result<u64, SessionStoreError> {
    let bytes = read_opened_file(file)?;
    let complete_len = if bytes.last().is_none_or(|byte| *byte == b'\n') {
        bytes.len()
    } else {
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |position| position + 1)
    };
    if complete_len != bytes.len() {
        file.set_len(u64::try_from(complete_len).map_err(|_| SessionStoreError::LimitOverflow)?)?;
        sync_event_file(file)?;
    }
    let events = parse_events::<serde_json::Value>(&bytes[..complete_len])?;
    u64::try_from(events.len()).map_err(|_| SessionStoreError::SequenceOverflow)
}

fn load_events<T: DeserializeOwned>(
    file: &File,
) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
    let bytes = read_opened_file(file)?;
    parse_events(&bytes)
}

fn parse_events<T: DeserializeOwned>(
    bytes: &[u8],
) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
    let mut events = Vec::new();
    for line in BufReader::new(bytes).lines() {
        let line = line?;
        if line.is_empty() {
            return Err(SessionStoreError::CorruptEvent("blank JSONL record"));
        }
        let envelope: EventEnvelope<T> = serde_json::from_str(&line)?;
        if envelope.schema_version != EVENT_SCHEMA_VERSION {
            return Err(SessionStoreError::UnsupportedEventVersion(
                envelope.schema_version,
            ));
        }
        let expected =
            u64::try_from(events.len()).map_err(|_| SessionStoreError::SequenceOverflow)?;
        if envelope.sequence != SequenceId(expected) {
            return Err(SessionStoreError::CorruptEvent(
                "non-contiguous event sequence",
            ));
        }
        events.push(envelope);
    }
    Ok(events)
}

#[cfg(unix)]
fn read_opened_file(file: &File) -> Result<Vec<u8>, SessionStoreError> {
    let stat = rustix::fs::fstat(file).map_err(std::io::Error::from)?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(SessionStoreError::UnsafeEventFileType);
    }
    let length = usize::try_from(stat.st_size).map_err(|_| SessionStoreError::LimitOverflow)?;
    let mut bytes = vec![0_u8; length];
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let position = u64::try_from(offset).map_err(|_| SessionStoreError::LimitOverflow)?;
        let read = file.read_at(&mut bytes[offset..], position)?;
        if read == 0 {
            bytes.truncate(offset);
            break;
        }
        offset = offset
            .checked_add(read)
            .ok_or(SessionStoreError::LimitOverflow)?;
    }
    let after = rustix::fs::fstat(file).map_err(std::io::Error::from)?;
    if after.st_dev != stat.st_dev || after.st_ino != stat.st_ino || after.st_size != stat.st_size {
        return Err(SessionStoreError::EventFileChangedDuringRead);
    }
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_opened_file(file: &File) -> Result<Vec<u8>, SessionStoreError> {
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn ensure_regular_file(file: &File) -> Result<(), SessionStoreError> {
    if file.metadata()?.file_type().is_file() {
        Ok(())
    } else {
        Err(SessionStoreError::UnsafeEventFileType)
    }
}

#[cfg(unix)]
fn open_session_file(root: &Path, session_id: &str) -> Result<File, SessionStoreError> {
    fs::create_dir_all(root)?;
    let root = File::open(root)?;
    if !root.metadata()?.is_dir() {
        return Err(SessionStoreError::UnsafeSessionDirectory);
    }
    let sessions = open_or_create_directory(&root, "sessions")?;
    let session = open_or_create_directory(&sessions, session_id)?;
    open_or_create_event_file(&session)
}

#[cfg(unix)]
fn open_or_create_directory(parent: &File, name: &str) -> Result<File, SessionStoreError> {
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::CLOEXEC
        | rustix::fs::OFlags::NOFOLLOW;
    match rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty()) {
        Ok(descriptor) => Ok(File::from(descriptor)),
        Err(rustix::io::Errno::NOENT) => {
            match rustix::fs::mkdirat(parent, name, rustix::fs::Mode::from_bits_truncate(0o700)) {
                Ok(()) => sync_event_file(parent)?,
                Err(rustix::io::Errno::EXIST) => {}
                Err(source) => return Err(std::io::Error::from(source).into()),
            }
            let descriptor = rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty())
                .map_err(std::io::Error::from)?;
            Ok(File::from(descriptor))
        }
        Err(source) => Err(std::io::Error::from(source).into()),
    }
}

#[cfg(unix)]
fn open_or_create_event_file(parent: &File) -> Result<File, SessionStoreError> {
    let flags = rustix::fs::OFlags::RDWR
        | rustix::fs::OFlags::APPEND
        | rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::CLOEXEC
        | rustix::fs::OFlags::NOFOLLOW;
    match rustix::fs::openat(parent, "events.jsonl", flags, rustix::fs::Mode::empty()) {
        Ok(descriptor) => Ok(File::from(descriptor)),
        Err(rustix::io::Errno::NOENT) => {
            let created = rustix::fs::openat(
                parent,
                "events.jsonl",
                flags | rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL,
                rustix::fs::Mode::from_bits_truncate(0o600),
            );
            match created {
                Ok(descriptor) => {
                    let file = File::from(descriptor);
                    sync_event_file(&file)?;
                    sync_event_file(parent)?;
                    Ok(file)
                }
                Err(rustix::io::Errno::EXIST) => {
                    let descriptor = rustix::fs::openat(
                        parent,
                        "events.jsonl",
                        flags,
                        rustix::fs::Mode::empty(),
                    )
                    .map_err(std::io::Error::from)?;
                    Ok(File::from(descriptor))
                }
                Err(source) => Err(std::io::Error::from(source).into()),
            }
        }
        Err(source) => Err(std::io::Error::from(source).into()),
    }
}

#[cfg(not(unix))]
fn open_session_file_portable(
    root: &Path,
    directory: &Path,
    path: &Path,
) -> Result<File, SessionStoreError> {
    create_checked_directory_portable(root)?;
    create_checked_directory_portable(&root.join("sessions"))?;
    create_checked_directory_portable(directory)?;
    let file = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            OpenOptions::new().read(true).append(true).open(path)?
        }
        Ok(_) => return Err(SessionStoreError::UnsafeEventFileType),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => OpenOptions::new()
            .create_new(true)
            .read(true)
            .append(true)
            .open(path)?,
        Err(source) => return Err(source.into()),
    };
    sync_event_file(&file)?;
    Ok(file)
}

#[cfg(not(unix))]
fn create_checked_directory_portable(path: &Path) -> Result<(), SessionStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(SessionStoreError::UnsafeSessionDirectory),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            Ok(())
        }
        Err(source) => Err(source.into()),
    }
}

#[cfg(unix)]
fn sync_event_file(file: &File) -> std::io::Result<()> {
    rustix::fs::fsync(file).map_err(std::io::Error::from)
}

#[cfg(not(unix))]
fn sync_event_file(file: &File) -> std::io::Result<()> {
    file.sync_all()
}

#[cfg(unix)]
fn lock_writer(file: &File) -> Result<(), SessionStoreError> {
    const MAX_TRANSIENT_RETRIES: usize = 20;
    for attempt in 0..=MAX_TRANSIENT_RETRIES {
        match rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(()),
            Err(source)
                if source.kind() == std::io::ErrorKind::WouldBlock
                    && attempt < MAX_TRANSIENT_RETRIES =>
            {
                // A concurrent fork can briefly inherit the old writer's descriptor before
                // CLOEXEC closes it. Bound the retry so a real second writer still fails closed.
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(source) => return Err(std::io::Error::from(source).into()),
        }
    }
    unreachable!("bounded writer-lock loop always returns")
}

#[cfg(not(unix))]
fn lock_writer(_file: &File) -> Result<(), SessionStoreError> {
    Ok(())
}

fn validate_session_id(value: &str) -> Result<(), SessionStoreError> {
    if value.is_empty()
        || value.len() > 128
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(SessionStoreError::InvalidSessionId);
    }
    Ok(())
}

/// Session log/index failure without transcript contents in diagnostics.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    /// Session ids are path components and must use the restricted alphabet.
    #[error("session id is empty, too long, or contains unsafe characters")]
    InvalidSessionId,
    /// Session storage components must be real directories, not links or special files.
    #[error("session storage contains an unsafe directory component")]
    UnsafeSessionDirectory,
    /// The authoritative log must be a regular, non-symlink file.
    #[error("session event log is not a regular file")]
    UnsafeEventFileType,
    /// An unlocked external writer changed the log during a descriptor-stable read.
    #[error("session event log changed while it was being read")]
    EventFileChangedDuringRead,
    /// A complete JSONL record was structurally corrupt.
    #[error("session event log is corrupt: {0}")]
    CorruptEvent(&'static str),
    /// A derived index row stored a malformed decimal watermark.
    #[error("session index projection watermark is corrupt")]
    CorruptProjectionWatermark,
    /// A pre-sequenced event did not match the durable log tail.
    #[error("session event sequence mismatch: expected {expected:?}, got {actual:?}")]
    UnexpectedEventSequence {
        /// Sequence which could be safely appended.
        expected: SequenceId,
        /// Sequence supplied by the caller.
        actual: SequenceId,
    },
    /// The reader does not understand this schema version.
    #[error("unsupported session event schema version {0}")]
    UnsupportedEventVersion(u16),
    /// Event sequence cannot be represented.
    #[error("session event sequence overflow")]
    SequenceOverflow,
    /// Caller-supplied query limit cannot be represented by `SQLite`.
    #[error("session query limit overflow")]
    LimitOverflow,
    /// Filesystem failure.
    #[error("session storage I/O failed")]
    Io(#[from] std::io::Error),
    /// JSON failure. Payload contents are intentionally omitted.
    #[error("session event JSON is invalid")]
    Json(#[from] serde_json::Error),
    /// `SQLite` failure. `SQLite`'s structural diagnostic is retained.
    #[error("session index failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize, Serializer, ser::Error as _};
    use tempfile::tempdir;

    use rw_types::SequenceId;

    use super::{
        EventEnvelope, ProjectionStatus, SessionEventLog, SessionIndex, SessionProjection,
        SessionSummary,
    };

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    struct FixtureEvent {
        kind: String,
        text: String,
    }

    struct FailableEvent {
        text: &'static str,
        fail: bool,
    }

    impl Serialize for FailableEvent {
        fn serialize<SerializerType>(
            &self,
            serializer: SerializerType,
        ) -> Result<SerializerType::Ok, SerializerType::Error>
        where
            SerializerType: Serializer,
        {
            if self.fail {
                Err(SerializerType::Error::custom(
                    "fixture serialization failure",
                ))
            } else {
                serializer.serialize_str(self.text)
            }
        }
    }

    #[test]
    fn killed_partial_tail_is_truncated_and_sequence_resumes() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let mut log = SessionEventLog::open(root.path(), "session-1")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        log.append(FixtureEvent {
            kind: "user".to_owned(),
            text: "complete".to_owned(),
        })
        .unwrap_or_else(|error| panic!("event must append: {error}"));
        let mut file = OpenOptions::new()
            .append(true)
            .open(log.path())
            .unwrap_or_else(|error| panic!("tail file must open: {error}"));
        file.write_all(br#"{"schema_version":1,"sequence":1,"event":{"kind":"assistant""#)
            .unwrap_or_else(|error| panic!("partial tail must write: {error}"));
        file.sync_data()
            .unwrap_or_else(|error| panic!("partial tail must sync: {error}"));
        drop(file);
        drop(log);

        let mut recovered = SessionEventLog::open(root.path(), "session-1")
            .unwrap_or_else(|error| panic!("partial tail must recover: {error}"));
        assert_eq!(recovered.next_sequence(), 1);
        recovered
            .append(FixtureEvent {
                kind: "assistant".to_owned(),
                text: "resumed".to_owned(),
            })
            .unwrap_or_else(|error| panic!("resumed event must append: {error}"));
        let events = recovered
            .load::<FixtureEvent>()
            .unwrap_or_else(|error| panic!("events must load: {error}"));
        assert_eq!(
            events,
            vec![
                EventEnvelope {
                    schema_version: 1,
                    sequence: SequenceId(0),
                    event: FixtureEvent {
                        kind: "user".to_owned(),
                        text: "complete".to_owned(),
                    },
                },
                EventEnvelope {
                    schema_version: 1,
                    sequence: SequenceId(1),
                    event: FixtureEvent {
                        kind: "assistant".to_owned(),
                        text: "resumed".to_owned(),
                    },
                },
            ]
        );
    }

    #[test]
    fn batch_append_assigns_exact_envelopes_and_serialization_is_all_or_nothing() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let mut log = SessionEventLog::open(root.path(), "batch")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        let appended = log
            .append_batch([
                FixtureEvent {
                    kind: "turn_started".to_owned(),
                    text: "one".to_owned(),
                },
                FixtureEvent {
                    kind: "user_message".to_owned(),
                    text: "two".to_owned(),
                },
            ])
            .unwrap_or_else(|error| panic!("batch must append: {error}"));
        assert_eq!(
            appended
                .iter()
                .map(|envelope| envelope.sequence)
                .collect::<Vec<_>>(),
            vec![SequenceId(0), SequenceId(1)]
        );
        assert_eq!(
            log.load::<FixtureEvent>()
                .unwrap_or_else(|error| panic!("batch must load: {error}")),
            appended
        );

        let before = std::fs::read(log.path())
            .unwrap_or_else(|error| panic!("batch log must read: {error}"));
        assert!(
            log.append_batch([
                FailableEvent {
                    text: "serializable",
                    fail: false,
                },
                FailableEvent {
                    text: "fails",
                    fail: true,
                },
            ])
            .is_err()
        );
        assert_eq!(log.next_sequence(), 2);
        assert_eq!(
            std::fs::read(log.path())
                .unwrap_or_else(|error| panic!("batch log must reread: {error}")),
            before
        );
    }

    #[test]
    fn five_hundred_event_batch_round_trips_contiguously() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let mut log = SessionEventLog::open(root.path(), "five-hundred")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        let events = (0..500).map(|index| FixtureEvent {
            kind: "sample".to_owned(),
            text: index.to_string(),
        });
        let appended = log
            .append_batch(events)
            .unwrap_or_else(|error| panic!("batch must append: {error}"));
        assert_eq!(appended.len(), 500);
        assert_eq!(
            appended.last().map(|event| event.sequence),
            Some(SequenceId(499))
        );
        assert_eq!(
            log.load::<FixtureEvent>()
                .unwrap_or_else(|error| panic!("batch must load: {error}")),
            appended
        );
    }

    #[test]
    fn sqlite_index_lists_updates_and_searches_transcripts() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let index = SessionIndex::open(root.path())
            .unwrap_or_else(|error| panic!("index must open: {error}"));
        let first = SessionSummary {
            id: "first".to_owned(),
            title: "Rust parser".to_owned(),
            updated_unix_ms: 10,
            cost_micros: 7,
        };
        let second = SessionSummary {
            id: "second".to_owned(),
            title: "TypeScript UI".to_owned(),
            updated_unix_ms: 20,
            cost_micros: 9,
        };
        index
            .upsert(&SessionProjection {
                summary: first.clone(),
                transcript: "implemented a resilient parser".to_owned(),
                projected_through: Some(SequenceId(3)),
            })
            .unwrap_or_else(|error| panic!("first index row must write: {error}"));
        index
            .upsert(&SessionProjection {
                summary: second.clone(),
                transcript: "rendered terminal cells".to_owned(),
                projected_through: Some(SequenceId(8)),
            })
            .unwrap_or_else(|error| panic!("second index row must write: {error}"));
        assert_eq!(
            index
                .list(10)
                .unwrap_or_else(|error| panic!("sessions must list: {error}")),
            vec![second.clone(), first.clone()]
        );
        assert_eq!(
            index
                .search("resilient", 10)
                .unwrap_or_else(|error| panic!("sessions must search: {error}")),
            vec![first.clone()]
        );
        let updated = SessionSummary {
            title: "Rust event parser".to_owned(),
            updated_unix_ms: 30,
            cost_micros: 11,
            ..first
        };
        index
            .upsert(&SessionProjection {
                summary: updated.clone(),
                transcript: "recovered a truncated transcript".to_owned(),
                projected_through: Some(SequenceId(11)),
            })
            .unwrap_or_else(|error| panic!("updated row must write: {error}"));
        assert!(
            index
                .search("resilient", 10)
                .unwrap_or_else(|error| panic!("old search must work: {error}"))
                .is_empty()
        );
        assert_eq!(
            index
                .search("truncated", 10)
                .unwrap_or_else(|error| panic!("updated search must work: {error}")),
            vec![updated]
        );
        assert_eq!(
            index
                .projection_status("first", Some(SequenceId(11)))
                .unwrap_or_else(|error| panic!("watermark must query: {error}")),
            ProjectionStatus::Current
        );
        assert!(matches!(
            index
                .projection_status("first", Some(SequenceId(12)))
                .unwrap_or_else(|error| panic!("stale watermark must query: {error}")),
            ProjectionStatus::Stale { .. }
        ));
    }

    #[test]
    fn event_envelope_tolerates_additive_fields() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let log = SessionEventLog::open(root.path(), "future-fields")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        std::fs::write(
            log.path(),
            br#"{"schema_version":1,"sequence":"0","event":{"kind":"user","text":"hello","future_event_field":true},"future_envelope_field":{"nested":1}}
"#,
        )
        .unwrap_or_else(|error| panic!("future fixture must write: {error}"));
        let events = log
            .load::<FixtureEvent>()
            .unwrap_or_else(|error| panic!("additive fields must load: {error}"));
        assert_eq!(events[0].event.text, "hello");
    }

    #[test]
    fn persisted_sequence_is_authoritative_decimal_string() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let mut log = SessionEventLog::open(root.path(), "sequence")
            .unwrap_or_else(|error| panic!("log must open: {error}"));
        assert_eq!(log.last_sequence(), None);
        let persisted = log
            .append(FixtureEvent {
                kind: "user".to_owned(),
                text: "hello".to_owned(),
            })
            .unwrap_or_else(|error| panic!("event must persist: {error}"));
        assert_eq!(persisted.sequence, SequenceId(0));
        let raw = std::fs::read_to_string(log.path())
            .unwrap_or_else(|error| panic!("log must read: {error}"));
        assert!(raw.contains("\"sequence\":\"0\""));
        assert!(!raw.contains("\"sequence\":0"));
        assert_eq!(log.last_sequence(), Some(SequenceId(0)));
        let before = raw;
        assert!(
            log.append_expected(
                SequenceId(2),
                FixtureEvent {
                    kind: "assistant".to_owned(),
                    text: "must not write".to_owned(),
                }
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(log.path())
                .unwrap_or_else(|error| panic!("unchanged log must read: {error}")),
            before
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_session_log_has_one_process_writer() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let log = SessionEventLog::open(root.path(), "single-writer")
            .unwrap_or_else(|error| panic!("first writer must open: {error}"));
        assert!(SessionEventLog::open(root.path(), "single-writer").is_err());
        drop(log);
        SessionEventLog::open(root.path(), "single-writer")
            .unwrap_or_else(|error| panic!("writer lock must release: {error:?}"));
    }

    #[cfg(unix)]
    #[test]
    fn session_log_rejects_symlink_escape_components() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let outside = tempdir().unwrap_or_else(|error| panic!("outside must create: {error}"));
        std::fs::create_dir_all(root.path().join("sessions/file-link"))
            .unwrap_or_else(|error| panic!("session directory must create: {error}"));
        let outside_log = outside.path().join("outside.jsonl");
        std::fs::write(&outside_log, b"outside")
            .unwrap_or_else(|error| panic!("outside log must write: {error}"));
        symlink(
            &outside_log,
            root.path().join("sessions/file-link/events.jsonl"),
        )
        .unwrap_or_else(|error| panic!("event symlink must create: {error}"));
        assert!(SessionEventLog::open(root.path(), "file-link").is_err());
        assert_eq!(
            std::fs::read(&outside_log)
                .unwrap_or_else(|error| panic!("outside log must read: {error}")),
            b"outside"
        );

        symlink(outside.path(), root.path().join("sessions/directory-link"))
            .unwrap_or_else(|error| panic!("directory symlink must create: {error}"));
        assert!(SessionEventLog::open(root.path(), "directory-link").is_err());
        assert!(!outside.path().join("events.jsonl").exists());
    }

    #[cfg(unix)]
    #[test]
    fn session_log_fifo_child() {
        let Some(root) = std::env::var_os("ROTTWEILER_TEST_FIFO_SESSION_ROOT") else {
            return;
        };
        let result = SessionEventLog::open(std::path::Path::new(&root), "fifo");
        assert!(matches!(
            result,
            Err(super::SessionStoreError::UnsafeEventFileType)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_event_log_is_rejected_in_a_non_hanging_subprocess() {
        use std::{process::Command, thread, time::Duration};

        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let directory = root.path().join("sessions/fifo");
        std::fs::create_dir_all(&directory)
            .unwrap_or_else(|error| panic!("session directory must create: {error}"));
        assert!(
            Command::new("mkfifo")
                .arg(directory.join("events.jsonl"))
                .status()
                .unwrap_or_else(|error| panic!("mkfifo must run: {error}"))
                .success()
        );
        let executable = std::env::current_exe()
            .unwrap_or_else(|error| panic!("test executable must resolve: {error}"));
        let mut child = Command::new(executable)
            .arg("--exact")
            .arg("session::tests::session_log_fifo_child")
            .arg("--nocapture")
            .env("ROTTWEILER_TEST_FIFO_SESSION_ROOT", root.path())
            .spawn()
            .unwrap_or_else(|error| panic!("FIFO test child must spawn: {error}"));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = child
                .try_wait()
                .unwrap_or_else(|error| panic!("FIFO test child must poll: {error}"))
            {
                assert!(status.success(), "FIFO test child failed: {status}");
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("opening a FIFO event log blocked for more than three seconds");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn derived_index_rebuild_replaces_stale_rows() {
        let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
        let stale = SessionProjection {
            summary: SessionSummary {
                id: "stale".to_owned(),
                title: "old".to_owned(),
                updated_unix_ms: 1,
                cost_micros: 0,
            },
            transcript: "obsolete".to_owned(),
            projected_through: Some(SequenceId(0)),
        };
        SessionIndex::open(root.path())
            .and_then(|index| index.upsert(&stale))
            .unwrap_or_else(|error| panic!("stale row must write: {error}"));
        let current = SessionProjection {
            summary: SessionSummary {
                id: "current".to_owned(),
                title: "new".to_owned(),
                updated_unix_ms: 2,
                cost_micros: 1,
            },
            transcript: "authoritative".to_owned(),
            projected_through: Some(SequenceId(u64::MAX)),
        };
        let rebuilt = SessionIndex::rebuild(root.path(), std::slice::from_ref(&current))
            .unwrap_or_else(|error| panic!("index must rebuild: {error}"));
        assert!(rebuilt.get("stale").unwrap_or(None).is_none());
        assert_eq!(
            rebuilt.get("current").unwrap_or(None),
            Some(current.summary)
        );
        assert_eq!(
            rebuilt
                .projection_status("current", Some(SequenceId(u64::MAX)))
                .unwrap_or_else(|error| panic!("watermark must survive: {error}")),
            ProjectionStatus::Current
        );
    }

    use std::{fs::OpenOptions, io::Write};
}
