//! Bounded immutable journal segments and descriptor-pinned read views (ADR-029).

mod append;
pub(super) mod decode;
pub use append::{JournalAppendPlan, PreparedJournalAppend};
mod catalog;
mod proof;
use catalog::SegmentCatalog;
pub use proof::{JournalAdvance, JournalPageProof, VerifiedJournalPage};

use super::{
    EVENT_SCHEMA_VERSION, EventEnvelope, SessionEventPage, SessionEventPageLimits,
    SessionStoreError,
};
use rw_types::SequenceId;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
#[cfg(not(unix))]
use std::io::Read as _;
#[cfg(test)]
use std::io::Write as _;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::Arc,
};

const SEGMENT_TARGET_BYTES: usize = 1024 * 1024;
/// Maximum encoded append/segment bytes, including every JSONL newline.
pub const MAX_JOURNAL_APPEND_BYTES: usize = 16 * 1024 * 1024;
/// Maximum structurally admitted decoded allocations in a record or returned page.
pub const MAX_JOURNAL_DECODE_BYTES: usize = 64 * 1024 * 1024;
const MAX_SEGMENT_BYTES: usize = MAX_JOURNAL_APPEND_BYTES;
const MAX_SEGMENTS: usize = 65_536;

/// One immutable segment boundary is a sparse sequence-index entry.
#[derive(Clone, Debug)]
struct Segment {
    first: u64,
    next: u64,
    bytes: u64,
    digest: blake3::Hash,
    name: String,
}

impl Segment {
    fn name(first: u64, next: u64, bytes: u64, digest: blake3::Hash) -> String {
        format!("{first:020}-{next:020}-{bytes:020}-{digest}.jsonl")
    }

    fn extend_identity(&self, prefix: blake3::Hash) -> blake3::Hash {
        let mut hash = blake3::Hasher::new_derive_key("rottweiler journal prefix v1");
        hash.update(prefix.as_bytes());
        hash.update(&self.first.to_le_bytes());
        hash.update(&self.next.to_le_bytes());
        hash.update(&self.bytes.to_le_bytes());
        hash.update(self.digest.as_bytes());
        hash.finalize()
    }

    fn parse(name: &str) -> Result<Self, SessionStoreError> {
        let invalid = || SessionStoreError::CorruptEvent("invalid sealed segment identity");
        let fields = name
            .strip_suffix(".jsonl")
            .ok_or_else(invalid)?
            .split('-')
            .collect::<Vec<_>>();
        if fields.len() != 4 {
            return Err(invalid());
        }
        let first = fields[0].parse::<u64>().map_err(|_| invalid())?;
        let next = fields[1].parse::<u64>().map_err(|_| invalid())?;
        let bytes = fields[2].parse::<u64>().map_err(|_| invalid())?;
        let digest = blake3::Hash::from_hex(fields[3]).map_err(|_| invalid())?;
        if first >= next
            || bytes == 0
            || bytes > MAX_SEGMENT_BYTES as u64
            || name != Self::name(first, next, bytes, digest)
        {
            return Err(invalid());
        }
        Ok(Self {
            first,
            next,
            bytes,
            digest,
            name: name.to_owned(),
        })
    }
}

/// Descriptor-bound root for one runtime's journal read service.
#[derive(Debug)]
pub struct JournalRoot {
    file: File,
    path: PathBuf,
}

