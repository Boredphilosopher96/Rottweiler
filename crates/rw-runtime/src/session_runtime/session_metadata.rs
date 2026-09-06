mod encoding;
use super::MAX_WORKSPACE_ROOTS;
pub(super) use encoding::encode as encode_session_metadata;
use miette::{IntoDiagnostic, Result, miette};
use rw_types::{SequenceId, SessionId, Turn};
use serde::{Deserialize, Serialize};
#[cfg(not(unix))]
use std::io::Read;
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) const SESSION_METADATA_VERSION: u16 = 1;

pub(super) const MAX_SESSION_METADATA_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionMetadata {
    #[serde(deserialize_with = "deserialize_version")]
    pub(super) version: u16,
    pub(super) session_id: String,
    pub(crate) budget_session_id: SessionId,
    pub workspace: PathBuf,
    pub model_alias: String,
    pub(super) initial_session_context: Vec<Turn>,
    pub workspace_generation: u64,
    pub workspace_roots: Vec<PathBuf>,
    pub(super) initial_context_workspace_root_count: usize,
    #[serde(deserialize_with = "deserialize_optional_value")]
    pub(crate) inherited_journal_through: Option<SequenceId>,
    #[serde(deserialize_with = "deserialize_optional_value")]
    pub(super) fork_parent_session_id: Option<String>,
    #[serde(deserialize_with = "deserialize_optional_value")]
    pub(crate) fork_at_turn: Option<u64>,
    #[serde(deserialize_with = "deserialize_optional_value")]
    pub(super) fork_operation_id: Option<String>,
}

fn deserialize_version<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u16, D::Error> {
    let value = u16::deserialize(deserializer)?;
    if value != SESSION_METADATA_VERSION {
        return Err(serde::de::Error::custom(
            "invalid session metadata schema version",
        ));
    }
    Ok(value)
}

fn deserialize_optional_value<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error> {
    Option::<T>::deserialize(deserializer)
}

pub(super) fn validate_session_id(value: &str) -> Result<()> {
    SessionId::validate(value).map_err(|_| miette!("session id is empty, too long, or unsafe"))
}

pub(super) fn persist_session_metadata(
    storage_root: &Path,
    session_id: &str,
    workspace: &Path,
    model_alias: &str,
    initial_session_context: &[Turn],
    workspace_roots: &[PathBuf],
) -> Result<()> {
    validate_session_id(session_id)?;
    let sessions = storage_root.join("sessions");
    ensure_real_directory(&sessions, false)?;
    let directory = sessions.join(session_id);
    ensure_real_directory(&directory, false)?;
    let metadata = SessionMetadata {
        version: SESSION_METADATA_VERSION,
        session_id: session_id.to_owned(),
        budget_session_id: SessionId(session_id.to_owned()),
        workspace: workspace.to_path_buf(),
        model_alias: model_alias.to_owned(),
        initial_session_context: initial_session_context.to_vec(),
        workspace_generation: 0,
        workspace_roots: workspace_roots.to_vec(),
        initial_context_workspace_root_count: workspace_roots.len(),
        inherited_journal_through: None,
        fork_parent_session_id: None,
        fork_at_turn: None,
        fork_operation_id: None,
    };
    let bytes = encode_session_metadata(&metadata)?;
    let path = directory.join("metadata.json");
    #[cfg(unix)]
    {
        persist_session_metadata_unix(&directory, &path, &bytes)
    }
    #[cfg(not(unix))]
    {
        persist_session_metadata_portable(&directory, &path, &bytes)
    }
}

