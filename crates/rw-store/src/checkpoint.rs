//! Content-addressed touched-file checkpoints and deterministic rewind.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use rw_types::{ReviewFileDecision, SessionId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MANIFEST_VERSION: u16 = 1;
const OPAQUE_PENDING_VERSION: u16 = 1;
const REWIND_TRANSACTION_VERSION: u16 = 1;
const REVIEW_LEDGER_VERSION: u16 = 1;
const MAX_REVIEW_FILES: usize = 1_024;
const MAX_REVIEW_FILE_BYTES: usize = 256 * 1024;
const MAX_REVIEW_IDENTITY_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REVIEW_TOTAL_DIFF_BYTES: usize = 2 * 1024 * 1024;
const MAX_CAPTURE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const CAPTURE_CHUNK_BYTES: usize = 64 * 1024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
type CapturedRegular = (File, Option<u32>);
type CapturedReview = (ReviewCurrentState, Option<Vec<u8>>);

/// Pre-mutation state for one workspace-relative path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CheckpointFileState {
    /// File bytes are available in the content-addressed store.
    Present {
        /// Lowercase BLAKE3 digest.
        blob: String,
        /// Original byte length.
        bytes: u64,
        /// Unix permission bits where available.
        unix_mode: Option<u32>,
    },
    /// The path did not exist before the mutation.
    Absent,
    /// The mutation touched a path whose prior state was never captured.
    Unrestorable {
        /// Sanitized explanation surfaced by review/rewind.
        reason: String,
    },
}

/// Versioned per-turn manifest of files affected by a mutating tool.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointManifest {
    /// Manifest schema version.
    pub version: u16,
    /// Stable session id.
    pub session_id: String,
    /// Turn whose mutation this manifest precedes.
    pub turn: u64,
    /// Workspace-relative slash-normalized paths in deterministic order.
    pub files: BTreeMap<String, CheckpointFileState>,
}

/// Result of applying every manifest after a requested turn in reverse order.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RewindReport {
    /// Paths restored from blobs, including repeated historical restores.
    pub restored: Vec<String>,
    /// Paths removed because their prior state was absent.
    pub removed: Vec<String>,
    /// Paths that cannot honestly be restored.
    pub unrestorable: BTreeMap<String, String>,
}

/// Durable identity returned before an opaque command may execute.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpaqueMutation {
    /// Stable session id.
    pub session_id: String,
    /// Turn containing the opaque command.
    pub turn: u64,
}

/// Caller-supplied identity of a two-phase rewind operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RewindHandle {
    /// Stable session id.
    pub session_id: String,
    /// Stable request/command id used to deduplicate recovery events.
    pub operation_id: String,
}