impl JournalRoot {
    /// Pins an existing storage directory without following its final symlink.
    ///
    /// # Errors
    /// Rejects missing, unsafe or inaccessible storage directories.
    pub fn open(path: &Path) -> Result<Self, SessionStoreError> {
        #[cfg(unix)]
        let file = File::from(
            rustix::fs::open(
                path,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        );
        #[cfg(not(unix))]
        let file = {
            if !fs::symlink_metadata(path)?.file_type().is_dir() {
                return Err(SessionStoreError::UnsafeSessionDirectory);
            }
            File::open(path)?
        };
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// Tests for a segmented session without reading payloads or taking its writer lock.
    ///
    /// # Errors
    /// Rejects invalid journal layouts and unsafe directory components.
    pub fn contains_session(&self, session_id: &str) -> Result<bool, SessionStoreError> {
        match Directory::open_at(self, session_id, false) {
            Ok(_) => Ok(true),
            Err(SessionStoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Captures an offline prefix beneath the pinned storage directory.
    ///
    /// # Errors
    /// Rejects live writers, unsafe journal components and corrupt active tails.
    pub fn read_view(
        &self,
        session_id: &str,
    ) -> Result<Option<JournalReadView>, SessionStoreError> {
        JournalReadView::from_directory(Directory::open_at(self, session_id, false))
    }

    /// Checks that an active view belongs to this root's current session directory.
    ///
    /// # Errors
    /// Rejects unsafe or replaced session directories and foreign views.
    pub fn validate_view(
        &self,
        session_id: &str,
        view: &JournalReadView,
    ) -> Result<(), SessionStoreError> {
        let directory = Directory::open_at(self, session_id, false)?;
        #[cfg(unix)]
        {
            let expected = rustix::fs::fstat(&directory.file).map_err(std::io::Error::from)?;
            let actual = rustix::fs::fstat(&view.directory.file).map_err(std::io::Error::from)?;
            if expected.st_dev != actual.st_dev || expected.st_ino != actual.st_ino {
                return Err(SessionStoreError::UnsafeSessionDirectory);
            }
        }
        #[cfg(not(unix))]
        if directory.path != view.directory.path {
            return Err(SessionStoreError::UnsafeSessionDirectory);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Directory {
    file: File,
    path: PathBuf,
}

impl Directory {
    fn open(root: &Path, session_id: &str, create: bool) -> Result<Self, SessionStoreError> {
        if create {
            fs::create_dir_all(root)?;
        }
        Self::open_at(&JournalRoot::open(root)?, session_id, create)
    }

    fn open_at(
        root: &JournalRoot,
        session_id: &str,
        create: bool,
    ) -> Result<Self, SessionStoreError> {
        super::validate_session_id(session_id)?;
        let path = root.path.join("sessions").join(session_id).join("journal");
        #[cfg(unix)]
        {
            let mut parent = root.file.try_clone()?;
            for name in ["sessions", session_id, "journal"] {
                if name == "journal" {
                    match rustix::fs::statat(
                        &parent,
                        "events.jsonl",
                        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
                    ) {
                        Ok(_) => return Err(SessionStoreError::UnsupportedJournalLayout),
                        Err(rustix::io::Errno::NOENT) => {}
                        Err(error) => return Err(std::io::Error::from(error).into()),
                    }
                }
                parent = if create {
                    super::open_or_create_directory(&parent, name)?
                } else {
                    File::from(
                        rustix::fs::openat(
                            &parent,
                            name,
                            rustix::fs::OFlags::RDONLY
                                | rustix::fs::OFlags::DIRECTORY
                                | rustix::fs::OFlags::NOFOLLOW
                                | rustix::fs::OFlags::CLOEXEC,
                            rustix::fs::Mode::empty(),
                        )
                        .map_err(std::io::Error::from)?,
                    )
                };
            }
            Ok(Self { file: parent, path })
        }
        #[cfg(not(unix))]
        {
            if create {
                super::create_checked_directory_portable(&root.path)?;
            }
            match fs::symlink_metadata(
                root.path
                    .join("sessions")
                    .join(session_id)
                    .join("events.jsonl"),
            ) {
                Ok(_) => return Err(SessionStoreError::UnsupportedJournalLayout),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            for directory in [
                root.path.join("sessions"),
                root.path.join("sessions").join(session_id),
                path.clone(),
            ] {
                if create {
                    super::create_checked_directory_portable(&directory)?;
                }
                if !fs::symlink_metadata(&directory)?.file_type().is_dir() {
                    return Err(SessionStoreError::UnsafeSessionDirectory);
                }
            }
            Ok(Self {
                file: File::open(&path)?,
                path,
            })
        }
    }

    fn file(&self, name: &str, writable: bool, create: bool) -> Result<File, SessionStoreError> {
        #[cfg(unix)]
        let file = {
            use rustix::fs::{Mode, OFlags};
            let mut flags = OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
            flags |= if writable {
                OFlags::RDWR | OFlags::APPEND
            } else {
                OFlags::RDONLY
            };
            if create {
                flags |= OFlags::CREATE;
            }
            File::from(
                rustix::fs::openat(&self.file, name, flags, Mode::RUSR | Mode::WUSR)
                    .map_err(std::io::Error::from)?,
            )
        };
        #[cfg(not(unix))]
        let file = {
            let path = self.path.join(name);
            if let Ok(metadata) = fs::symlink_metadata(&path) {
                if !metadata.file_type().is_file() {
                    return Err(SessionStoreError::UnsafeEventFileType);
                }
            }
            fs::OpenOptions::new()
                .read(true)
                .append(writable)
                .create(create)
                .open(path)?
        };
        super::event_file_snapshot(&file)?;
        Ok(file)
    }

    fn names(&self) -> Result<Vec<String>, SessionStoreError> {
        let mut names = Vec::new();
        #[cfg(unix)]
        {
            let mut entries =
                rustix::fs::Dir::read_from(&self.file).map_err(std::io::Error::from)?;
            while let Some(entry) = entries.read() {
                let entry = entry.map_err(std::io::Error::from)?;
                let name = entry
                    .file_name()
                    .to_str()
                    .map_err(|_| SessionStoreError::UnsafeEventFileType)?;
                if name != "." && name != ".." {
                    names.push(name.to_owned());
                }
                if names.len() > MAX_SEGMENTS + 8 {
                    return Err(SessionStoreError::CorruptEvent("too many journal segments"));
                }
            }
        }
        #[cfg(not(unix))]
        for entry in fs::read_dir(&self.path)? {
            names.push(
                entry?
                    .file_name()
                    .into_string()
                    .map_err(|_| SessionStoreError::UnsafeEventFileType)?,
            );
            if names.len() > MAX_SEGMENTS + 8 {
                return Err(SessionStoreError::CorruptEvent("too many journal segments"));
            }
        }
        Ok(names)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), SessionStoreError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        rustix::fs::renameat_with(
            &self.file,
            from,
            &self.file,
            to,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(std::io::Error::from)?;
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic no-replace journal publication is unsupported on this target",
        )
        .into());
        #[cfg(not(unix))]
        fs::rename(self.path.join(from), self.path.join(to))?;
        self.file.sync_all()?;
        Ok(())
    }

    fn derived_directory(&self) -> Result<File, SessionStoreError> {
        #[cfg(unix)]
        {
            super::open_or_create_directory(&self.file, "derived")
        }
        #[cfg(not(unix))]
        {
            let path = self.path.join("derived");
            super::create_checked_directory_portable(&path)?;
            Ok(File::open(path)?)
        }
    }

    fn catalog(&self) -> Result<Vec<Segment>, SessionStoreError> {
        let mut segments = Vec::new();
        for name in self.names()? {
            if name == "active.jsonl" || name == "writer.lock" || name == "derived" {
                continue;
            }
            segments.push(Segment::parse(&name)?);
        }
        segments.sort_unstable_by_key(|segment| segment.first);
        let mut expected = 0;
        for segment in &segments {
            if segment.first != expected {
                return Err(SessionStoreError::CorruptEvent(
                    "non-contiguous journal segments",
                ));
            }
            expected = segment.next;
        }
        Ok(segments)
    }
}

/// Exclusive append owner. Sealed segment identities form its sparse index.
#[derive(Debug)]
pub struct SegmentedJournal {
    directory: Arc<Directory>,
    segments: Arc<SegmentCatalog>,
    segment_count: usize,
    active: Arc<File>,
    active_first: u64,
    active_bytes: u64,
    active_hash: blake3::Hasher,
    active_state: super::EventFileSnapshot,
    sealed_bytes: u64,
    sealed_identity: blake3::Hash,
    next_sequence: u64,
    poisoned: bool,
    _writer_lock: super::file_lock::AdvisoryFileLock,
}

/// Immutable logical prefix; later append/rotation cannot change its tail.
#[derive(Clone, Debug)]
pub struct JournalReadView {
    directory: Arc<Directory>,
    segments: Arc<SegmentCatalog>,
    segment_count: usize,
    active: Arc<File>,
    active_segment: Option<Segment>,
    next_sequence: u64,
    total_bytes: u64,
    identity: JournalPrefixIdentity,
}

/// Content-bound prefix identity used to anchor derived recovery checkpoints.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JournalPrefixIdentity {
    /// Exclusive sequence watermark of the identified prefix.
    pub next_sequence: u64,
    /// Chained segment content hashes, including the captured active prefix.
    pub digest: [u8; 32],
}

impl JournalPrefixIdentity {
    /// Identity of the empty authoritative prefix, before event zero.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            next_sequence: 0,
            digest: *blake3::hash(b"").as_bytes(),
        }
    }
}

/// Work performed by a page read, independent of historical journal size.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct JournalReadMetrics {
    /// Segment bytes read and checksummed, including records skipped before the cursor.
    pub bytes_read: u64,
    /// Record boundaries inspected in the referenced segments.
    pub records_scanned: u64,
    /// Envelopes deserialized into the returned page.
    pub records_decoded: u64,
    /// Referenced segments read from disk.
    pub segments_read: u64,
}

/// Explicit full-verification coverage, distinct from normal page reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalVerification {
    /// Contiguous records verified from sequence zero.
    pub events: u64,
    /// Bytes whose record structure and checksum were verified.
    pub bytes: u64,
}

impl SegmentedJournal {
    /// Opens or creates the derived-index directory beneath this pinned journal.
    ///
    /// Index owners use an independent lock and atomic/batched publication. These
    /// files are projections and cannot change the authoritative event prefix.
    ///
    /// # Errors
    /// Rejects unsafe directory components and propagates directory creation I/O.
    pub fn derived_directory(&self) -> Result<File, SessionStoreError> {
        self.directory.derived_directory()
    }

    /// Opens the journal, repairing only an incomplete active-tail record.
    /// Sealed historical checksums are verified on access or by `verify_all`.
    ///
    /// # Errors
    /// Rejects unsafe paths/descriptors, conflicting writers and corrupt tails.
    pub fn open(root: &Path, session_id: &str) -> Result<Self, SessionStoreError> {
        let directory = Arc::new(Directory::open(root, session_id, true)?);
        let writer_lock = directory.file("writer.lock", true, true)?;
        let writer_lock = super::file_lock::AdvisoryFileLock::try_exclusive(writer_lock)?;
        let segments = Arc::new(SegmentCatalog::from_segments(directory.catalog()?));
        let segment_count = segments.len();
        let active_first = segment_count
            .checked_sub(1)
            .and_then(|index| segments.get(index))
            .map_or(0, |segment| segment.next);
        let active = Arc::new(directory.file("active.jsonl", true, true)?);
        directory.file.sync_all()?;
        let mut bytes = super::read_opened_file_bounded(&active, MAX_SEGMENT_BYTES as u64)?;
        if bytes.last().is_some_and(|byte| *byte != b'\n') {
            let complete = bytes
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |offset| offset + 1);
            super::truncate_and_sync_event_file(&active, complete as u64)?;
            bytes.truncate(complete);
        }
        let next_sequence = super::validate_events_from_sequence(&bytes, active_first)?;
        let mut active_hash = blake3::Hasher::new();
        active_hash.update(&bytes);
        let active_state = super::event_file_snapshot(&active)?;
        let (sealed_bytes, sealed_identity) = segments.prefix(segment_count);
        Ok(Self {
            sealed_identity,
            active_state,
            sealed_bytes,
            directory,
            _writer_lock: writer_lock,
            segments,
            segment_count,
            active,
            active_first,
            active_bytes: bytes.len() as u64,
            active_hash,
            next_sequence,
            poisoned: false,
        })
    }

    /// Physical directory of the segmented journal.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.directory.path
    }