#[cfg(not(unix))]
pub(super) fn persist_session_metadata_portable(
    directory: &Path,
    path: &Path,
    bytes: &[u8],
) -> Result<()> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = directory.join(format!(".metadata-{}-{nonce}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    let mut file = options.open(&temporary).into_diagnostic()?;
    let result = (|| -> Result<()> {
        file.write_all(bytes).into_diagnostic()?;
        file.flush().into_diagnostic()?;
        sync_file(&file)?;
        if path.exists() {
            return Err(miette!("session metadata already exists"));
        }
        std::fs::rename(&temporary, path).into_diagnostic()?;
        sync_directory(directory)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

pub(super) fn load_session_metadata(
    storage_root: &Path,
    session_id: &str,
    expected_workspace: &Path,
) -> Result<SessionMetadata> {
    let metadata = load_session_metadata_any(storage_root, session_id)?;
    if metadata.workspace != expected_workspace {
        return Err(miette!(
            "session metadata identity does not match this session and canonical workspace"
        ));
    }
    Ok(metadata)
}

pub(crate) fn load_session_metadata_any(
    storage_root: &Path,
    session_id: &str,
) -> Result<SessionMetadata> {
    load_session_metadata_any_bounded(storage_root, session_id, MAX_SESSION_METADATA_BYTES)
        .map(|(metadata, _)| metadata)
}

pub(crate) fn load_session_metadata_any_bounded(
    storage_root: &Path,
    session_id: &str,
    max_bytes: u64,
) -> Result<(SessionMetadata, u64)> {
    let max_bytes = max_bytes.min(MAX_SESSION_METADATA_BYTES);
    validate_session_id(session_id)?;
    let sessions = storage_root.join("sessions");
    ensure_real_directory(&sessions, false)?;
    let directory = sessions.join(session_id);
    ensure_real_directory(&directory, false)?;
    let path = directory.join("metadata.json");
    #[cfg(unix)]
    let (bytes, byte_count) = load_session_metadata_unix(&directory, &path, max_bytes)?;
    #[cfg(not(unix))]
    let (bytes, byte_count) = load_session_metadata_portable(&path, max_bytes)?;
    let metadata: SessionMetadata = serde_json::from_slice(&bytes).into_diagnostic()?;
    validate_session_id(&metadata.budget_session_id.0)?;
    if metadata.version != SESSION_METADATA_VERSION || metadata.session_id != session_id {
        return Err(miette!(
            "session metadata identity does not match this session and canonical workspace"
        ));
    }
    if metadata.workspace_roots.is_empty()
        || metadata.workspace_roots.len() > MAX_WORKSPACE_ROOTS
        || metadata.workspace_roots.first() != Some(&metadata.workspace)
        || metadata
            .workspace_roots
            .iter()
            .any(|root| !root.is_absolute())
        || metadata.initial_context_workspace_root_count == 0
        || metadata.initial_context_workspace_root_count > metadata.workspace_roots.len()
    {
        return Err(miette!(
            "session metadata has an invalid workspace-root mapping"
        ));
    }
    Ok((metadata, byte_count))
}

/// Reads only the inherited-accounting boundary needed by aggregate clients.
///
/// The private metadata representation remains an implementation detail of the
/// runtime; callers receive the bounded field and the number of bytes charged.
///
/// # Errors
/// Returns an error when metadata is unsafe, malformed, or exceeds the byte cap.
pub fn load_inherited_accounting_boundary_bounded(
    storage_root: &Path,
    session_id: &str,
    max_bytes: u64,
) -> Result<(Option<SequenceId>, u64)> {
    load_session_metadata_any_bounded(storage_root, session_id, max_bytes)
        .map(|(metadata, bytes)| (metadata.inherited_journal_through, bytes))
}

#[cfg(unix)]
pub(super) fn open_session_metadata_directory(directory: &Path) -> Result<std::os::fd::OwnedFd> {
    rustix::fs::open(
        directory,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()
}

#[cfg(unix)]
pub(super) fn persist_session_metadata_unix(
    directory: &Path,
    path: &Path,
    bytes: &[u8],
) -> Result<()> {
    let parent = open_session_metadata_directory(directory)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = format!(".metadata-{}-{nonce}.tmp", std::process::id());
    let descriptor = rustix::fs::openat(
        &parent,
        &temporary,
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    let mut file = std::fs::File::from(descriptor);
    let result = (|| -> Result<()> {
        file.write_all(bytes).into_diagnostic()?;
        file.flush().into_diagnostic()?;
        rustix::fs::fsync(&file)
            .map_err(std::io::Error::from)
            .into_diagnostic()?;
        rustix::fs::renameat_with(
            &parent,
            &temporary,
            &parent,
            "metadata.json",
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(std::io::Error::from)
        .into_diagnostic()?;
        rustix::fs::fsync(&parent)
            .map_err(std::io::Error::from)
            .into_diagnostic()
            .map_err(|error| miette!("could not synchronize {}: {error}", path.display()))
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&parent, &temporary, rustix::fs::AtFlags::empty());
    }
    result
}

#[cfg(unix)]
pub(super) fn load_session_metadata_unix(
    directory: &Path,
    path: &Path,
    max_bytes: u64,
) -> Result<(Vec<u8>, u64)> {
    let parent = open_session_metadata_directory(directory)?;
    let stat = rustix::fs::statat(
        &parent,
        "metadata.json",
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
        return Err(miette!("session metadata is not a regular file"));
    }
    if stat.st_mode & 0o077 != 0 {
        return Err(miette!(
            "session metadata permissions grant group or other access"
        ));
    }
    let byte_count =
        u64::try_from(stat.st_size).map_err(|_| miette!("session metadata size is invalid"))?;
    if byte_count > max_bytes {
        return Err(miette!(
            "session metadata exceeds the {max_bytes}-byte read limit"
        ));
    }
    let descriptor = rustix::fs::openat(
        &parent,
        "metadata.json",
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    let file = std::fs::File::from(descriptor);
    let opened = rustix::fs::fstat(&file)
        .map_err(std::io::Error::from)
        .into_diagnostic()?;
    if opened.st_dev != stat.st_dev
        || opened.st_ino != stat.st_ino
        || opened.st_size != stat.st_size
        || opened.st_nlink != 1
    {
        return Err(miette!("session metadata changed while it was opened"));
    }
    let length = usize::try_from(byte_count)
        .map_err(|_| miette!("session metadata size cannot be represented"))?;
    let mut bytes = vec![0_u8; length];
    let mut offset = 0_usize;
    while offset < bytes.len() {
        use std::os::unix::fs::FileExt as _;
        let position = u64::try_from(offset)
            .map_err(|_| miette!("session metadata offset cannot be represented"))?;
        let read = file
            .read_at(&mut bytes[offset..], position)
            .into_diagnostic()
            .map_err(|error| miette!("could not read {}: {error}", path.display()))?;
        if read == 0 {
            return Err(miette!("session metadata changed while it was read"));
        }
        offset = offset
            .checked_add(read)
            .ok_or_else(|| miette!("session metadata offset overflow"))?;
    }
    let after = rustix::fs::fstat(&file)
        .map_err(std::io::Error::from)
        .into_diagnostic()?;
    let named_after = rustix::fs::statat(
        &parent,
        "metadata.json",
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    for current in [&after, &named_after] {
        if !rustix::fs::FileType::from_raw_mode(current.st_mode).is_file()
            || current.st_nlink != 1
            || current.st_dev != stat.st_dev
            || current.st_ino != stat.st_ino
            || current.st_size != stat.st_size
            || current.st_mtime != stat.st_mtime
            || current.st_mtime_nsec != stat.st_mtime_nsec
            || current.st_ctime != stat.st_ctime
            || current.st_ctime_nsec != stat.st_ctime_nsec
        {
            return Err(miette!("session metadata changed while it was read"));
        }
    }
    Ok((bytes, byte_count))
}

#[cfg(not(unix))]
pub(super) fn load_session_metadata_portable(
    path: &Path,
    max_bytes: u64,
) -> Result<(Vec<u8>, u64)> {
    let before = std::fs::symlink_metadata(path).into_diagnostic()?;
    if before.file_type().is_symlink() || !before.is_file() {
        return Err(miette!("session metadata is not a regular file"));
    }
    if before.len() > max_bytes {
        return Err(miette!(
            "session metadata exceeds the {max_bytes}-byte read limit"
        ));
    }
    let file = std::fs::File::open(path).into_diagnostic()?;
    let opened = file.metadata().into_diagnostic()?;
    if opened.len() != before.len() || opened.modified().ok() != before.modified().ok() {
        return Err(miette!("session metadata changed while it was opened"));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .into_diagnostic()?;
    let byte_count =
        u64::try_from(bytes.len()).map_err(|_| miette!("session metadata size overflow"))?;
    let after = std::fs::symlink_metadata(path).into_diagnostic()?;
    if byte_count > max_bytes
        || after.file_type().is_symlink()
        || !after.is_file()
        || after.len() != before.len()
        || after.modified().ok() != before.modified().ok()
    {
        return Err(miette!("session metadata changed while it was read"));
    }
    Ok((bytes, byte_count))
}

pub(super) fn ensure_real_directory(path: &Path, create: bool) -> Result<()> {
    if create {
        std::fs::create_dir_all(path).into_diagnostic()?;
    }
    let metadata = std::fs::symlink_metadata(path).into_diagnostic()?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(miette!("{} is not a real directory", path.display()));
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn sync_directory(path: &Path) -> Result<()> {
    let directory = std::fs::File::open(path).into_diagnostic()?;
    sync_file(&directory)
}

#[cfg(not(unix))]
pub(super) fn sync_file(file: &std::fs::File) -> Result<()> {
    #[cfg(unix)]
    {
        rustix::fs::fsync(file)
            .map_err(std::io::Error::from)
            .into_diagnostic()
    }
    #[cfg(not(unix))]
    {
        file.sync_all().into_diagnostic()
    }
}

/// Allocates a cryptographically random local session identifier.
///
/// # Errors
/// Returns an error when the operating system random source is unavailable.
pub fn new_session_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| miette!("session id entropy failed: {error}"))?;
    let mut id = String::with_capacity(40);
    id.push_str("session-");
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut id, "{byte:02x}").into_diagnostic()?;
    }
    Ok(id)
}

#[cfg(test)]
mod tests;
