//! Journal append facade, explicit exports/forks, and empty-session collection.
use super::{
    EventEnvelope, SessionEventPage, SessionEventPageLimits, SessionStoreError, journal,
    journal_io::validate_session_id,
};
use rw_types::SequenceId;
use serde::{Serialize, de::DeserializeOwned};
use std::{fs, path::Path};

/// Append owner for a session's authoritative segmented journal.
#[derive(Debug)]
pub struct SessionEventLog {
    journal: journal::SegmentedJournal,
}

impl SessionEventLog {
    /// Opens or recovers the journal under its exclusive writer lock.
    ///
    /// # Errors
    /// Rejects unsafe paths, concurrent writers and corrupt active records.
    pub fn open(root: &Path, session_id: &str) -> Result<Self, SessionStoreError> {
        journal::SegmentedJournal::open(root, session_id).map(|journal| Self { journal })
    }

    /// Physical journal directory; the layout is owned by the store.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.journal.path()
    }

    /// Next durable envelope sequence.
    #[must_use]
    pub const fn next_sequence(&self) -> u64 {
        self.journal.next_sequence()
    }

    /// Last acknowledged sequence, or none for an empty journal.
    #[must_use]
    pub fn last_sequence(&self) -> Option<SequenceId> {
        self.journal.read_view().last_sequence()
    }

    /// Captures the acknowledged prefix without reading journal payloads.
    #[must_use]
    pub fn read_view(&self) -> journal::JournalReadView {
        self.journal.read_view()
    }

    /// Appends and synchronizes one durable event.
    ///
    /// # Errors
    /// Propagates serialization, identity, capacity and durable write failures.
    pub fn append<T: Serialize>(
        &mut self,
        event: T,
    ) -> Result<EventEnvelope<T>, SessionStoreError> {
        self.append_batch([event])?
            .pop()
            .ok_or(SessionStoreError::CorruptEvent("empty append"))
    }

    /// Appends only at the caller's expected next sequence.
    ///
    /// # Errors
    /// Rejects a mismatched sequence before serialization or writing.
    pub fn append_expected<T: Serialize>(
        &mut self,
        expected: SequenceId,
        event: T,
    ) -> Result<EventEnvelope<T>, SessionStoreError> {
        if expected.0 != self.next_sequence() {
            return Err(SessionStoreError::UnexpectedEventSequence {
                expected: SequenceId(self.next_sequence()),
                actual: expected,
            });
        }
        self.append(event)
    }

    /// Appends a bounded batch with one event synchronization.
    ///
    /// # Errors
    /// Propagates serialization, capacity, identity and durable write failures.
    pub fn append_batch<T: Serialize>(
        &mut self,
        events: impl IntoIterator<Item = T>,
    ) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        self.journal.append_batch(events)
    }

    /// Appends canonical bytes at their expected sequence with one durable sync.
    ///
    /// # Errors
    /// Rejects changed prefixes, unsafe descriptors, and failed durable I/O.
    pub fn append_prepared(
        &mut self,
        prepared: journal::PreparedJournalAppend,
    ) -> Result<(), SessionStoreError> {
        self.journal.append_prepared(prepared)
    }

    /// Loads a small bounded journal; streaming consumers use `read_view`.
    ///
    /// # Errors
    /// Rejects journals exceeding 512 MiB or one million events and corruption.
    pub fn load<T: DeserializeOwned>(&self) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        self.read_view()
            .collect_bounded(512 * 1024 * 1024, 1_000_000)
    }

    fn existing_view(
        root: &Path,
        session_id: &str,
    ) -> Result<journal::JournalReadView, SessionStoreError> {
        journal::JournalReadView::open_existing(root, session_id)?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "session journal does not exist",
            )
            .into()
        })
    }

    /// Loads an offline journal under the default aggregate bounds.
    ///
    /// # Errors
    /// Rejects live ownership, corruption and excessive aggregate output.
    pub fn load_existing<T: DeserializeOwned>(
        root: &Path,
        session_id: &str,
    ) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        Self::load_existing_bounded(root, session_id, 512 * 1024 * 1024, 1_000_000)
    }

    /// Loads an offline journal under explicit aggregate output limits.
    ///
    /// # Errors
    /// Rejects live ownership, corruption and excessive aggregate output.
    pub fn load_existing_bounded<T: DeserializeOwned>(
        root: &Path,
        session_id: &str,
        max_bytes: u64,
        max_events: usize,
    ) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        Self::existing_view(root, session_id)?.collect_bounded(max_bytes, max_events)
    }

    /// Loads an offline journal and its exact pinned byte length.
    ///
    /// # Errors
    /// Rejects live ownership, corruption and excessive aggregate output.
    pub fn load_existing_bounded_with_size<T: DeserializeOwned>(
        root: &Path,
        session_id: &str,
        max_bytes: u64,
        max_events: usize,
    ) -> Result<(Vec<EventEnvelope<T>>, u64), SessionStoreError> {
        let view = Self::existing_view(root, session_id)?;
        Ok((
            view.collect_bounded(max_bytes, max_events)?,
            view.total_bytes(),
        ))
    }

    /// Reads a bounded cursor-exclusive page from an offline journal.
    ///
    /// # Errors
    /// Rejects live ownership, invalid limits/cursors and referenced corruption.
    pub fn load_existing_page<T: DeserializeOwned>(
        root: &Path,
        session_id: &str,
        after: Option<SequenceId>,
        limits: SessionEventPageLimits,
    ) -> Result<SessionEventPage<T>, SessionStoreError> {
        Self::existing_view(root, session_id)?.page(after, limits)
    }

    /// Reads the most recent bounded page from an offline journal.
    ///
    /// # Errors
    /// Rejects live ownership, invalid limits and referenced corruption.
    pub fn load_existing_tail_page<T: DeserializeOwned>(
        root: &Path,
        session_id: &str,
        limits: SessionEventPageLimits,
    ) -> Result<SessionEventPage<T>, SessionStoreError> {
        Self::existing_view(root, session_id)?.tail_page(limits)
    }

    /// Copies an exact bounded parent prefix into an idempotent child journal.
    ///
    /// # Errors
    /// Rejects missing cursors, conflicting identities, divergent children and I/O.
    pub fn fork(
        root: &Path,
        parent_session_id: &str,
        child_session_id: &str,
        through_sequence: Option<SequenceId>,
    ) -> Result<Self, SessionStoreError> {
        Self::fork_mapped::<serde_json::Value, _>(
            root,
            parent_session_id,
            child_session_id,
            through_sequence,
            Ok,
        )
    }

    /// Typed fork primitive which can rewrite payload-owned session identity
    /// while preserving envelope sequence and an exact durable prefix.
    ///
    /// `None` means the explicit empty prefix. Callers for a non-empty fork
    /// must resolve and pass the exact durable boundary sequence.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identities, a missing source boundary,
    /// conflicting child history, mapping failure, or durable storage I/O.
    pub fn fork_mapped<T, Map>(
        root: &Path,
        parent_session_id: &str,
        child_session_id: &str,
        through_sequence: Option<SequenceId>,
        map: Map,
    ) -> Result<Self, SessionStoreError>
    where
        T: DeserializeOwned + PartialEq + Serialize,
        Map: FnMut(T) -> Result<T, SessionStoreError>,
    {
        let parent = Self::existing_view(root, parent_session_id)?;
        Self::fork_mapped_view(
            root,
            parent_session_id,
            child_session_id,
            &parent,
            through_sequence,
            map,
        )
    }

    /// Copies a pinned parent prefix in bounded pages, resuming an identical child.
    /// A failed write or mapping can leave a matching partial child for retry.
    ///
    /// # Errors
    /// Rejects invalid identities/cursors, mapping failures, an incompatible
    /// existing child prefix, or durable read/write errors.
    pub fn fork_mapped_view<T, Map>(
        root: &Path,
        parent_session_id: &str,
        child_session_id: &str,
        parent: &journal::JournalReadView,
        through_sequence: Option<SequenceId>,
        mut map: Map,
    ) -> Result<Self, SessionStoreError>
    where
        T: DeserializeOwned + PartialEq + Serialize,
        Map: FnMut(T) -> Result<T, SessionStoreError>,
    {
        validate_session_id(parent_session_id)?;
        validate_session_id(child_session_id)?;
        if parent_session_id == child_session_id {
            return Err(SessionStoreError::ForkIdentityConflict);
        }
        if through_sequence
            .is_some_and(|through| parent.last_sequence().is_none_or(|tail| through > tail))
        {
            return Err(SessionStoreError::ForkSourceCursorMissing);
        }
        journal::JournalRoot::open(root)?.validate_view(parent_session_id, parent)?;
        let mut child = Self::open(root, child_session_id)?;
        let existing = child.read_view();
        if existing
            .last_sequence()
            .is_some_and(|tail| through_sequence.is_none_or(|through| tail > through))
        {
            return Err(SessionStoreError::ForkTargetConflict);
        }
        let limits = SessionEventPageLimits {
            max_page_events: 256,
            ..SessionEventPageLimits::default()
        };
        let mut cursor = None;
        let mut child_cursor = None;
        let mut child_page = std::collections::VecDeque::new();
        while cursor != through_sequence {
            let page = parent.page::<T>(cursor, limits)?;
            if page.events.is_empty() {
                return Err(SessionStoreError::ForkSourceCursorMissing);
            }
            let mut append = Vec::new();
            for envelope in page.events {
                if through_sequence.is_none_or(|through| envelope.sequence > through) {
                    break;
                }
                cursor = Some(envelope.sequence);
                let mapped = EventEnvelope {
                    schema_version: envelope.schema_version,
                    sequence: envelope.sequence,
                    event: map(envelope.event)?,
                };
                if existing
                    .last_sequence()
                    .is_some_and(|tail| mapped.sequence <= tail)
                {
                    if child_page.is_empty() {
                        child_page.extend(existing.page::<T>(child_cursor, limits)?.events);
                    }
                    let found = child_page
                        .pop_front()
                        .ok_or(SessionStoreError::ForkTargetConflict)?;
                    child_cursor = Some(found.sequence);
                    if found != mapped {
                        return Err(SessionStoreError::ForkTargetConflict);
                    }
                } else {
                    append.push(mapped.event);
                }
            }
            child.append_batch(append)?;
        }
        Ok(child)
    }
}

