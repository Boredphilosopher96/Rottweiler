//! Bounded immutable journal segments and descriptor-pinned read views (ADR-029).

use super::{
    EVENT_SCHEMA_VERSION, EventEnvelope, SessionEventPage, SessionEventPageLimits,
    SessionStoreError,
};
use rw_types::SequenceId;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
#[cfg(not(unix))]
use std::io::Read as _;
use std::{
    fs::{self, File},
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
};

const SEGMENT_TARGET_BYTES: usize = 1024 * 1024;
const MAX_SEGMENT_BYTES: usize = 16 * 1024 * 1024;
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

#[derive(Debug)]
struct Directory {
    file: File,
    path: PathBuf,
}

impl Directory {
    fn open(root: &Path, session_id: &str, create: bool) -> Result<Self, SessionStoreError> {
        super::validate_session_id(session_id)?;
        let path = root.join("sessions").join(session_id).join("journal");
        #[cfg(unix)]
        {
            if create {
                fs::create_dir_all(root)?;
            }
            let mut parent = File::open(root)?;
            for name in ["sessions", session_id, "journal"] {
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
                super::create_checked_directory_portable(root)?;
            }
            for directory in [
                root.join("sessions"),
                root.join("sessions").join(session_id),
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

    fn catalog(&self) -> Result<Vec<Segment>, SessionStoreError> {
        let mut segments = Vec::new();
        for name in self.names()? {
            if name == "active.jsonl" || name == "writer.lock" {
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

#[derive(Default)]
struct BoundedBatch {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl std::io::Write for BoundedBatch {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_SEGMENT_BYTES - self.bytes.len() {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "journal batch exceeds its byte limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Exclusive append owner. Sealed segment identities form its sparse index.
#[derive(Debug)]
pub struct SegmentedJournal {
    directory: Arc<Directory>,
    _writer_lock: File,
    segments: Arc<Vec<Segment>>,
    active: Arc<File>,
    active_first: u64,
    active_bytes: u64,
    active_hash: blake3::Hasher,
    active_state: super::EventFileSnapshot,
    sealed_bytes: u64,
    sealed_identity: blake3::Hash,
    next_sequence: u64,
    poisoned: bool,
}

/// Immutable logical prefix; later append/rotation cannot change its tail.
#[derive(Clone, Debug)]
pub struct JournalReadView {
    directory: Arc<Directory>,
    segments: Arc<Vec<Segment>>,
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
    /// Opens the journal, repairing only an incomplete active-tail record.
    /// Sealed historical checksums are verified on access or by `verify_all`.
    ///
    /// # Errors
    /// Rejects unsafe paths/descriptors, conflicting writers and corrupt tails.
    pub fn open(root: &Path, session_id: &str) -> Result<Self, SessionStoreError> {
        let directory = Arc::new(Directory::open(root, session_id, true)?);
        let writer_lock = directory.file("writer.lock", true, true)?;
        super::lock_writer(&writer_lock)?;
        let segments = Arc::new(directory.catalog()?);
        let active_first = segments.last().map_or(0, |segment| segment.next);
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
        let records = super::parse_events_bounded_from_sequence::<serde_json::Value>(
            &bytes,
            usize::MAX,
            active_first,
        )?;
        let next_sequence = active_first
            .checked_add(records.len() as u64)
            .ok_or(SessionStoreError::SequenceOverflow)?;
        let mut active_hash = blake3::Hasher::new();
        active_hash.update(&bytes);
        let active_state = super::event_file_snapshot(&active)?;
        let sealed_bytes = segments.iter().map(|segment| segment.bytes).sum();
        let sealed_identity = segments
            .iter()
            .fold(blake3::hash(b""), |identity, segment| {
                segment.extend_identity(identity)
            });
        Ok(Self {
            sealed_identity,
            active_state,
            sealed_bytes,
            directory,
            _writer_lock: writer_lock,
            segments,
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
    pub fn append_batch<T: Serialize>(
        &mut self,
        events: impl IntoIterator<Item = T>,
    ) -> Result<Vec<EventEnvelope<T>>, SessionStoreError> {
        if self.poisoned {
            return Err(SessionStoreError::EventWriterPoisoned);
        }
        if let Err(error) = super::verify_event_file_snapshot(&self.active, &self.active_state) {
            self.poisoned = true;
            return Err(error);
        }
        let mut bounded = BoundedBatch::default();
        let mut envelopes = Vec::new();
        for event in events {
            let sequence = self
                .next_sequence
                .checked_add(envelopes.len() as u64)
                .ok_or(SessionStoreError::SequenceOverflow)?;
            let envelope = EventEnvelope {
                schema_version: EVENT_SCHEMA_VERSION,
                sequence: SequenceId(sequence),
                event,
            };
            let serialized = serde_json::to_writer(&mut bounded, &envelope);
            if bounded.exceeded {
                return Err(SessionStoreError::EventRecordTooLarge {
                    max_line_bytes: MAX_SEGMENT_BYTES,
                });
            }
            serialized?;
            if bounded.bytes.len() == MAX_SEGMENT_BYTES {
                return Err(SessionStoreError::EventRecordTooLarge {
                    max_line_bytes: MAX_SEGMENT_BYTES,
                });
            }
            bounded.bytes.push(b'\n');
            envelopes.push(envelope);
        }
        let bytes = bounded.bytes;
        let next_sequence = self
            .next_sequence
            .checked_add(envelopes.len() as u64)
            .ok_or(SessionStoreError::SequenceOverflow)?;
        if envelopes.is_empty() {
            return Ok(envelopes);
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
        let append = (&*self.active)
            .write_all(&bytes)
            .and_then(|()| super::sync_event_file(&self.active));
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
        Ok(envelopes)
    }

    fn seal(&mut self) -> Result<(), SessionStoreError> {
        if self.segments.len() >= MAX_SEGMENTS {
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
        Arc::make_mut(&mut self.segments).push(segment);
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
            active: Arc::clone(&self.active),
            active_segment,
            next_sequence: self.next_sequence,
            total_bytes: self.sealed_bytes + self.active_bytes,
        }
    }
}

impl JournalReadView {
    /// Captures an existing offline journal without modifying it.
    ///
    /// A live owner must supply its own [`SegmentedJournal::read_view`]. Taking
    /// the ownership lock prevents mistaking written but unsynchronized active
    /// records for an acknowledged tail. The lock is released after capture.
    ///
    /// # Errors
    /// Rejects a live writer, unsafe files, corrupt complete records, or an
    /// interrupted tail which must first be recovered by the writer.
    pub fn open_existing(root: &Path, session_id: &str) -> Result<Option<Self>, SessionStoreError> {
        let directory = match Directory::open(root, session_id, false) {
            Ok(directory) => Arc::new(directory),
            Err(SessionStoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let ownership = directory.file("writer.lock", false, false)?;
        super::lock_writer(&ownership)?;
        let segments = Arc::new(directory.catalog()?);
        let active = Arc::new(directory.file("active.jsonl", false, false)?);
        let bytes = super::read_opened_file_bounded(&active, MAX_SEGMENT_BYTES as u64)?;
        if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
            return Err(SessionStoreError::CorruptEvent(
                "active journal tail requires writer recovery",
            ));
        }
        let first = segments.last().map_or(0, |segment| segment.next);
        let records = super::parse_events_bounded_from_sequence::<serde_json::Value>(
            &bytes,
            usize::MAX,
            first,
        )?;
        let next_sequence = first
            .checked_add(records.len() as u64)
            .ok_or(SessionStoreError::SequenceOverflow)?;
        let active_segment = (!bytes.is_empty()).then(|| Segment {
            first,
            next: next_sequence,
            bytes: bytes.len() as u64,
            digest: blake3::hash(&bytes),
            name: "active.jsonl".to_owned(),
        });
        let total_bytes =
            segments.iter().map(|segment| segment.bytes).sum::<u64>() + bytes.len() as u64;
        let digest = segments
            .iter()
            .chain(active_segment.iter())
            .fold(blake3::hash(b""), |identity, segment| {
                segment.extend_identity(identity)
            });
        Ok(Some(Self {
            directory,
            segments,
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

    fn segment_bytes(&self, segment: &Segment, active: bool) -> Result<Vec<u8>, SessionStoreError> {
        let file = if active {
            Arc::clone(&self.active)
        } else {
            Arc::new(self.directory.file(&segment.name, false, false)?)
        };
        let before = super::event_file_snapshot(&file)?;
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
        if blake3::hash(&bytes) != segment.digest {
            return Err(SessionStoreError::CorruptEvent(
                "pinned journal segment checksum changed",
            ));
        }
        if active {
            super::event_file_snapshot(&file)?;
        } else {
            super::verify_event_file_snapshot(&file, &before)?;
        }
        Ok(bytes)
    }

    fn page_segment_bytes(
        &self,
        segment: &Segment,
        active: bool,
        limits: SessionEventPageLimits,
        metrics: &mut JournalReadMetrics,
    ) -> Result<Vec<u8>, SessionStoreError> {
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
        let bytes = self.segment_bytes(segment, active)?;
        let record_count = memchr::memchr_iter(b'\n', &bytes).count() as u64;
        if record_count != segment_records || bytes.last() != Some(&b'\n') {
            return Err(SessionStoreError::CorruptEvent(
                "journal segment record count differs from index",
            ));
        }
        metrics.bytes_read += segment.bytes;
        metrics.records_scanned += segment_records;
        metrics.segments_read += 1;
        Ok(bytes)
    }

    /// Reads one bounded cursor-exclusive page from the pinned prefix.
    ///
    /// # Errors
    /// Rejects invalid cursors/limits, corrupt referenced segments and oversized records.
    pub fn page<T: DeserializeOwned>(
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
    pub fn page_with_metrics<T: DeserializeOwned>(
        &self,
        after: Option<SequenceId>,
        limits: SessionEventPageLimits,
    ) -> Result<(SessionEventPage<T>, JournalReadMetrics), SessionStoreError> {
        if limits.max_page_events == 0
            || limits.max_page_bytes == 0
            || limits.max_line_bytes == 0
            || limits.max_scan_bytes == 0
            || limits.max_scan_events == 0
        {
            return Err(SessionStoreError::InvalidEventPageLimits);
        }
        let mut metrics = JournalReadMetrics::default();
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
        let mut events = Vec::new();
        let mut page_bytes = 0;
        let mut next = first;
        let first_segment = self
            .segments
            .partition_point(|segment| segment.next <= first);
        'segments: for (segment, active) in self.segments[first_segment..]
            .iter()
            .map(|segment| (segment, false))
            .chain(self.active_segment.iter().map(|segment| (segment, true)))
        {
            if events.len() >= limits.max_page_events || page_bytes >= limits.max_page_bytes {
                break;
            }
            if segment.next <= first {
                continue;
            }
            let bytes = self.page_segment_bytes(segment, active, limits, &mut metrics)?;
            for (index, line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
                let sequence = segment.first + index as u64;
                if sequence < first {
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
        Ok((
            SessionEventPage {
                events,
                page_bytes,
                next_cursor: if next == 0 {
                    None
                } else {
                    Some(SequenceId(next - 1))
                },
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

    /// Checks every historical segment and record with bounded working memory.
    ///
    /// # Errors
    /// Rejects checksum, schema, sequence or descriptor corruption anywhere in the view.
    pub fn verify_all(&self) -> Result<JournalVerification, SessionStoreError> {
        let mut expected = 0;
        let mut verified_bytes = 0;
        for (segment, active) in self
            .segments
            .iter()
            .map(|segment| (segment, false))
            .chain(self.active_segment.iter().map(|segment| (segment, true)))
        {
            let bytes = self.segment_bytes(segment, active)?;
            let events = super::parse_events_bounded_from_sequence::<serde_json::Value>(
                &bytes,
                usize::MAX,
                expected,
            )?;
            expected += events.len() as u64;
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use serde_json::{Value, json};
    use tempfile::tempdir;

    fn page_limits(events: usize) -> SessionEventPageLimits {
        SessionEventPageLimits {
            max_page_events: events,
            ..SessionEventPageLimits::default()
        }
    }

    #[test]
    fn views_pin_their_tail_across_append_rotation_and_writer_reopen() {
        let root = tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "rotation").expect("journal");
        journal
            .append_batch([json!({"text":"first"})])
            .expect("first append");
        let first = journal.read_view();
        journal
            .append_batch([json!({"text":"x".repeat(SEGMENT_TARGET_BYTES)})])
            .expect("rotating append");
        let second = journal.read_view();
        assert_eq!(first.last_sequence(), Some(SequenceId(0)));
        assert_eq!(
            first
                .page::<Value>(None, page_limits(10))
                .expect("old page")
                .events
                .len(),
            1
        );
        assert_eq!(
            second
                .page::<Value>(Some(SequenceId(0)), page_limits(10))
                .expect("new page")
                .events
                .len(),
            1
        );
        assert_eq!(second.verify_all().expect("verify").events, 2);
        drop(journal);
        let mut reopened = SegmentedJournal::open(root.path(), "rotation")
            .expect("reopen without read-view writer lock");
        reopened
            .append_batch([json!({"text":"third"})])
            .expect("third append");
        assert_eq!(second.verify_all().expect("pinned verification").events, 2);
        assert_eq!(
            reopened
                .read_view()
                .verify_all()
                .expect("latest verification")
                .events,
            3
        );
    }

    #[test]
    fn pages_are_bounded_and_cursor_exclusive() {
        let root = tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "pages").expect("journal");
        journal
            .append_batch((0..200).map(|value| json!({"value":value})))
            .expect("append");
        let view = journal.read_view();
        let page = view
            .page::<Value>(Some(SequenceId(178)), page_limits(10))
            .expect("page");
        assert_eq!(page.events.len(), 10);
        assert_eq!(page.events[0].sequence, SequenceId(179));
        assert_eq!(page.next_cursor, Some(SequenceId(188)));
        assert_eq!(page.events_before_page, 179);
        assert_eq!(page.events_after_page, 11);
        assert!(page.has_more);
        let tail = view
            .page::<Value>(Some(SequenceId(199)), page_limits(10))
            .expect("tail");
        assert!(tail.events.is_empty());
        assert!(!tail.has_more);
        assert!(matches!(
            view.page::<Value>(Some(SequenceId(200)), page_limits(10)),
            Err(SessionStoreError::EventPageCursorAhead)
        ));
        assert!(matches!(
            view.page::<Value>(None, page_limits(0)),
            Err(SessionStoreError::InvalidEventPageLimits)
        ));
        let tiny = SessionEventPageLimits {
            max_page_bytes: 1,
            ..page_limits(10)
        };
        assert!(matches!(
            view.page::<Value>(None, tiny),
            Err(SessionStoreError::EventPageByteLimitTooSmall { .. })
        ));
    }

    #[test]
    fn referenced_segment_work_respects_scan_budgets_and_reports_actual_io() {
        let root = tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "budgets").expect("journal");
        journal
            .append_batch((0..200).map(|value| json!({"value":value})))
            .expect("append");
        let view = journal.read_view();
        let (page, metrics) = view
            .page_with_metrics::<Value>(Some(SequenceId(198)), page_limits(1))
            .expect("tail");
        assert_eq!(page.events.len(), 1);
        assert_eq!(
            metrics,
            JournalReadMetrics {
                bytes_read: view.total_bytes,
                records_scanned: 200,
                records_decoded: 1,
                segments_read: 1,
            }
        );
        let (_, empty_metrics) = view
            .page_with_metrics::<Value>(Some(SequenceId(199)), page_limits(1))
            .expect("empty tail");
        assert_eq!(empty_metrics, JournalReadMetrics::default());
        assert!(matches!(
            view.page::<Value>(
                Some(SequenceId(198)),
                SessionEventPageLimits {
                    max_scan_bytes: view.total_bytes - 1,
                    ..page_limits(1)
                }
            ),
            Err(SessionStoreError::EventScanBytesExceeded { .. })
        ));
        assert!(matches!(
            view.page::<Value>(
                Some(SequenceId(198)),
                SessionEventPageLimits {
                    max_scan_events: 199,
                    ..page_limits(1)
                }
            ),
            Err(SessionStoreError::EventScanCountExceeded { .. })
        ));
        assert!(matches!(
            view.page::<Value>(
                None,
                SessionEventPageLimits {
                    max_scan_events: 0,
                    ..page_limits(1)
                }
            ),
            Err(SessionStoreError::InvalidEventPageLimits)
        ));
    }

    #[test]
    fn rejected_batch_leaves_the_committed_prefix_and_writer_usable() {
        let root = tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "oversized").expect("journal");
        journal.append_batch([json!("first")]).expect("append");
        let identity = journal.read_view().prefix_identity();
        assert!(matches!(
            journal.append_batch([json!("x".repeat(MAX_SEGMENT_BYTES))]),
            Err(SessionStoreError::EventRecordTooLarge { .. })
        ));
        assert_eq!(journal.read_view().prefix_identity(), identity);
        journal
            .append_batch([json!("next")])
            .expect("usable after rejection");
        assert_eq!(journal.read_view().verify_all().expect("verify").events, 2);
    }

    #[test]
    fn segment_publication_never_overwrites_a_collision_and_poison_is_explicit() {
        let root = tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "collision").expect("journal");
        journal.append_batch([json!("first")]).expect("append");
        let name = Segment::name(0, 1, journal.active_bytes, journal.active_hash.finalize());
        let collision = journal.path().join(name);
        fs::write(&collision, b"existing").expect("collision");
        assert!(journal.seal().is_err());
        assert_eq!(fs::read(collision).expect("read collision"), b"existing");
        assert!(matches!(
            journal.append_batch([json!("second")]),
            Err(SessionStoreError::EventWriterPoisoned)
        ));
    }

    #[test]
    fn prefix_identity_survives_rotation_and_recovery_after_active_rename() {
        let root = tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "publication").expect("journal");
        journal.append_batch([json!("first")]).expect("append");
        let identity = journal.read_view().prefix_identity();
        journal.seal().expect("seal");
        assert_eq!(journal.read_view().prefix_identity(), identity);
        journal.append_batch([json!("second")]).expect("append");
        let expected = journal.read_view().prefix_identity();
        let name = Segment::name(
            journal.active_first,
            journal.next_sequence,
            journal.active_bytes,
            journal.active_hash.finalize(),
        );
        journal
            .directory
            .rename("active.jsonl", &name)
            .expect("simulate crash after publication before new active");
        drop(journal);
        let mut recovered = SegmentedJournal::open(root.path(), "publication").expect("recover");
        assert_eq!(recovered.read_view().prefix_identity(), expected);
        recovered.append_batch([json!("third")]).expect("continue");
        assert_eq!(
            recovered.read_view().verify_all().expect("verify").events,
            3
        );
    }

    #[test]
    #[ignore = "writes 1M events; run explicitly for storage scaling evidence"]
    fn journal_tail_read_scaling_metrics() {
        use std::time::Instant;
        let root = tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "scaling").expect("journal");
        let mut count = 0;
        for target in [10_000_u64, 100_000, 1_000_000] {
            while count < target {
                let end = (count + 1_000).min(target);
                journal
                    .append_batch(
                        (count..end)
                            .map(|value| json!({"value":value,"message":"bounded tail fixture"})),
                    )
                    .expect("batch");
                count = end;
            }
            let identity = journal.read_view().prefix_identity();
            let active_bytes = journal.active_bytes;
            let segment_count = journal.segments.len() + 1;
            drop(journal);
            let started = Instant::now();
            journal = SegmentedJournal::open(root.path(), "scaling").expect("reopen");
            let open_micros = started.elapsed().as_micros();
            assert_eq!(journal.read_view().prefix_identity(), identity);
            let view_started = Instant::now();
            let view = journal.read_view();
            let capture_nanos = view_started.elapsed().as_nanos();
            let read_started = Instant::now();
            let (page, metrics) = view
                .page_with_metrics::<Value>(Some(SequenceId(target - 101)), page_limits(100))
                .expect("tail");
            let read_micros = read_started.elapsed().as_micros();
            assert_eq!(page.events.len(), 100);
            assert_eq!(page.events[0].sequence, SequenceId(target - 100));
            assert!(!page.has_more);
            assert_eq!(metrics.records_decoded, 100);
            assert!(metrics.bytes_read <= 2 * MAX_SEGMENT_BYTES as u64);
            assert!(metrics.segments_read <= 2);
            println!(
                "{}",
                json!({
                    "events":target,"total_bytes":view.total_bytes,"segments":segment_count,
                    "tail_bytes_read":metrics.bytes_read,"tail_records_scanned":metrics.records_scanned,
                    "tail_records_decoded":metrics.records_decoded,"tail_segments_read":metrics.segments_read,
                    "page_bytes":page.page_bytes,"open_active_bytes":active_bytes,
                    "open_micros":open_micros,"capture_nanos":capture_nanos,"tail_read_micros":read_micros,
                    "scope":"store only; open excludes engine projection; timings are diagnostic, not calibrated gates"
                })
            );
        }
    }

    #[test]
    fn offline_views_capture_only_unowned_journals_and_release_ownership_before_reading() {
        let root = tempdir().expect("root");
        assert!(
            JournalReadView::open_existing(root.path(), "absent")
                .expect("absent")
                .is_none()
        );
        let mut journal = SegmentedJournal::open(root.path(), "offline").expect("journal");
        journal.append_batch([json!("first")]).expect("append");
        let identity = journal.read_view().prefix_identity();
        assert!(JournalReadView::open_existing(root.path(), "offline").is_err());
        drop(journal);
        let view = JournalReadView::open_existing(root.path(), "offline")
            .expect("capture")
            .expect("exists");
        assert_eq!(view.prefix_identity(), identity);
        let mut reopened =
            SegmentedJournal::open(root.path(), "offline").expect("capture releases ownership");
        reopened.append_batch([json!("second")]).expect("append");
        assert_eq!(view.verify_all().expect("verify offline view").events, 1);
        assert_eq!(
            view.page::<Value>(None, page_limits(10))
                .expect("page")
                .events
                .len(),
            1
        );
    }

    #[test]
    fn old_bitrot_is_detected_on_access_or_explicit_full_verification() {
        let root = tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "bitrot").expect("journal");
        journal
            .append_batch([json!({"text":"x".repeat(SEGMENT_TARGET_BYTES)})])
            .expect("first append");
        journal
            .append_batch([json!({"text":"latest"})])
            .expect("seal old segment");
        let view = journal.read_view();
        let old = journal.path().join(&view.segments[0].name);
        let mut bytes = fs::read(&old).expect("read old segment");
        bytes[0] = b'[';
        fs::write(old, bytes).expect("simulate old bitrot");
        assert_eq!(
            view.page::<Value>(Some(SequenceId(0)), page_limits(1))
                .expect("unrelated tail")
                .events
                .len(),
            1
        );
        assert!(view.page::<Value>(None, page_limits(1)).is_err());
        assert!(view.verify_all().is_err());
    }

    #[test]
    fn incomplete_active_tail_repairs_but_complete_corruption_fails() {
        let root = tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "repair").expect("journal");
        journal
            .append_batch([json!({"first":true})])
            .expect("append");
        let active = journal.path().join("active.jsonl");
        let length = fs::metadata(&active).expect("metadata").len();
        drop(journal);
        fs::OpenOptions::new()
            .append(true)
            .open(&active)
            .expect("active")
            .write_all(b"{partial")
            .expect("torn append");
        let repaired = SegmentedJournal::open(root.path(), "repair").expect("repair");
        assert_eq!(repaired.next_sequence(), 1);
        assert_eq!(fs::metadata(&active).expect("metadata").len(), length);
        drop(repaired);
        fs::OpenOptions::new()
            .append(true)
            .open(&active)
            .expect("active")
            .write_all(b"{invalid}\n")
            .expect("corrupt complete record");
        assert!(SegmentedJournal::open(root.path(), "repair").is_err());
    }

    #[test]
    fn single_writer_lock_is_independent_of_segment_rotation() {
        let root = tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "writer").expect("journal");
        journal
            .append_batch([json!({"text":"x".repeat(SEGMENT_TARGET_BYTES)})])
            .expect("append");
        journal
            .append_batch([json!({"text":"next"})])
            .expect("rotate");
        assert!(SegmentedJournal::open(root.path(), "writer").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_segment_descriptors_and_pinned_active_mutation_fail_closed() {
        use std::os::unix::fs::symlink;
        let root = tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "unsafe").expect("journal");
        journal
            .append_batch([json!({"text":"x".repeat(SEGMENT_TARGET_BYTES)})])
            .expect("first append");
        journal
            .append_batch([json!({"text":"active"})])
            .expect("rotate");
        let view = journal.read_view();
        let sealed = journal.path().join(&view.segments[0].name);
        let backup = root.path().join("backup");
        fs::rename(&sealed, &backup).expect("move segment");
        symlink(&backup, &sealed).expect("symlink");
        assert!(view.page::<Value>(None, page_limits(1)).is_err());
        fs::remove_file(&sealed).expect("unlink symlink");
        fs::hard_link(&backup, &sealed).expect("hardlink");
        assert!(view.page::<Value>(None, page_limits(1)).is_err());
        fs::write(journal.path().join("active.jsonl"), b"changed\n").expect("mutate active");
        assert!(
            view.page::<Value>(Some(SequenceId(0)), page_limits(1))
                .is_err()
        );
    }
}