/// Workspace commit which must be recorded in the conversation before ack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindCommit {
    /// Durable operation identity to put in the conversation event.
    pub handle: RewindHandle,
    /// Conversation turn retained after rewind.
    pub target_turn: u64,
    /// Final restoration result.
    pub report: RewindReport,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum InventoryEntry {
    Regular { digest: String },
    Symlink { target: String },
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitTrackedEntry {
    object_id: String,
    unix_mode: Option<u32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct GitTrackedBaseline {
    entries: BTreeMap<String, GitTrackedEntry>,
    paths: BTreeSet<String>,
    complete: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct GitDirtyPaths {
    paths: BTreeSet<String>,
    complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpaquePending {
    version: u16,
    session_id: String,
    turn: u64,
    before: BTreeMap<String, InventoryEntry>,
    tracked: BTreeMap<String, GitTrackedEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RewindStep {
    path: String,
    state: CheckpointFileState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RewindPhase {
    Applying,
    WorkspaceCommitted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RewindTransaction {
    version: u16,
    handle: RewindHandle,
    target_turn: u64,
    steps: Vec<RewindStep>,
    next_step: usize,
    report: RewindReport,
    phase: RewindPhase,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum ReviewCurrentState {
    Present {
        content_blake3: String,
        bytes: u64,
        unix_mode: Option<u32>,
    },
    Absent,
    Unsupported {
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewDecisionRecord {
    decision: ReviewFileDecision,
    current: ReviewCurrentState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewLedger {
    version: u16,
    session_id: String,
    files: BTreeMap<String, ReviewDecisionRecord>,
}

/// Checkpoint storage bound to one canonical workspace root.
#[derive(Clone, Debug)]
pub struct CheckpointStore {
    root: PathBuf,
    workspace: PathBuf,
    storage_relative: Option<String>,
    git_program: PathBuf,
}

mod capture;
mod git;
mod operation;
pub use operation::{CheckpointCancellation, CheckpointOperation};
mod review;
mod rewind;

fn baseline_matches_current(baseline: &CheckpointFileState, current: &ReviewCurrentState) -> bool {
    match (baseline, current) {
        (
            CheckpointFileState::Present {
                blob,
                bytes,
                unix_mode,
            },
            ReviewCurrentState::Present {
                content_blake3,
                bytes: current_bytes,
                unix_mode: current_mode,
            },
        ) => blob == content_blake3 && bytes == current_bytes && unix_mode == current_mode,
        (CheckpointFileState::Absent, ReviewCurrentState::Absent) => true,
        (
            CheckpointFileState::Present { .. }
            | CheckpointFileState::Absent
            | CheckpointFileState::Unrestorable { .. },
            ReviewCurrentState::Present { .. }
            | ReviewCurrentState::Absent
            | ReviewCurrentState::Unsupported { .. },
        ) => false,
    }
}

fn review_identity(value: &impl Serialize) -> Result<String, CheckpointError> {
    Ok(blake3::hash(&serde_json::to_vec(value)?)
        .to_hex()
        .to_string())
}

fn validate_review_current(current: &ReviewCurrentState) -> Result<(), CheckpointError> {
    match current {
        ReviewCurrentState::Present {
            content_blake3,
            unix_mode,
            ..
        } if !is_lower_blake3(content_blake3) || unix_mode.is_some_and(|mode| mode > 0o7777) => {
            Err(CheckpointError::CorruptReviewLedger)
        }
        ReviewCurrentState::Unsupported { reason }
            if reason.is_empty()
                || reason.len() > 1_024
                || reason.chars().any(char::is_control) =>
        {
            Err(CheckpointError::CorruptReviewLedger)
        }
        ReviewCurrentState::Present { .. }
        | ReviewCurrentState::Absent
        | ReviewCurrentState::Unsupported { .. } => Ok(()),
    }
}

fn render_whole_file_diff(
    path: &str,
    original: Option<&[u8]>,
    current: Option<&[u8]>,
    limit: usize,
) -> (String, bool) {
    let original_text = original.map(std::str::from_utf8).transpose();
    let current_text = current.map(std::str::from_utf8).transpose();
    let (Ok(original_text), Ok(current_text)) = (original_text, current_text) else {
        let (message, _) = bounded_diff_text("Binary files differ\n", limit);
        return (message, true);
    };
    let escaped_path = path.escape_default().to_string();
    let original_header = original.map_or("/dev/null".to_owned(), |_| format!("a/{escaped_path}"));
    let current_header = current.map_or("/dev/null".to_owned(), |_| format!("b/{escaped_path}"));
    let mut config = similar::TextDiff::configure();
    config.timeout(std::time::Duration::from_millis(50));
    let diff = config.diff_lines(original_text.unwrap_or(""), current_text.unwrap_or(""));
    let output = diff
        .unified_diff()
        .context_radius(3)
        .header(&original_header, &current_header)
        .to_string();
    bounded_diff_text(&output, limit)
}

fn bounded_diff_text(value: &str, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value.to_owned(), false);
    }
    let mut boundary = limit.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (value[..boundary].to_owned(), true)
}

fn normalize_relative(path: &Path) -> Result<String, CheckpointError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(CheckpointError::UnsafePath);
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().ok_or(CheckpointError::UnsafePath)?;
                if value.is_empty() {
                    return Err(CheckpointError::UnsafePath);
                }
                parts.push(value);
            }
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => return Err(CheckpointError::UnsafePath),
        }
    }
    if parts.is_empty() {
        return Err(CheckpointError::UnsafePath);
    }
    Ok(parts.join("/"))
}

fn validate_session_id(value: &str) -> Result<(), CheckpointError> {
    SessionId::validate(value).map_err(|_| CheckpointError::InvalidSessionId)
}

fn validate_operation_id(value: &str) -> Result<(), CheckpointError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(CheckpointError::InvalidOperationId);
    }
    Ok(())
}

fn changed_inventory_paths(
    before: &BTreeMap<String, InventoryEntry>,
    after: &BTreeMap<String, InventoryEntry>,
) -> BTreeSet<String> {
    before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect()
}

fn is_lower_blake3(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_rewind_report(report: &RewindReport) -> Result<(), CheckpointError> {
    for path in report
        .restored
        .iter()
        .chain(&report.removed)
        .chain(report.unrestorable.keys())
    {
        if normalize_relative(Path::new(path))? != *path {
            return Err(CheckpointError::CorruptRewindTransaction);
        }
    }
    if report.unrestorable.values().any(|reason| {
        reason.is_empty() || reason.len() > 1_024 || reason.chars().any(char::is_control)
    }) {
        return Err(CheckpointError::CorruptRewindTransaction);
    }
    Ok(())
}

fn parse_exact_turn_filename(filename: &std::ffi::OsStr) -> Option<u64> {
    let filename = filename.to_str()?;
    if filename.len() != 25 || filename.as_bytes().get(20..) != Some(b".json") {
        return None;
    }
    let digits = &filename[..20];
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn is_private_temporary(filename: &std::ffi::OsStr) -> bool {
    let Some(filename) = filename.to_str() else {
        return false;
    };
    let Some(body) = filename
        .strip_prefix(".rw-")
        .and_then(|value| value.strip_suffix(".tmp"))
    else {
        return false;
    };
    let Some((pid, nonce)) = body.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && !nonce.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && nonce.bytes().all(|byte| byte.is_ascii_digit())
}

fn cleanup_stale_temporaries_in(directory: &Path) -> Result<(), CheckpointError> {
    if !directory.exists() {
        return Ok(());
    }
    let mut removed = false;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if is_private_temporary(&entry.file_name()) {
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                fs::remove_dir_all(entry.path())?;
                removed = true;
            } else if metadata.is_file() || metadata.file_type().is_symlink() {
                fs::remove_file(entry.path())?;
                removed = true;
            }
        }
    }
    if removed {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

fn remove_durable(path: &Path) -> Result<(), CheckpointError> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                File::open(parent)?.sync_all()?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn digest_os_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    blake3::hash(path.as_os_str().as_bytes())
        .to_hex()
        .to_string()
}

#[cfg(not(unix))]
fn digest_os_path(path: &Path) -> String {
    blake3::hash(path.as_os_str().to_string_lossy().as_bytes())
        .to_hex()
        .to_string()
}

#[cfg(unix)]
fn open_workspace_root(workspace: &Path) -> Result<std::os::fd::OwnedFd, CheckpointError> {
    use rustix::fs::{Mode, OFlags};
    rustix::fs::open(
        workspace,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| std::io::Error::from(error).into())
}

#[cfg(unix)]
fn open_confined_parent(
    workspace: &Path,
    key: &str,
    create: bool,
) -> Result<Option<(std::os::fd::OwnedFd, String)>, CheckpointError> {
    use rustix::fs::{Mode, OFlags};
    let normalized = normalize_relative(Path::new(key))?;
    if normalized != key {
        return Err(CheckpointError::UnsafePath);
    }
    let mut parts = key.split('/').collect::<Vec<_>>();
    let name = parts.pop().ok_or(CheckpointError::UnsafePath)?.to_owned();
    let mut directory = open_workspace_root(workspace)?;
    for part in parts {
        match rustix::fs::openat(
            &directory,
            part,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(next) => directory = next,
            Err(rustix::io::Errno::NOENT) if !create => return Ok(None),
            Err(rustix::io::Errno::NOENT) => {
                rustix::fs::mkdirat(&directory, part, Mode::from_raw_mode(0o700))
                    .map_err(std::io::Error::from)?;
                rustix::fs::fsync(&directory).map_err(std::io::Error::from)?;
                directory = rustix::fs::openat(
                    &directory,
                    part,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(std::io::Error::from)?;
            }
            Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => {
                return Err(CheckpointError::UnsupportedFileKind(key.to_owned()));
            }
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
    }
    Ok(Some((directory, name)))
}

#[cfg(unix)]
fn capture_regular_confined(
    workspace: &Path,
    key: &str,
) -> Result<Option<CapturedRegular>, CheckpointError> {
    use rustix::fs::{FileType, Mode, OFlags};
    let Some((parent, name)) = open_confined_parent(workspace, key, false)? else {
        return Ok(None);
    };
    let descriptor = match rustix::fs::openat(
        &parent,
        name.as_str(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP | rustix::io::Errno::ISDIR) => {
            return Err(CheckpointError::UnsupportedFileKind(key.to_owned()));
        }
        Err(error) => return Err(std::io::Error::from(error).into()),
    };
    let stat = rustix::fs::fstat(&descriptor).map_err(std::io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(CheckpointError::UnsupportedFileKind(key.to_owned()));
    }
    #[cfg(target_os = "linux")]
    let mode = Some(Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o7777);
    #[cfg(not(target_os = "linux"))]
    let mode = Some(u32::from(
        Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o7777,
    ));
    Ok(Some((File::from(descriptor), mode)))
}

#[cfg(unix)]
fn capture_review_regular_confined(
    workspace: &Path,
    key: &str,
) -> Result<Option<CapturedReview>, CheckpointError> {
    use rustix::fs::{FileType, Mode, OFlags};
    let Some((parent, name)) = open_confined_parent(workspace, key, false)? else {
        return Ok(None);
    };
    let descriptor = match rustix::fs::openat(
        &parent,
        name.as_str(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP | rustix::io::Errno::ISDIR) => {
            return Err(CheckpointError::UnsupportedFileKind(key.to_owned()));
        }
        Err(error) => return Err(std::io::Error::from(error).into()),
    };
    let stat = rustix::fs::fstat(&descriptor).map_err(std::io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(CheckpointError::UnsupportedFileKind(key.to_owned()));
    }
    #[cfg(target_os = "linux")]
    let mode = Some(Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o7777);
    #[cfg(not(target_os = "linux"))]
    let mode = Some(u32::from(
        Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o7777,
    ));
    capture_review_open_file(File::from(descriptor), mode).map(Some)
}

fn capture_review_open_file(
    mut file: File,
    unix_mode: Option<u32>,
) -> Result<CapturedReview, CheckpointError> {
    let before = file.metadata()?;
    if before.len() > MAX_REVIEW_IDENTITY_SCAN_BYTES {
        return Err(CheckpointError::ReviewIdentityLimit);
    }
    let mut hasher = blake3::Hasher::new();
    let mut retained = Vec::new();
    let mut retain_content = true;
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(u64::try_from(count).map_err(|_| CheckpointError::CorruptManifest)?)
            .ok_or(CheckpointError::CorruptManifest)?;
        if bytes > MAX_REVIEW_IDENTITY_SCAN_BYTES {
            return Err(CheckpointError::ReviewIdentityLimit);
        }
        hasher.update(&buffer[..count]);
        if retain_content
            && retained
                .len()
                .checked_add(count)
                .is_some_and(|length| length <= MAX_REVIEW_FILE_BYTES)
        {
            retained.extend_from_slice(&buffer[..count]);
        } else {
            retain_content = false;
            retained.clear();
        }
    }
    let after = file.metadata()?;
    if !same_open_file_identity(&before, &after) || bytes != after.len() {
        return Err(CheckpointError::ReviewPathChanged);
    }
    Ok((
        ReviewCurrentState::Present {
            content_blake3: hasher.finalize().to_hex().to_string(),
            bytes,
            unix_mode,
        },
        retain_content.then_some(retained),
    ))
}

#[cfg(unix)]
fn same_open_file_identity(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.size() == after.size()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

#[cfg(not(unix))]
fn same_open_file_identity(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.len() == after.len()
        && before.modified().ok() == after.modified().ok()
        && before.is_file() == after.is_file()
}

#[cfg(unix)]
fn restore_regular_confined(
    workspace: &Path,
    key: &str,
    bytes: &[u8],
    unix_mode: Option<u32>,
) -> Result<(), CheckpointError> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    let (parent, name) =
        open_confined_parent(workspace, key, true)?.ok_or(CheckpointError::UnsafePath)?;
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = format!(".rw-{}-{nonce}.tmp", std::process::id());
    let descriptor = rustix::fs::openat(
        &parent,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(std::io::Error::from)?;
    let mut file = File::from(descriptor);
    let result = (|| -> Result<(), CheckpointError> {
        file.write_all(bytes)?;
        file.flush()?;
        if let Some(mode) = unix_mode {
            #[cfg(target_os = "linux")]
            rustix::fs::fchmod(&file, Mode::from_raw_mode(mode)).map_err(std::io::Error::from)?;
            #[cfg(not(target_os = "linux"))]
            {
                let mode = u16::try_from(mode).map_err(|_| CheckpointError::CorruptManifest)?;
                rustix::fs::fchmod(&file, Mode::from_raw_mode(mode))
                    .map_err(std::io::Error::from)?;
            }
        }
        file.sync_all()?;
        rustix::fs::renameat(&parent, temporary.as_str(), &parent, name.as_str())
            .map_err(std::io::Error::from)?;
        rustix::fs::fsync(&parent).map_err(std::io::Error::from)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&parent, temporary.as_str(), AtFlags::empty());
    }
    result
}

#[cfg(unix)]
fn remove_file_or_symlink_confined(workspace: &Path, key: &str) -> Result<(), CheckpointError> {
    use rustix::fs::{AtFlags, FileType};
    let Some((parent, name)) = open_confined_parent(workspace, key, false)? else {
        return Ok(());
    };
    let stat = match rustix::fs::statat(&parent, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(std::io::Error::from(error).into()),
    };
    let file_type = FileType::from_raw_mode(stat.st_mode);
    if !file_type.is_file() && !file_type.is_symlink() {
        return Err(CheckpointError::UnsupportedFileKind(key.to_owned()));
    }
    rustix::fs::unlinkat(&parent, name.as_str(), AtFlags::empty()).map_err(std::io::Error::from)?;
    rustix::fs::fsync(&parent).map_err(std::io::Error::from)?;
    Ok(())
}

#[cfg(unix)]
fn inventory_confined(
    workspace: &Path,
    storage_relative: Option<&str>,
    operation: &mut CheckpointOperation,
) -> Result<BTreeMap<String, InventoryEntry>, CheckpointError> {
    let root = open_workspace_root(workspace)?;
    let mut inventory = BTreeMap::new();
    inventory_directory_fd(&root, "", storage_relative, &mut inventory, operation)?;
    Ok(inventory)
}

#[cfg(unix)]
fn inventory_directory_fd(
    directory: &std::os::fd::OwnedFd,
    prefix: &str,
    storage_relative: Option<&str>,
    inventory: &mut BTreeMap<String, InventoryEntry>,
    operation: &mut CheckpointOperation,
) -> Result<(), CheckpointError> {
    use std::os::unix::ffi::OsStrExt as _;

    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    let mut entries = rustix::fs::Dir::read_from(directory).map_err(std::io::Error::from)?;
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| CheckpointError::UnsafePath)?;
        if name == "." || name == ".." || (prefix.is_empty() && name == ".git") {
            continue;
        }
        let key = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if storage_relative
            .is_some_and(|storage| key == storage || key.starts_with(&format!("{storage}/")))
        {
            continue;
        }
        operation.path(&key)?;
        let stat = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(std::io::Error::from)?;
        let file_type = FileType::from_raw_mode(stat.st_mode);
        if file_type.is_symlink() {
            let target = rustix::fs::readlinkat(directory, name, Vec::new())
                .map_err(std::io::Error::from)?;
            let target = Path::new(std::ffi::OsStr::from_bytes(target.to_bytes()));
            inventory.insert(
                key,
                InventoryEntry::Symlink {
                    target: digest_os_path(target),
                },
            );
        } else if file_type.is_dir() {
            let child = rustix::fs::openat(
                directory,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?;
            inventory.insert(key.clone(), InventoryEntry::Directory);
            inventory_directory_fd(&child, &key, storage_relative, inventory, operation)?;
        } else if file_type.is_file() {
            let descriptor = rustix::fs::openat(
                directory,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?;
            let current = rustix::fs::fstat(&descriptor).map_err(std::io::Error::from)?;
            if !FileType::from_raw_mode(current.st_mode).is_file() {
                return Err(CheckpointError::UnsupportedFileKind(key));
            }
            inventory.insert(
                key,
                InventoryEntry::Regular {
                    digest: hash_inventory_file(File::from(descriptor), operation)?
                        .to_hex()
                        .to_string(),
                },
            );
        } else {
            return Err(CheckpointError::UnsupportedFileKind(key));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn capture_regular_confined(
    workspace: &Path,
    key: &str,
) -> Result<Option<CapturedRegular>, CheckpointError> {
    let path = checked_workspace_path_fallback(workspace, key)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(Some((File::open(path)?, None))),
        Ok(_) => Err(CheckpointError::UnsupportedFileKind(key.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn capture_review_regular_confined(
    workspace: &Path,
    key: &str,
) -> Result<Option<CapturedReview>, CheckpointError> {
    let path = checked_workspace_path_fallback(workspace, key)?;
    match OpenOptions::new().read(true).open(path) {
        Ok(file) if file.metadata()?.is_file() => capture_review_open_file(file, None).map(Some),
        Ok(_) => Err(CheckpointError::UnsupportedFileKind(key.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn restore_regular_confined(
    workspace: &Path,
    key: &str,
    bytes: &[u8],
    _unix_mode: Option<u32>,
) -> Result<(), CheckpointError> {
    atomic_replace(&checked_workspace_path_fallback(workspace, key)?, bytes)
}

#[cfg(not(unix))]
fn remove_file_or_symlink_confined(workspace: &Path, key: &str) -> Result<(), CheckpointError> {
    let path = checked_workspace_path_fallback(workspace, key)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(path)?;
            Ok(())
        }
        Ok(_) => Err(CheckpointError::UnsupportedFileKind(key.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn inventory_confined(
    workspace: &Path,
    storage_relative: Option<&str>,
    operation: &mut CheckpointOperation,
) -> Result<BTreeMap<String, InventoryEntry>, CheckpointError> {
    fn scan(
        workspace: &Path,
        directory: &Path,
        prefix: &Path,
        storage_relative: Option<&str>,
        output: &mut BTreeMap<String, InventoryEntry>,
        operation: &mut CheckpointOperation,
    ) -> Result<(), CheckpointError> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if prefix.as_os_str().is_empty() && entry.file_name() == ".git" {
                continue;
            }
            let relative = prefix.join(entry.file_name());
            let key = normalize_relative(&relative)?;
            if storage_relative
                .is_some_and(|storage| key == storage || key.starts_with(&format!("{storage}/")))
            {
                continue;
            }
            operation.path(&key)?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                output.insert(
                    key,
                    InventoryEntry::Symlink {
                        target: digest_os_path(&fs::read_link(entry.path())?),
                    },
                );
            } else if metadata.is_dir() {
                output.insert(key, InventoryEntry::Directory);
                scan(
                    workspace,
                    &entry.path(),
                    &relative,
                    storage_relative,
                    output,
                    operation,
                )?;
            } else if metadata.is_file() {
                let file = capture_regular_confined(workspace, &key)?
                    .ok_or(CheckpointError::UnsafePath)?
                    .0;
                output.insert(
                    key,
                    InventoryEntry::Regular {
                        digest: hash_inventory_file(file, operation)?.to_hex().to_string(),
                    },
                );
            }
        }
        Ok(())
    }
    let mut output = BTreeMap::new();
    scan(
        workspace,
        workspace,
        Path::new(""),
        storage_relative,
        &mut output,
        operation,
    )?;
    Ok(output)
}

#[cfg(not(unix))]
fn checked_workspace_path_fallback(
    workspace: &Path,
    key: &str,
) -> Result<PathBuf, CheckpointError> {
    let normalized = normalize_relative(Path::new(key))?;
    if normalized != key {
        return Err(CheckpointError::UnsafePath);
    }
    let path = workspace.join(key);
    let mut current = workspace.to_owned();
    for part in key.split('/') {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(CheckpointError::UnsupportedFileKind(key.to_owned()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

fn hash_reader(mut reader: impl Read) -> Result<blake3::Hash, CheckpointError> {
    let mut hash = blake3::Hasher::new();
    let mut chunk = vec![0_u8; CAPTURE_CHUNK_BYTES].into_boxed_slice();
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok(hash.finalize());
        }
        hash.update(&chunk[..count]);
    }
}

fn hash_inventory_file(
    mut file: File,
    operation: &mut CheckpointOperation,
) -> Result<blake3::Hash, CheckpointError> {
    let before = file.metadata()?;
    let hash = operation.hash(&mut file)?;
    if !same_open_file_identity(&before, &file.metadata()?) {
        return Err(CheckpointError::CaptureChanged);
    }
    Ok(hash)
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), CheckpointError> {
    let parent = path.parent().ok_or(CheckpointError::UnsafePath)?;
    fs::create_dir_all(parent)?;
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".rw-{}-{nonce}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = (|| -> Result<(), CheckpointError> {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// Checkpoint failure with no captured file contents in diagnostics.
#[derive(Debug, Error)]
pub enum CheckpointError {
    /// Worker cancellation stops further scan/capture work, retaining recovery markers.
    #[error("checkpoint operation cancelled")]
    Cancelled,
    /// Aggregate work exceeded the operation allowance.
    #[error("checkpoint operation exceeded {0}")]
    OperationLimit(&'static str),
    /// A preimage cannot be captured within its file byte budget.
    #[error("checkpoint preimage exceeds the 64 MiB file capture limit; mutation was not admitted")]
    CaptureFileLimit,
    /// A concurrent edit invalidated the captured file version.
    #[error("checkpoint file changed while being read; capture must be retried")]
    CaptureChanged,
    /// Session id is not a safe path component.
    #[error("checkpoint session id is invalid")]
    InvalidSessionId,
    /// Rewind operation ids are durable path/event identities.
    #[error("checkpoint rewind operation id is invalid")]
    InvalidOperationId,
    /// Workspace root is not a directory.
    #[error("checkpoint workspace is not a directory")]
    WorkspaceNotDirectory,
    /// A workspace-relative path attempted traversal or used non-Unicode bytes.
    #[error("checkpoint path is not a safe workspace-relative path")]
    UnsafePath,
    /// Only regular files can be captured/restored in M2.
    #[error("checkpoint path has an unsupported file kind: {0}")]
    UnsupportedFileKind(String),
    /// Manifest fields or identity are inconsistent.
    #[error("checkpoint manifest is corrupt")]
    CorruptManifest,
    /// Manifest version is not supported.
    #[error("unsupported checkpoint manifest version {0}")]
    UnsupportedManifestVersion(u16),
    /// Blob content or digest is inconsistent.
    #[error("checkpoint blob is missing or corrupt")]
    CorruptBlob,
    /// An opaque command already has a durable unfinished baseline.
    #[error("an opaque checkpoint mutation is already pending")]
    OpaqueMutationPending,
    /// A durable opaque-command baseline failed validation.
    #[error("opaque checkpoint mutation marker is corrupt")]
    CorruptOpaqueMutation,
    /// Git returned malformed tracked-file baseline data.
    #[error("opaque checkpoint Git baseline is corrupt")]
    GitBaselineCorrupt,
    /// Another rewind for this session must be recovered or acknowledged.
    #[error("a checkpoint rewind is already pending")]
    RewindPending,
    /// Durable rewind identity did not match the caller's handle.
    #[error("checkpoint rewind identity does not match")]
    RewindIdentityMismatch,
    /// A rewind must finish applying before it can be acknowledged.
    #[error("checkpoint rewind workspace is not committed")]
    RewindNotCommitted,
    /// Prepared state may be removed only before the first workspace step.
    #[error("checkpoint rewind cannot be discarded after workspace application begins")]
    RewindCannotDiscard,
    /// A durable rewind transaction failed validation.
    #[error("checkpoint rewind transaction is corrupt")]
    CorruptRewindTransaction,
    /// A durable review decision ledger failed validation.
    #[error("checkpoint review ledger is corrupt")]
    CorruptReviewLedger,
    /// A session touched more files than one bounded review can represent.
    #[error("checkpoint review exceeds its file limit")]
    ReviewFileLimit,
    /// A requested review path was not changed by this session.
    #[error("checkpoint review path is not available")]
    ReviewPathNotFound,
    /// A truncated or unrestorable review entry cannot be safely reverted.
    #[error("checkpoint review path cannot be safely reverted")]
    ReviewPathNotRevertible,
    /// The path changed after the review snapshot displayed to the driver.
    #[error("checkpoint review path changed after it was displayed")]
    ReviewPathChanged,
    /// A current file is too large to fingerprint within the review work bound.
    #[error("checkpoint review identity scan limit exceeded")]
    ReviewIdentityLimit,
    /// Parent and child session identities must differ.
    #[error("checkpoint fork identities conflict")]
    ForkIdentityConflict,
    /// Source and target checkpoint stores must bind the same workspace root.
    #[error("checkpoint fork workspace roots do not match")]
    ForkWorkspaceMismatch,
    /// A child checkpoint namespace already exists.
    #[error("checkpoint fork target already exists")]
    ForkTargetExists,
    /// Filesystem failure.
    #[error("checkpoint storage I/O failed")]
    Io(#[from] std::io::Error),
    /// JSON failure without captured contents.
    #[error("checkpoint manifest JSON is invalid")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests;