/// Removes abandoned session directories whose unlocked log has no user or
/// turn event. Sessions with a user turn or any sibling artifact are preserved.
///
/// # Errors
///
/// Returns an error when the sessions directory cannot be inspected or an
/// already-qualified empty directory cannot be atomically quarantined.
pub fn garbage_collect_empty_sessions(root: &Path) -> Result<Vec<String>, SessionStoreError> {
    #[cfg(not(unix))]
    {
        let _ = root;
        return Ok(Vec::new());
    }

    #[cfg(unix)]
    {
        let sessions_root = root.join("sessions");
        let entries = match fs::read_dir(&sessions_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut candidates = entries.collect::<Result<Vec<_>, _>>()?;
        candidates.sort_by_key(std::fs::DirEntry::file_name);
        let mut removed = Vec::new();
        for entry in candidates {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(session_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_session_id(&session_id).is_err() {
                continue;
            }
            let directory = entry.path();
            if !directory_contains_only_event_log(&directory)? {
                continue;
            }
            let log = match SessionEventLog::open(root, &session_id) {
                Ok(log) => log,
                Err(SessionStoreError::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let view = log.read_view();
            let mut cursor = None;
            let has_user_turn = loop {
                let page =
                    view.page::<serde_json::Value>(cursor, SessionEventPageLimits::default())?;
                if page.events.iter().any(|event| {
                    matches!(
                        event.event.get("type").and_then(serde_json::Value::as_str),
                        Some("turn_started" | "user_message_accepted")
                    )
                }) {
                    break true;
                }
                if !page.has_more {
                    break false;
                }
                cursor = page.next_cursor;
            };
            if has_user_turn || !directory_contains_only_event_log(&directory)? {
                continue;
            }
            let quarantine = root.join(format!(
                ".empty-session-{}-{session_id}",
                std::process::id()
            ));
            match fs::rename(&directory, &quarantine) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            }
            let journal = quarantine.join("journal");
            for entry in fs::read_dir(&journal)? {
                fs::remove_file(entry?.path())?;
            }
            fs::remove_dir(journal)?;
            fs::remove_dir(&quarantine)?;
            drop(log);
            removed.push(session_id);
        }
        Ok(removed)
    }
}

#[cfg(unix)]
fn directory_contains_only_event_log(directory: &Path) -> Result<bool, SessionStoreError> {
    let entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    if entries.len() != 1
        || entries[0].file_name() != "journal"
        || !entries[0].file_type()?.is_dir()
    {
        return Ok(false);
    }
    for entry in fs::read_dir(entries[0].path())? {
        if !entry?.file_type()?.is_file() {
            return Ok(false);
        }
    }
    Ok(true)
}
