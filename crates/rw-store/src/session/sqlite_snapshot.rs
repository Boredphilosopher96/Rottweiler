//! Descriptor-checked bounded snapshots for read-only `SQLite` queries.
use super::SessionStoreError;
#[cfg(not(unix))]
use std::io::Read as _;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::FileExt as _;
use std::{
    fs::{self, File},
    path::Path,
};
use tempfile::TempDir;
pub(super) const MAX_SEARCH_INDEX_BYTES: u64 = 256 * 1024 * 1024;
pub(super) const MAX_SEARCH_INDEX_WAL_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn validate_read_only_index(path: &Path) -> Result<fs::Metadata, SessionStoreError> {
    let link = fs::symlink_metadata(path)?;
    if link.file_type().is_symlink() || !link.is_file() {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if link.nlink() != 1 {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
    }
    Ok(link)
}

/// Copies the `SQLite` database and any committed WAL into a private snapshot.
///
/// `SQLite` WAL readers may create an empty `-wal` file or update read marks in
/// `-shm`, even when the database handle itself is opened read-only. Querying a
/// private snapshot keeps the live derived index byte-for-byte unchanged while
/// still including committed frames which have not yet been checkpointed.
#[cfg(unix)]
pub(super) fn read_only_index_snapshot(
    root: &Path,
    expected: &fs::Metadata,
) -> Result<TempDir, SessionStoreError> {
    use rustix::{
        fs::{FileType, Mode, OFlags},
        io::Errno,
    };

    let directory = File::from(
        rustix::fs::open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let main = open_snapshot_source(&directory, "index.sqlite")?
        .ok_or(SessionStoreError::UnsafeSessionIndex)?;
    let main_before = main.metadata()?;
    if !same_file_identity(expected, &main_before) {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    validate_snapshot_source_size(&main_before, "index.sqlite", MAX_SEARCH_INDEX_BYTES)?;

    let wal = open_snapshot_source(&directory, "index.sqlite-wal")?;
    let wal_before = wal.as_ref().map(File::metadata).transpose()?;
    if let Some(metadata) = wal_before.as_ref() {
        validate_snapshot_source_size(metadata, "index.sqlite-wal", MAX_SEARCH_INDEX_WAL_BYTES)?;
    }
    let snapshot = tempfile::tempdir()?;
    copy_snapshot_source(
        &main,
        &snapshot.path().join("index.sqlite"),
        "index.sqlite",
        MAX_SEARCH_INDEX_BYTES,
    )?;
    if let Some(wal) = wal.as_ref() {
        copy_snapshot_source(
            wal,
            &snapshot.path().join("index.sqlite-wal"),
            "index.sqlite-wal",
            MAX_SEARCH_INDEX_WAL_BYTES,
        )?;
    }

    let main_after = main.metadata()?;
    if !same_snapshot_version(&main_before, &main_after)
        || !snapshot_name_still_refers_to(&directory, "index.sqlite", &main_before)?
    {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    match (wal_before.as_ref(), wal.as_ref()) {
        (Some(before), Some(wal)) => {
            if !same_snapshot_version(before, &wal.metadata()?)
                || !snapshot_name_still_refers_to(&directory, "index.sqlite-wal", before)?
            {
                return Err(SessionStoreError::UnsafeSessionIndex);
            }
        }
        (None, None) => match rustix::fs::openat(
            &directory,
            "index.sqlite-wal",
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Err(Errno::NOENT) => {}
            Ok(_) | Err(_) => return Err(SessionStoreError::UnsafeSessionIndex),
        },
        _ => return Err(SessionStoreError::UnsafeSessionIndex),
    }

    let stat = rustix::fs::fstat(&main).map_err(std::io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    Ok(snapshot)
}

#[cfg(unix)]
fn open_snapshot_source(parent: &File, name: &str) -> Result<Option<File>, SessionStoreError> {
    use rustix::{
        fs::{FileType, Mode, OFlags},
        io::Errno,
    };

    match rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(descriptor) => {
            let stat = rustix::fs::fstat(&descriptor).map_err(std::io::Error::from)?;
            if !FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
                return Err(SessionStoreError::UnsafeSessionIndex);
            }
            Ok(Some(File::from(descriptor)))
        }
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(std::io::Error::from(error).into()),
    }
}

#[cfg(unix)]
fn copy_snapshot_source(
    source: &File,
    destination: &Path,
    component: &'static str,
    max_bytes: u64,
) -> Result<(), SessionStoreError> {
    let metadata = source.metadata()?;
    validate_snapshot_source_size(&metadata, component, max_bytes)?;
    let length = usize::try_from(metadata.len()).map_err(|_| SessionStoreError::LimitOverflow)?;
    let mut output = File::create(destination)?;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut offset = 0_usize;
    while offset < length {
        let remaining = buffer.len().min(length.saturating_sub(offset));
        let count = source.read_at(
            &mut buffer[..remaining],
            u64::try_from(offset).map_err(|_| SessionStoreError::LimitOverflow)?,
        )?;
        if count == 0 {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        output.write_all(&buffer[..count])?;
        offset = offset
            .checked_add(count)
            .ok_or(SessionStoreError::LimitOverflow)?;
    }
    output.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn snapshot_name_still_refers_to(
    parent: &File,
    name: &str,
    expected: &fs::Metadata,
) -> Result<bool, SessionStoreError> {
    let Some(current) = open_snapshot_source(parent, name)? else {
        return Ok(false);
    };
    Ok(same_file_identity(expected, &current.metadata()?))
}

fn same_snapshot_version(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_file_identity(left, right) && left.modified().ok() == right.modified().ok()
}

fn validate_snapshot_source_size(
    metadata: &fs::Metadata,
    component: &'static str,
    max_bytes: u64,
) -> Result<(), SessionStoreError> {
    if metadata.len() > max_bytes {
        return Err(SessionStoreError::SessionIndexSnapshotTooLarge {
            component,
            max_bytes,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn read_only_index_snapshot(
    root: &Path,
    expected: &fs::Metadata,
) -> Result<TempDir, SessionStoreError> {
    let main_path = root.join("index.sqlite");
    let main_link = fs::symlink_metadata(&main_path)?;
    if main_link.file_type().is_symlink() || !same_file_identity(expected, &main_link) {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    validate_snapshot_source_size(&main_link, "index.sqlite", MAX_SEARCH_INDEX_BYTES)?;
    let wal_path = root.join("index.sqlite-wal");
    let wal_link = match fs::symlink_metadata(&wal_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(SessionStoreError::UnsafeSessionIndex);
            }
            validate_snapshot_source_size(
                &metadata,
                "index.sqlite-wal",
                MAX_SEARCH_INDEX_WAL_BYTES,
            )?;
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let snapshot = tempfile::tempdir()?;
    copy_snapshot_path(
        &main_path,
        &snapshot.path().join("index.sqlite"),
        &main_link,
        "index.sqlite",
        MAX_SEARCH_INDEX_BYTES,
    )?;
    if let Some(wal_link) = wal_link.as_ref() {
        copy_snapshot_path(
            &wal_path,
            &snapshot.path().join("index.sqlite-wal"),
            wal_link,
            "index.sqlite-wal",
            MAX_SEARCH_INDEX_WAL_BYTES,
        )?;
        if !same_snapshot_version(&wal_link, &fs::symlink_metadata(&wal_path)?) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
    }
    if !same_snapshot_version(&main_link, &fs::symlink_metadata(&main_path)?) {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    Ok(snapshot)
}

#[cfg(not(unix))]
fn copy_snapshot_path(
    source_path: &Path,
    destination: &Path,
    expected: &fs::Metadata,
    component: &'static str,
    max_bytes: u64,
) -> Result<(), SessionStoreError> {
    let source = File::open(source_path)?;
    let metadata = source.metadata()?;
    if !same_file_identity(expected, &metadata) {
        return Err(SessionStoreError::UnsafeSessionIndex);
    }
    validate_snapshot_source_size(&metadata, component, max_bytes)?;
    let mut bounded = source.take(max_bytes.saturating_add(1));
    let mut output = File::create(destination)?;
    let copied = std::io::copy(&mut bounded, &mut output)?;
    if copied > max_bytes {
        return Err(SessionStoreError::SessionIndexSnapshotTooLarge {
            component,
            max_bytes,
        });
    }
    output.sync_all()?;
    Ok(())
}

#[cfg(unix)]
pub(super) fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino() && left.len() == right.len()
}

#[cfg(not(unix))]
pub(super) fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}