    /// Next sequence to assign; zero denotes an empty journal.
    #[must_use]
    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Appends one bounded batch with one event-file synchronization.
    ///
    /// # Errors
    /// Rejects oversized batches, changed active data and failed durable writes.
    pub fn append_batch<T: Serialize + rw_types::allocation::DecodeAllocation>(
        &mut self,
        events: impl IntoIterator<Item = T>,
    ) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        let span = tracing::trace_span!(target: "rw_performance", "journal.append",
            session_id = ?self.directory.path.parent().and_then(Path::file_name),
            first_sequence = self.next_sequence,
            events = tracing::field::Empty, bytes = tracing::field::Empty);
        let _entered = span.enter();
        let (prepared, envelopes) = {
            let _serialize =
                tracing::trace_span!(target: "rw_performance", "journal.serialize").entered();
            append::encode_owned(self.next_sequence, events)?
        };
        span.record("events", prepared.count);
        span.record("bytes", prepared.bytes.len());
        self.write_prepared(prepared)?;
        Ok(envelopes)
    }

    /// Writes an encoded batch only at its captured expected sequence.
    ///
    /// # Errors
    /// Rejects a changed writer prefix, unsafe descriptors, or failed writes/syncs.
    pub fn append_prepared(
        &mut self,
        prepared: PreparedJournalAppend,
    ) -> Result<(), SessionStoreError> {
        let _span = tracing::trace_span!(target: "rw_performance", "journal.append",
            session_id = ?self.directory.path.parent().and_then(Path::file_name),
            first_sequence = prepared.first, events = prepared.count, bytes = prepared.bytes.len())
        .entered();
        self.write_prepared(prepared)
    }

    fn write_prepared(&mut self, prepared: PreparedJournalAppend) -> Result<(), SessionStoreError> {
        if prepared.first != self.next_sequence {
            return Err(SessionStoreError::UnexpectedEventSequence {
                expected: SequenceId(self.next_sequence),
                actual: SequenceId(prepared.first),
            });
        }
        if self.poisoned {
            return Err(SessionStoreError::EventWriterPoisoned);
        }
        if let Err(error) = super::verify_event_file_snapshot(&self.active, &self.active_state) {
            self.poisoned = true;
            return Err(error);
        }
        let PreparedJournalAppend {
            bytes,
            next: next_sequence,
            count,
            ..
        } = prepared;
        if count == 0 {
            return Ok(());
        }
        if self.active_bytes > 0
            && self.active_bytes + bytes.len() as u64 > SEGMENT_TARGET_BYTES as u64
        {
            self.seal()?;
        }
        if self.active.metadata()?.len() != self.active_bytes {
            self.poisoned = true;
            return Err(SessionStoreError::CorruptEvent(
                "active journal length changed after validation",
            ));
        }
        let append = {
            let _write = tracing::trace_span!(target: "rw_performance", "journal.write", bytes = bytes.len()).entered();
            super::write_event_bytes(&mut &*self.active, &bytes)
        }
        .and_then(|()| {
            let _sync = tracing::trace_span!(target: "rw_performance", "journal.sync").entered();
            #[cfg(test)]
            telemetry_tests::run_sync_hook();
            super::sync_event_file(&self.active)
        });
        if let Err(error) = append {
            if let Err(rollback) =
                super::truncate_and_sync_event_file(&self.active, self.active_bytes)
            {
                self.poisoned = true;
                return Err(SessionStoreError::AppendRollbackFailed {
                    append: error,
                    rollback,
                });
            }
            self.poisoned = true;
            self.active_state = super::event_file_snapshot(&self.active)?;
            self.poisoned = false;
            return Err(error.into());
        }
        self.active_bytes += bytes.len() as u64;
        self.active_hash.update(&bytes);
        self.next_sequence = next_sequence;
        self.poisoned = true;
        self.active_state = super::event_file_snapshot(&self.active)?;
        self.poisoned = false;
        Ok(())
    }

    fn seal(&mut self) -> Result<(), SessionStoreError> {
        if self.segment_count >= MAX_SEGMENTS {
            return Err(SessionStoreError::CorruptEvent("too many journal segments"));
        }
        let digest = self.active_hash.finalize();
        let name = Segment::name(
            self.active_first,
            self.next_sequence,
            self.active_bytes,
            digest,
        );
        self.poisoned = true;
        self.directory.rename("active.jsonl", &name)?;
        // After rename, any failure requires reopening from the authoritative
        // segment names rather than continuing through an obsolete descriptor.
        self.poisoned = true;
        let active = Arc::new(self.directory.file("active.jsonl", true, true)?);
        if active.metadata()?.len() != 0 {
            return Err(SessionStoreError::CorruptEvent(
                "new active segment is not empty",
            ));
        }
        self.directory.file.sync_all()?;
        let segment = Segment {
            first: self.active_first,
            next: self.next_sequence,
            bytes: self.active_bytes,
            digest,
            name,
        };
        self.sealed_identity = segment.extend_identity(self.sealed_identity);
        self.segments.push(segment);
        self.segment_count += 1;
        self.sealed_bytes += self.active_bytes;
        self.active_state = super::event_file_snapshot(&active)?;
        self.active = active;
        self.active_first = self.next_sequence;
        self.active_bytes = 0;
        self.active_hash = blake3::Hasher::new();
        self.poisoned = false;
        Ok(())
    }

    /// Pins the current committed prefix without reading journal bytes.
    #[must_use]
    pub fn read_view(&self) -> JournalReadView {
        let active_segment = (self.active_bytes > 0).then(|| Segment {
            first: self.active_first,
            next: self.next_sequence,
            bytes: self.active_bytes,
            digest: self.active_hash.finalize(),
            name: "active.jsonl".to_owned(),
        });
        let digest = active_segment
            .as_ref()
            .map_or(self.sealed_identity, |segment| {
                segment.extend_identity(self.sealed_identity)
            });
        JournalReadView {
            identity: JournalPrefixIdentity {
                next_sequence: self.next_sequence,
                digest: *digest.as_bytes(),
            },
            directory: Arc::clone(&self.directory),
            segments: Arc::clone(&self.segments),
            segment_count: self.segment_count,
            active: Arc::clone(&self.active),
            active_segment,
            next_sequence: self.next_sequence,
            total_bytes: self.sealed_bytes + self.active_bytes,
        }
    }
}

