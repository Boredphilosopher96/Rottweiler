//! Bounded synchronous file IO, called only by an owned file transaction.
use super::transaction::FileTransaction;
use crate::{ToolContext, ToolError};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicU64, Ordering};
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(super) fn read_capped(
    context: &ToolContext,
    path: &std::path::Path,
    limit: usize,
) -> Result<Vec<u8>, ToolError> {
    Ok(read_capped_snapshot(context, path, limit)?.bytes)
}

#[derive(Clone, Debug)]
pub(super) struct FileSnapshot {
    pub(super) bytes: Vec<u8>,
    content_hash: [u8; 32],
    #[cfg(unix)]
    identity: FileIdentity,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt as _;
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

pub(super) fn read_capped_snapshot(
    context: &ToolContext,
    path: &std::path::Path,
    limit: usize,
) -> Result<FileSnapshot, ToolError> {
    #[cfg(unix)]
    let (file, identity) = {
        let (parent, file_name) = context.secure_parent(path)?;
        let descriptor = rustix::fs::openat(
            parent,
            file_name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(|source| ToolError::Io {
            operation: "open file without following links",
            path: path.to_path_buf(),
            source: source.into(),
        })?;
        let file = std::fs::File::from(descriptor);
        let metadata = file.metadata().map_err(|source| ToolError::Io {
            operation: "inspect opened file",
            path: path.to_path_buf(),
            source,
        })?;
        if !metadata.file_type().is_file() {
            return Err(ToolError::InvalidInput(format!(
                "{} is not a regular file",
                context.relative_display(path).display()
            )));
        }
        (file, file_identity(&metadata))
    };
    #[cfg(not(unix))]
    let file = std::fs::File::open(path).map_err(|source| ToolError::Io {
        operation: "open file",
        path: path.to_path_buf(),
        source,
    })?;
    let bytes = read_chunks(file, path, limit, &context.cancellation)?;
    Ok(FileSnapshot {
        content_hash: *blake3::hash(&bytes).as_bytes(),
        bytes,
        #[cfg(unix)]
        identity,
    })
}

pub(super) fn atomic_write(
    transaction: &mut FileTransaction,
    context: &ToolContext,
    path: &std::path::Path,
    payload: &[u8],
    cancellation: &crate::CancellationToken,
) -> Result<(), ToolError> {
    atomic_write_with_snapshot(transaction, context, path, payload, None, cancellation)
}

pub(super) fn atomic_write_if_unchanged(
    transaction: &mut FileTransaction,
    context: &ToolContext,
    path: &std::path::Path,
    payload: &[u8],
    snapshot: &FileSnapshot,
    cancellation: &crate::CancellationToken,
) -> Result<(), ToolError> {
    atomic_write_with_snapshot(
        transaction,
        context,
        path,
        payload,
        Some(snapshot),
        cancellation,
    )
}

fn atomic_write_with_snapshot(
    transaction: &mut FileTransaction,
    context: &ToolContext,
    path: &std::path::Path,
    payload: &[u8],
    snapshot: Option<&FileSnapshot>,
    cancellation: &crate::CancellationToken,
) -> Result<(), ToolError> {
    #[cfg(unix)]
    {
        atomic_write_unix(transaction, context, path, payload, snapshot, cancellation)
    }
    #[cfg(not(unix))]
    {
        atomic_write_portable(transaction, path, payload, snapshot, cancellation)
    }
}

#[cfg(unix)]
fn atomic_write_unix(
    transaction: &mut FileTransaction,
    context: &ToolContext,
    path: &std::path::Path,
    payload: &[u8],
    snapshot: Option<&FileSnapshot>,
    cancellation: &crate::CancellationToken,
) -> Result<(), ToolError> {
    cancellation.check()?;
    let (parent, file_name) = context.secure_parent(path)?;
    let existing_permissions = existing_permissions_unix(context, &parent, &file_name, path)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = format!(
        ".{}.rottweiler.{}.{sequence}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    );
    let descriptor = rustix::fs::openat(
        &parent,
        temporary.as_str(),
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::from_bits_truncate(0o666),
    )
    .map_err(|source| ToolError::Io {
        operation: "create temporary file",
        path: path.with_file_name(&temporary),
        source: source.into(),
    })?;
    let parent = transaction.register(
        parent,
        temporary.clone().into(),
        path.with_file_name(&temporary),
    );
    let mut file = std::fs::File::from(descriptor);
    {
        write_chunks(&mut file, payload, cancellation).map_err(|source| ToolError::Io {
            operation: "write temporary file",
            path: path.with_file_name(&temporary),
            source,
        })?;
        file.flush().map_err(|source| ToolError::Io {
            operation: "flush temporary file",
            path: path.with_file_name(&temporary),
            source,
        })?;
        if let Some(permissions) = existing_permissions {
            file.set_permissions(permissions)
                .map_err(|source| ToolError::Io {
                    operation: "preserve file permissions",
                    path: path.with_file_name(&temporary),
                    source,
                })?;
        }
        file.sync_all().map_err(|source| ToolError::Io {
            operation: "synchronize temporary file",
            path: path.with_file_name(&temporary),
            source,
        })?;
        cancellation.check()?;
        drop(file);
        let target = verify_snapshot_unchanged(&parent, &file_name, path, snapshot, cancellation)?;
        rustix::fs::renameat(parent, temporary.as_str(), parent, &file_name).map_err(|source| {
            ToolError::Io {
                operation: "replace file",
                path: path.to_path_buf(),
                source: source.into(),
            }
        })?;
        drop(target);
        rustix::fs::fsync(parent).map_err(|source| ToolError::Io {
            operation: "synchronize parent directory",
            path: path
                .parent()
                .map_or_else(|| path.to_path_buf(), std::path::Path::to_path_buf),
            source: source.into(),
        })
    }?;
    transaction.committed();
    Ok(())
}

#[cfg(unix)]
fn existing_permissions_unix(
    context: &ToolContext,
    parent: &impl std::os::fd::AsFd,
    file_name: &std::ffi::OsStr,
    path: &std::path::Path,
) -> Result<Option<std::fs::Permissions>, ToolError> {
    match rustix::fs::statat(parent, file_name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
                return Err(ToolError::InvalidInput(format!(
                    "{} is not a regular file",
                    context.relative_display(path).display()
                )));
            }
            #[cfg(target_os = "linux")]
            let mode = stat.st_mode;
            #[cfg(not(target_os = "linux"))]
            let mode = u32::from(stat.st_mode);
            Ok(Some(std::fs::Permissions::from_mode(mode & 0o7777)))
        }
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(source) => Err(ToolError::Io {
            operation: "inspect existing file without following links",
            path: path.to_path_buf(),
            source: source.into(),
        }),
    }
}

#[cfg(unix)]
fn verify_snapshot_unchanged(
    parent: &impl std::os::fd::AsFd,
    file_name: &std::ffi::OsStr,
    path: &std::path::Path,
    snapshot: Option<&FileSnapshot>,
    cancellation: &crate::CancellationToken,
) -> Result<Option<std::fs::File>, ToolError> {
    let target = snapshot
        .map(|snapshot| verify_snapshot_unix(parent, file_name, path, snapshot, cancellation))
        .transpose()?;
    cancellation.check()?;
    Ok(target)
}

#[cfg(unix)]
fn verify_snapshot_unix(
    parent: &impl std::os::fd::AsFd,
    file_name: &std::ffi::OsStr,
    path: &std::path::Path,
    snapshot: &FileSnapshot,
    cancellation: &crate::CancellationToken,
) -> Result<std::fs::File, ToolError> {
    let descriptor = rustix::fs::openat(
        parent,
        file_name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| ToolError::FileChangedSinceRead(path.to_path_buf()))?;
    let mut file = std::fs::File::from(descriptor);
    let metadata = file.metadata().map_err(|source| ToolError::Io {
        operation: "inspect file for compare-and-swap",
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file()
        || file_identity(&metadata) != snapshot.identity
        || metadata.len() != snapshot.bytes.len() as u64
    {
        return Err(ToolError::FileChangedSinceRead(path.to_path_buf()));
    }
    let path_stat = rustix::fs::statat(parent, file_name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| ToolError::FileChangedSinceRead(path.to_path_buf()))?;
    let descriptor_stat = rustix::fs::fstat(&file).map_err(|source| ToolError::Io {
        operation: "inspect file descriptor for compare-and-swap",
        path: path.to_path_buf(),
        source: source.into(),
    })?;
    if path_stat.st_dev != descriptor_stat.st_dev || path_stat.st_ino != descriptor_stat.st_ino {
        return Err(ToolError::FileChangedSinceRead(path.to_path_buf()));
    }
    let bytes =
        read_chunks(&mut file, path, snapshot.bytes.len(), cancellation).map_err(|error| {
            match error {
                ToolError::SizeLimit { .. } => ToolError::FileChangedSinceRead(path.to_path_buf()),
                error => error,
            }
        })?;
    if bytes.len() != snapshot.bytes.len()
        || *blake3::hash(&bytes).as_bytes() != snapshot.content_hash
    {
        return Err(ToolError::FileChangedSinceRead(path.to_path_buf()));
    }
    let final_path_stat =
        rustix::fs::statat(parent, file_name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| ToolError::FileChangedSinceRead(path.to_path_buf()))?;
    let final_descriptor_stat = rustix::fs::fstat(&file).map_err(|source| ToolError::Io {
        operation: "reinspect file descriptor for compare-and-swap",
        path: path.to_path_buf(),
        source: source.into(),
    })?;
    let final_length = file
        .metadata()
        .map_err(|source| ToolError::Io {
            operation: "reinspect file length for compare-and-swap",
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if final_path_stat.st_dev != final_descriptor_stat.st_dev
        || final_path_stat.st_ino != final_descriptor_stat.st_ino
        || final_length != snapshot.bytes.len() as u64
    {
        return Err(ToolError::FileChangedSinceRead(path.to_path_buf()));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn atomic_write_portable(
    transaction: &mut FileTransaction,
    path: &std::path::Path,
    content: &[u8],
    snapshot: Option<&FileSnapshot>,
    cancellation: &crate::CancellationToken,
) -> Result<(), ToolError> {
    cancellation.check()?;
    let existing_permissions = std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_file_name(format!(
        ".{file_name}.rottweiler.{}.{sequence}.tmp",
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    let mut file = options.open(&temporary).map_err(|source| ToolError::Io {
        operation: "create temporary file",
        path: temporary.clone(),
        source,
    })?;
    transaction.register(temporary.clone());
    {
        write_chunks(&mut file, content, cancellation).map_err(|source| ToolError::Io {
            operation: "write temporary file",
            path: temporary.clone(),
            source,
        })?;
        file.flush().map_err(|source| ToolError::Io {
            operation: "flush temporary file",
            path: temporary.clone(),
            source,
        })?;
        if let Some(permissions) = existing_permissions {
            file.set_permissions(permissions)
                .map_err(|source| ToolError::Io {
                    operation: "preserve file permissions",
                    path: temporary.clone(),
                    source,
                })?;
        }
        file.sync_all().map_err(|source| ToolError::Io {
            operation: "synchronize temporary file",
            path: temporary.clone(),
            source,
        })?;
        cancellation.check()?;
        drop(file);
        if let Some(snapshot) = snapshot {
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|_| ToolError::FileChangedSinceRead(path.to_path_buf()))?;
            if !metadata.file_type().is_file() || metadata.len() != snapshot.bytes.len() as u64 {
                return Err(ToolError::FileChangedSinceRead(path.to_path_buf()));
            }
            let file = std::fs::File::open(path).map_err(|source| ToolError::Io {
                operation: "reopen file for compare-and-swap",
                path: path.to_path_buf(),
                source,
            })?;
            let current = read_chunks(file, path, snapshot.bytes.len(), cancellation)?;
            if *blake3::hash(&current).as_bytes() != snapshot.content_hash {
                return Err(ToolError::FileChangedSinceRead(path.to_path_buf()));
            }
        }
        cancellation.check()?;
        std::fs::rename(&temporary, path).map_err(|source| ToolError::Io {
            operation: "replace file",
            path: path.to_path_buf(),
            source,
        })
    }?;
    transaction.committed();
    Ok(())
}

fn read_chunks(
    mut file: impl Read,
    path: &std::path::Path,
    limit: usize,
    cancellation: &crate::CancellationToken,
) -> Result<Vec<u8>, ToolError> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut chunk = [0u8; 16 * 1024];
    loop {
        cancellation.check()?;
        let available = chunk
            .len()
            .min(limit.saturating_add(1).saturating_sub(bytes.len()));
        let count = file
            .read(&mut chunk[..available])
            .map_err(|source| ToolError::Io {
                operation: "read file",
                path: path.to_path_buf(),
                source,
            })?;
        if count == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > limit {
            return Err(ToolError::SizeLimit { limit });
        }
    }
}
fn write_chunks(
    file: &mut impl Write,
    payload: &[u8],
    cancellation: &crate::CancellationToken,
) -> std::io::Result<()> {
    for chunk in payload.chunks(64 * 1024) {
        cancellation.check().map_err(std::io::Error::other)?;
        file.write_all(chunk)?;
    }
    Ok(())
}