impl JournalReadView {
    /// Opens or creates the derived-index directory beneath this pinned journal.
    ///
    /// Index owners use an independent lock and atomic/batched publication. These
    /// files are projections and cannot change the authoritative event prefix.
    ///
    /// # Errors
    /// Rejects unsafe directory components and propagates directory creation I/O.
    pub fn derived_directory(&self) -> Result<File, SessionStoreError> {
        self.directory.derived_directory()
    }

    /// Captures an existing offline journal without modifying it.
    ///
    /// A live owner must supply its own [`SegmentedJournal::read_view`]. Taking
    /// a shared capture lock excludes writers while allowing independent readers.
    /// This prevents mistaking unsynchronized active records for an acknowledged
    /// tail. The lock is released after catalog and active-prefix capture.
    ///
    /// # Errors
    /// Rejects a live writer, unsafe files, corrupt complete records, or an
    /// interrupted tail which must first be recovered by the writer.
    pub fn open_existing(root: &Path, session_id: &str) -> Result<Option<Self>, SessionStoreError> {
        Self::from_directory(Directory::open(root, session_id, false))
    }

    fn from_directory(
        directory: Result<Directory, SessionStoreError>,
    ) -> Result<Option<Self>, SessionStoreError> {
        let directory = match directory {
            Ok(directory) => Arc::new(directory),
            Err(SessionStoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let ownership = directory.file("writer.lock", false, false)?;
        // Offline readers share capture ownership; every writer remains exclusive.
        let _ownership = super::file_lock::AdvisoryFileLock::try_shared(ownership)?;
        let segments = Arc::new(SegmentCatalog::from_segments(directory.catalog()?));
        let segment_count = segments.len();
        let active = Arc::new(directory.file("active.jsonl", false, false)?);
        let bytes = super::read_opened_file_bounded(&active, MAX_SEGMENT_BYTES as u64)?;
        if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
            return Err(SessionStoreError::CorruptEvent(
                "active journal tail requires writer recovery",
            ));
        }
        let first = segment_count
            .checked_sub(1)
            .and_then(|index| segments.get(index))
            .map_or(0, |segment| segment.next);
        let next_sequence = super::validate_events_from_sequence(&bytes, first)?;
        let active_segment = (!bytes.is_empty()).then(|| Segment {
            first,
            next: next_sequence,
            bytes: bytes.len() as u64,
            digest: blake3::hash(&bytes),
            name: "active.jsonl".to_owned(),
        });
        let (sealed_bytes, sealed_identity) = segments.prefix(segment_count);
        let total_bytes = sealed_bytes + bytes.len() as u64;
        let digest = active_segment.as_ref().map_or(sealed_identity, |segment| {
            segment.extend_identity(sealed_identity)
        });
        Ok(Some(Self {
            directory,
            segments,
            segment_count,
            active,
            active_segment,
            next_sequence,
            total_bytes,
            identity: JournalPrefixIdentity {
                next_sequence,
                digest: *digest.as_bytes(),
            },
        }))
    }

    /// Reopens an exact historical prefix without retaining a server-side cursor.
    ///
    /// The boundary segment is read and verified. Earlier segment identities are
    /// included in the chain; their payloads remain subject to on-access or full
    /// integrity verification. This also reconstructs prefixes originally captured
    /// inside an active segment which has since grown or been sealed.
    ///
    /// # Errors
    /// Rejects future watermarks, mismatched identities and corrupt boundary data.
    pub fn at_prefix(&self, identity: JournalPrefixIdentity) -> Result<Self, SessionStoreError> {
        let prefix = self.prefix_at(identity.next_sequence)?;
        if prefix.identity != identity {
            return Err(SessionStoreError::CorruptEvent(
                "journal prefix identity mismatch",
            ));
        }
        Ok(prefix)
    }

    /// Captures the exact prefix ending at a validated event cursor.
    /// `None` denotes the empty prefix. Only the boundary segment payload is read.
    ///
    /// # Errors
    /// Rejects future cursors, overflow and corrupt or unsafe boundary segments.
    pub fn prefix_through(&self, through: Option<SequenceId>) -> Result<Self, SessionStoreError> {
        let next = through
            .map(|through| {
                through
                    .0
                    .checked_add(1)
                    .ok_or(SessionStoreError::SequenceOverflow)
            })
            .transpose()?
            .unwrap_or(0);
        self.prefix_at(next)
    }

    fn prefix_at(&self, next: u64) -> Result<Self, SessionStoreError> {
        if next > self.next_sequence {
            return Err(SessionStoreError::EventPageCursorAhead);
        }
        if next == 0 {
            return Ok(Self {
                directory: Arc::clone(&self.directory),
                segments: Arc::clone(&self.segments),
                segment_count: 0,
                active: Arc::clone(&self.active),
                active_segment: None,
                next_sequence: 0,
                total_bytes: 0,
                identity: JournalPrefixIdentity::empty(),
            });
        }
        let index = self
            .segments
            .partition(self.segment_count, |segment| segment.next < next);
        let (boundary, active) = if let Some(segment) = (index < self.segment_count)
            .then(|| self.segments.get(index))
            .flatten()
        {
            (segment, false)
        } else {
            (
                self.active_segment
                    .clone()
                    .ok_or(SessionStoreError::CorruptEvent(
                        "missing journal prefix boundary",
                    ))?,
                true,
            )
        };
        let boundary_file = self.segment_file(&boundary, active)?;
        let bytes = Self::read_segment_bytes(&boundary_file, &boundary, active)?;
        let count = next
            .checked_sub(boundary.first)
            .and_then(|count| usize::try_from(count).ok())
            .ok_or(SessionStoreError::CorruptEvent(
                "invalid journal prefix boundary",
            ))?;
        let mut delimiters = memchr::memchr_iter(b'\n', &bytes);
        let prefix_bytes = count
            .checked_sub(1)
            .and_then(|index| delimiters.nth(index))
            .map(|offset| offset + 1)
            .ok_or(SessionStoreError::CorruptEvent(
                "missing journal prefix records",
            ))?;
        let prefix = Segment {
            first: boundary.first,
            next,
            bytes: prefix_bytes as u64,
            digest: blake3::hash(&bytes[..prefix_bytes]),
            name: boundary.name.clone(),
        };
        let (prior_bytes, prior) = self.segments.prefix(index);
        let identity = JournalPrefixIdentity {
            next_sequence: next,
            digest: *prefix.extend_identity(prior).as_bytes(),
        };
        let total_bytes = prior_bytes + prefix.bytes;
        Ok(Self {
            directory: Arc::clone(&self.directory),
            segments: Arc::clone(&self.segments),
            segment_count: index,
            active: boundary_file,
            active_segment: Some(prefix),
            next_sequence: next,
            total_bytes,
            identity,
        })
    }

    /// Identity of this captured prefix, stable across rotation and reopening.
    #[must_use]
    pub const fn prefix_identity(&self) -> JournalPrefixIdentity {
        self.identity
    }

    /// Tail captured by this view, independent of subsequent appends.
    #[must_use]
    pub const fn last_sequence(&self) -> Option<SequenceId> {
        if self.next_sequence == 0 {
            None
        } else {
            Some(SequenceId(self.next_sequence - 1))
        }
    }

    fn segment_file(
        &self,
        segment: &Segment,
        active: bool,
    ) -> Result<Arc<File>, SessionStoreError> {
        if active {
            Ok(Arc::clone(&self.active))
        } else {
            self.directory
                .file(&segment.name, false, false)
                .map(Arc::new)
        }
    }

    fn segments_from(&self, first: usize) -> impl DoubleEndedIterator<Item = (Segment, bool)> + '_ {
        (first..self.segment_count)
            .filter_map(|index| self.segments.get(index).map(|segment| (segment, false)))
            .chain(
                self.active_segment
                    .iter()
                    .cloned()
                    .map(|segment| (segment, true)),
            )
    }

    fn segment_bytes(&self, segment: &Segment, active: bool) -> Result<Vec<u8>, SessionStoreError> {
        let file = self.segment_file(segment, active)?;
        Self::read_segment_bytes(&file, segment, active)
    }

    fn read_segment_bytes(
        file: &File,
        segment: &Segment,
        active: bool,
    ) -> Result<Vec<u8>, SessionStoreError> {
        let before = super::event_file_snapshot(file)?;
        if before.len() < segment.bytes || (!active && before.len() != segment.bytes) {
            return Err(SessionStoreError::CorruptEvent(
                "pinned journal segment length changed",
            ));
        }
        let mut bytes =
            vec![0; usize::try_from(segment.bytes).map_err(|_| SessionStoreError::LimitOverflow)?];
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt as _;
            file.read_exact_at(&mut bytes, 0)?;
        }
        #[cfg(not(unix))]
        {
            use std::io::{Seek as _, SeekFrom};
            let mut file = file.try_clone()?;
            file.seek(SeekFrom::Start(0))?;
            file.read_exact(&mut bytes)?;
        }
        #[cfg(test)]
        super::run_event_read_hook();
        if blake3::hash(&bytes) != segment.digest {
            return Err(SessionStoreError::CorruptEvent(
                "pinned journal segment checksum changed",
            ));
        }
        if active {
            super::event_file_snapshot(file)?;
        } else {
            super::verify_event_file_snapshot(file, &before)?;
        }
        Ok(bytes)
    }

    fn page_segment_bytes(
        &self,
        segment: &Segment,
        active: bool,
        limits: SessionEventPageLimits,
        metrics: &mut JournalReadMetrics,
    ) -> Result<(Arc<File>, Vec<u8>), SessionStoreError> {
        if metrics.bytes_read + segment.bytes > limits.max_scan_bytes {
            return Err(SessionStoreError::EventScanBytesExceeded {
                max_bytes: limits.max_scan_bytes,
            });
        }
        let segment_records = segment.next - segment.first;
        if metrics.records_scanned + segment_records > limits.max_scan_events {
            return Err(SessionStoreError::EventScanCountExceeded {
                max_events: limits.max_scan_events,
            });
        }
        let file = self.segment_file(segment, active)?;
        let bytes = Self::read_segment_bytes(&file, segment, active)?;
        let record_count = memchr::memchr_iter(b'\n', &bytes).count() as u64;
        if record_count != segment_records || bytes.last() != Some(&b'\n') {
            return Err(SessionStoreError::CorruptEvent(
                "journal segment record count differs from index",
            ));
        }
        metrics.bytes_read += segment.bytes;
        metrics.records_scanned += segment_records;
        metrics.segments_read += 1;
        Ok((file, bytes))
    }

    /// Reads one bounded cursor-exclusive page from the pinned prefix.
    ///
    /// # Errors
    /// Rejects invalid cursors/limits, corrupt referenced segments and oversized records.
    pub fn page<T: DeserializeOwned + rw_types::allocation::DecodeAllocation>(
        &self,
        after: Option<SequenceId>,
        limits: SessionEventPageLimits,
    ) -> Result<SessionEventPage<T>, SessionStoreError> {
        self.page_with_metrics(after, limits).map(|(page, _)| page)
    }

    /// Reads a page and reports the actual segment I/O and decoding work.
    ///
    /// # Errors
    /// Has the same cursor, integrity and resource-limit errors as [`Self::page`].
    pub fn page_with_metrics<T: DeserializeOwned + rw_types::allocation::DecodeAllocation>(
        &self,
        after: Option<SequenceId>,
        limits: SessionEventPageLimits,
    ) -> Result<(SessionEventPage<T>, JournalReadMetrics), SessionStoreError> {
        self.page_internal(after, limits, None)
    }

    fn checked_page_start(
        &self,
        after: Option<SequenceId>,
        limits: SessionEventPageLimits,
    ) -> Result<u64, SessionStoreError> {
        if limits.max_page_events == 0
            || limits.max_page_bytes == 0
            || limits.max_line_bytes == 0
            || limits.max_scan_bytes == 0
            || limits.max_scan_events == 0
        {
            return Err(SessionStoreError::InvalidEventPageLimits);
        }
        let first = match after {
            Some(sequence) => sequence
                .0
                .checked_add(1)
                .ok_or(SessionStoreError::EventPageCursorAhead)?,
            None => 0,
        };
        if first > self.next_sequence {
            return Err(SessionStoreError::EventPageCursorAhead);
        }
        Ok(first)
    }

    fn page_internal<T: DeserializeOwned + rw_types::allocation::DecodeAllocation>(
        &self,
        after: Option<SequenceId>,
        limits: SessionEventPageLimits,
        mut proof: Option<&mut proof::ProofBuilder>,
    ) -> Result<(SessionEventPage<T>, JournalReadMetrics), SessionStoreError> {
        let span = tracing::trace_span!(target: "rw_performance", "journal.page",
            session_id = ?self.directory.path.parent().and_then(Path::file_name),
            after = ?after, through = self.next_sequence,
            bytes_read = tracing::field::Empty, records_scanned = tracing::field::Empty,
            records_decoded = tracing::field::Empty, segments_read = tracing::field::Empty);
        let _entered = span.enter();
        let first = self.checked_page_start(after, limits)?;
        let mut metrics = JournalReadMetrics::default();
        let mut events = Vec::new();
        let mut page_bytes = 0;
        let mut decode_bytes = 0usize;
        let mut next = first;
        let first_segment = self
            .segments
            .partition(self.segment_count, |segment| segment.next <= first);
        'segments: for (offset, (segment, active)) in self.segments_from(first_segment).enumerate()
        {
            if events.len() >= limits.max_page_events || page_bytes >= limits.max_page_bytes {
                break;
            }
            if segment.next <= first {
                continue;
            }
            if proof
                .as_ref()
                .is_some_and(|proof| !proof.can_read_segment())
            {
                break;
            }
            let (file, bytes) = self.page_segment_bytes(&segment, active, limits, &mut metrics)?;
            if let Some(proof) = &mut proof {
                proof.begin_segment(first_segment + offset, &segment, file);
            }
            for (index, line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
                let sequence = segment.first + index as u64;
                if sequence < first {
                    if let Some(proof) = &mut proof {
                        proof.line(sequence, line, false);
                    }
                    continue;
                }
                if events.len() >= limits.max_page_events {
                    break 'segments;
                }
                if line.len() - 1 > limits.max_line_bytes {
                    return Err(SessionStoreError::EventRecordTooLarge {
                        max_line_bytes: limits.max_line_bytes,
                    });
                }
                if page_bytes + line.len() as u64 > limits.max_page_bytes {
                    if events.is_empty() {
                        return Err(SessionStoreError::EventPageByteLimitTooSmall {
                            required_bytes: line.len() as u64,
                            max_bytes: limits.max_page_bytes,
                        });
                    }
                    break 'segments;
                }
                let charge = decode::preflight_record::<T>(line)?;
                if charge > MAX_JOURNAL_DECODE_BYTES - decode_bytes {
                    break 'segments;
                }
                decode_bytes += charge;
                let envelope = decode_page_event(line, next)?;
                if let Some(proof) = &mut proof {
                    proof.line(sequence, line, true);
                }
                page_bytes += line.len() as u64;
                next += 1;
                events.push(envelope);
                metrics.records_decoded += 1;
            }
        }
        if next < self.next_sequence && events.len() < limits.max_page_events && page_bytes == 0 {
            return Err(SessionStoreError::CorruptEvent(
                "journal segment index exceeds its records",
            ));
        }
        span.record("bytes_read", metrics.bytes_read);
        span.record("records_scanned", metrics.records_scanned);
        span.record("records_decoded", metrics.records_decoded);
        span.record("segments_read", metrics.segments_read);
        Ok((
            SessionEventPage {
                events,
                page_bytes,
                next_cursor: next.checked_sub(1).map(SequenceId),
                has_more: next < self.next_sequence,
                events_before_page: first,
                events_after_page: self.next_sequence - next,
                total_events: self.next_sequence,
                total_bytes: self.total_bytes,
                tail_sequence: self.last_sequence(),
            },
            metrics,
        ))
    }

    /// Reads a complete prefix only when it fits explicit aggregate limits.
    ///
    /// Intended for bounded exports and small control records. Replay and recovery
    /// should fold [`Self::page`] results instead of retaining the complete prefix.
    ///
    /// # Errors
    /// Rejects aggregate limits before allocation and propagates page integrity errors.
    pub fn collect_bounded<T: DeserializeOwned + rw_types::allocation::DecodeAllocation>(
        &self,
        max_bytes: u64,
        max_events: usize,
    ) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        if self.total_bytes > max_bytes {
            return Err(SessionStoreError::EventLogTooLarge { max_bytes });
        }
        if self.next_sequence > max_events as u64 {
            return Err(SessionStoreError::EventCountTooLarge { max_events });
        }
        let limits = SessionEventPageLimits {
            max_page_bytes: MAX_SEGMENT_BYTES as u64,
            max_page_events: 2_000,
            ..SessionEventPageLimits::default()
        };
        let mut result = Vec::new();
        let mut after = None;
        loop {
            let page = self.page(after, limits)?;
            after = page.next_cursor;
            result.extend(page.events);
            if !page.has_more {
                return Ok(result);
            }
        }
    }

    /// Total serialized bytes in this pinned prefix, without reading payloads.
    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Reads the latest bounded page ending at this view's durable tail.
    ///
    /// # Errors
    /// Rejects invalid limits, corrupt referenced segments and oversized last records.
    pub fn tail_page<T: DeserializeOwned + rw_types::allocation::DecodeAllocation>(
        &self,
        limits: SessionEventPageLimits,
    ) -> Result<SessionEventPage<T>, SessionStoreError> {
        if limits.max_page_events == 0
            || limits.max_page_bytes == 0
            || limits.max_line_bytes == 0
            || limits.max_scan_bytes == 0
            || limits.max_scan_events == 0
        {
            return Err(SessionStoreError::InvalidEventPageLimits);
        }
        let mut events = Vec::new();
        let mut page_bytes = 0;
        let mut decode_bytes = 0usize;
        let mut metrics = JournalReadMetrics::default();
        'segments: for (segment, active) in self.segments_from(0).rev() {
            if events.len() >= limits.max_page_events || page_bytes >= limits.max_page_bytes {
                break;
            }
            let (_file, bytes) = self.page_segment_bytes(&segment, active, limits, &mut metrics)?;
            let mut end = bytes.len();
            for index in memchr::memchr_iter(b'\n', &bytes[..bytes.len() - 1])
                .rev()
                .map(|position| position + 1)
                .chain(std::iter::once(0))
            {
                let line = &bytes[index..end];
                end = index;
                if events.len() >= limits.max_page_events {
                    break 'segments;
                }
                if line.len() - 1 > limits.max_line_bytes {
                    return Err(SessionStoreError::EventRecordTooLarge {
                        max_line_bytes: limits.max_line_bytes,
                    });
                }
                if page_bytes + line.len() as u64 > limits.max_page_bytes {
                    if events.is_empty() {
                        return Err(SessionStoreError::EventPageByteLimitTooSmall {
                            required_bytes: line.len() as u64,
                            max_bytes: limits.max_page_bytes,
                        });
                    }
                    break 'segments;
                }
                let charge = decode::preflight_record::<T>(line)?;
                if charge > MAX_JOURNAL_DECODE_BYTES - decode_bytes {
                    break 'segments;
                }
                decode_bytes += charge;
                let event: EventEnvelope<T> = serde_json::from_slice(line)?;
                if event.schema_version != EVENT_SCHEMA_VERSION {
                    return Err(SessionStoreError::UnsupportedEventVersion(
                        event.schema_version,
                    ));
                }
                if event.sequence.0 != self.next_sequence - events.len() as u64 - 1 {
                    return Err(SessionStoreError::CorruptEvent(
                        "non-contiguous journal tail page",
                    ));
                }
                page_bytes += line.len() as u64;
                events.push(event);
            }
        }
        events.reverse();
        Ok(SessionEventPage {
            events_before_page: self.next_sequence - events.len() as u64,
            events,
            page_bytes,
            next_cursor: self.last_sequence(),
            has_more: false,
            events_after_page: 0,
            total_events: self.next_sequence,
            total_bytes: self.total_bytes,
            tail_sequence: self.last_sequence(),
        })
    }

    /// Checks every historical segment and record with bounded working memory.
    ///
    /// # Errors
    /// Rejects checksum, schema, sequence or descriptor corruption anywhere in the view.
    pub fn verify_all(&self) -> Result<JournalVerification, SessionStoreError> {
        let mut expected = 0;
        let mut verified_bytes = 0;
        for (segment, active) in self.segments_from(0) {
            let bytes = self.segment_bytes(&segment, active)?;
            expected = super::validate_events_from_sequence(&bytes, expected)?;
            if expected != segment.next {
                return Err(SessionStoreError::CorruptEvent(
                    "journal segment record count differs from index",
                ));
            }
            verified_bytes += segment.bytes;
        }
        Ok(JournalVerification {
            events: expected,
            bytes: verified_bytes,
        })
    }
}

fn decode_page_event<T: DeserializeOwned + rw_types::allocation::DecodeAllocation>(
    line: &[u8],
    next: u64,
) -> Result<EventEnvelope<T>, SessionStoreError> {
    let envelope: EventEnvelope<T> = serde_json::from_slice(line)?;
    if envelope.schema_version != EVENT_SCHEMA_VERSION {
        return Err(SessionStoreError::UnsupportedEventVersion(
            envelope.schema_version,
        ));
    }
    if envelope.sequence.0 != next || line.last() != Some(&b'\n') {
        return Err(SessionStoreError::CorruptEvent(
            "non-contiguous pinned journal page",
        ));
    }
    Ok(envelope)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod proof_tests;

#[cfg(test)]
mod telemetry_tests;
