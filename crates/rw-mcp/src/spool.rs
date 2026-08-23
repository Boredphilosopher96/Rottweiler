use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read as _,
    io::Write as _,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use rw_types::McpServerId;

use crate::{McpError, OverflowReference};

const DIRECTORY: &str = "mcp-overflow-v1";
const MAX_SPOOL_BYTES: usize = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_RECORDS: usize = 512;

#[async_trait]
pub trait OverflowSpool: Send + Sync {
    async fn write(
        &self,
        server: &McpServerId,
        operation: &str,
        bytes: &[u8],
    ) -> Result<OverflowReference, McpError>;
    async fn read(&self, reference: &OverflowReference) -> Result<Vec<u8>, McpError>;
    async fn remove(&self, reference: &OverflowReference) -> Result<(), McpError>;
}

/// Pinned private overflow directory. Model-visible references never expose its path.
pub struct FilesystemSpool {
    #[cfg(unix)]
    parent: std::os::fd::OwnedFd,
    #[cfg(unix)]
    root: std::os::fd::OwnedFd,
    #[cfg(unix)]
    identity: (rustix::fs::Dev, u64),
    #[cfg(not(unix))]
    root: PathBuf,
    sequence: AtomicU64,
    records: std::sync::Mutex<BTreeMap<String, usize>>,
    pending: std::sync::Mutex<BTreeSet<String>>,
}

impl FilesystemSpool {
    #[allow(clippy::unused_async, clippy::needless_pass_by_value)]
    pub async fn new(parent: PathBuf) -> Result<Self, McpError> {
        Self::open(parent)
    }

    #[cfg(unix)]
    #[allow(clippy::needless_pass_by_value)]
    fn open(parent: PathBuf) -> Result<Self, McpError> {
        use rustix::fs::{Mode, OFlags};
        let parent_fd = rustix::fs::open(
            &parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(spool_error)?;
        let created = match rustix::fs::mkdirat(&parent_fd, DIRECTORY, Mode::from_raw_mode(0o700)) {
            Ok(()) => true,
            Err(rustix::io::Errno::EXIST) => false,
            Err(error) => return Err(spool_error(error)),
        };
        let root = rustix::fs::openat(
            &parent_fd,
            DIRECTORY,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(spool_error)?;
        let stat = rustix::fs::fstat(&root).map_err(spool_error)?;
        let mode = Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o777;
        if stat.st_uid != rustix::process::geteuid().as_raw() || mode != 0o700 {
            return Err(McpError::Spool(
                "overflow directory must be current-user owned with mode 0700".to_owned(),
            ));
        }
        if created {
            rustix::fs::fsync(&parent_fd).map_err(spool_error)?;
        } else {
            purge_stale_payloads(&root)?;
        }
        Ok(Self {
            parent: parent_fd,
            root,
            identity: (stat.st_dev, stat.st_ino),
            sequence: AtomicU64::new(0),
            records: std::sync::Mutex::new(BTreeMap::new()),
            pending: std::sync::Mutex::new(BTreeSet::new()),
        })
    }

    #[cfg(not(unix))]
    #[allow(clippy::needless_pass_by_value)]
    fn open(parent: PathBuf) -> Result<Self, McpError> {
        let root = parent.join(DIRECTORY);
        std::fs::create_dir(&root)
            .or_else(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|error| McpError::Spool(error.to_string()))?;
        Ok(Self {
            root,
            sequence: AtomicU64::new(0),
            records: std::sync::Mutex::new(BTreeMap::new()),
            pending: std::sync::Mutex::new(BTreeSet::new()),
        })
    }

    #[cfg(unix)]
    fn validate_namespace(&self) -> Result<(), McpError> {
        use rustix::fs::{Mode, OFlags};
        let retained = rustix::fs::fstat(&self.root).map_err(spool_error)?;
        let current = rustix::fs::openat(
            &self.parent,
            DIRECTORY,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(spool_error)?;
        let current = rustix::fs::fstat(&current).map_err(spool_error)?;
        let retained_identity = (retained.st_dev, retained.st_ino);
        let current_identity = (current.st_dev, current.st_ino);
        if retained_identity != self.identity || current_identity != self.identity {
            return Err(McpError::Spool(
                "overflow namespace was replaced".to_owned(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
#[allow(clippy::format_collect)]
impl OverflowSpool for FilesystemSpool {
    async fn write(
        &self,
        server: &McpServerId,
        _operation: &str,
        bytes: &[u8],
    ) -> Result<OverflowReference, McpError> {
        if bytes.len() > MAX_SPOOL_BYTES {
            return Err(McpError::Spool(
                "MCP result exceeds the overflow hard cap".to_owned(),
            ));
        }
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random)
            .map_err(|_| McpError::Spool("spool entropy failed".to_owned()))?;
        let random = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let id = format!("mcp-{server}-{sequence}-{random}");
        {
            let mut records = self
                .records
                .lock()
                .map_err(|_| McpError::Spool("overflow quota lock was poisoned".to_owned()))?;
            let total = records.values().copied().sum::<usize>();
            if records.len() >= MAX_RECORDS || total.saturating_add(bytes.len()) > MAX_TOTAL_BYTES {
                return Err(McpError::Spool(
                    "MCP overflow aggregate quota exceeded".to_owned(),
                ));
            }
            records.insert(id.clone(), bytes.len());
            self.pending
                .lock()
                .map_err(|_| McpError::Spool("overflow pending lock was poisoned".to_owned()))?
                .insert(id.clone());
        }
        #[cfg(unix)]
        let write_result = {
            self.validate_namespace()?;
            let root = rustix::io::dup(&self.root).map_err(spool_error)?;
            let payload = bytes.to_vec();
            let name = format!("{id}.payload");
            tokio::task::spawn_blocking(move || write_payload(&root, &name, &payload))
                .await
                .map_err(|error| McpError::Spool(error.to_string()))?
        };
        #[cfg(not(unix))]
        let write_result = {
            let path = self.root.join(format!("{id}.payload"));
            tokio::fs::write(path, bytes)
                .await
                .map_err(|error| McpError::Spool(error.to_string()))
        };
        if let Err(error) = write_result {
            if let Ok(mut records) = self.records.lock() {
                records.remove(&id);
            }
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&id);
            }
            return Err(error);
        }
        self.pending
            .lock()
            .map_err(|_| McpError::Spool("overflow pending lock was poisoned".to_owned()))?
            .remove(&id);
        Ok(OverflowReference {
            id,
            bytes: bytes.len(),
        })
    }

    async fn read(&self, reference: &OverflowReference) -> Result<Vec<u8>, McpError> {
        if self
            .pending
            .lock()
            .map_err(|_| McpError::Spool("overflow pending lock was poisoned".to_owned()))?
            .contains(&reference.id)
        {
            return Err(McpError::Spool(
                "overflow payload write is still pending".to_owned(),
            ));
        }
        let expected = self
            .records
            .lock()
            .map_err(|_| McpError::Spool("overflow quota lock was poisoned".to_owned()))?
            .get(&reference.id)
            .copied()
            .ok_or_else(|| McpError::Spool("unknown overflow reference".to_owned()))?;
        if expected != reference.bytes {
            return Err(McpError::Spool(
                "overflow reference size mismatch".to_owned(),
            ));
        }
        #[cfg(unix)]
        {
            self.validate_namespace()?;
            let root = rustix::io::dup(&self.root).map_err(spool_error)?;
            let name = format!("{}.payload", reference.id);
            tokio::task::spawn_blocking(move || read_payload(&root, &name, expected))
                .await
                .map_err(|error| McpError::Spool(error.to_string()))?
        }
        #[cfg(not(unix))]
        {
            tokio::fs::read(self.root.join(format!("{}.payload", reference.id)))
                .await
                .map_err(|error| McpError::Spool(error.to_string()))
        }
    }

    async fn remove(&self, reference: &OverflowReference) -> Result<(), McpError> {
        if self
            .pending
            .lock()
            .map_err(|_| McpError::Spool("overflow pending lock was poisoned".to_owned()))?
            .contains(&reference.id)
        {
            return Err(McpError::Spool(
                "overflow payload write is still pending".to_owned(),
            ));
        }
        let known = self
            .records
            .lock()
            .map_err(|_| McpError::Spool("overflow quota lock was poisoned".to_owned()))?
            .contains_key(&reference.id);
        if !known {
            return Ok(());
        }
        #[cfg(unix)]
        {
            self.validate_namespace()?;
            let root = rustix::io::dup(&self.root).map_err(spool_error)?;
            let name = format!("{}.payload", reference.id);
            tokio::task::spawn_blocking(move || remove_payload(&root, &name))
                .await
                .map_err(|error| McpError::Spool(error.to_string()))??;
        }
        #[cfg(not(unix))]
        {
            let _ =
                tokio::fs::remove_file(self.root.join(format!("{}.payload", reference.id))).await;
        }
        self.records
            .lock()
            .map_err(|_| McpError::Spool("overflow quota lock was poisoned".to_owned()))?
            .remove(&reference.id);
        Ok(())
    }
}

#[cfg(unix)]
fn write_payload(root: &std::os::fd::OwnedFd, name: &str, bytes: &[u8]) -> Result<(), McpError> {
    use rustix::fs::{Mode, OFlags};
    let descriptor = rustix::fs::openat(
        root,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(spool_error)?;
    let mut file = std::fs::File::from(descriptor);
    file.write_all(bytes).map_err(spool_error)?;
    file.sync_all().map_err(spool_error)?;
    rustix::fs::fsync(root).map_err(spool_error)
}

#[cfg(unix)]
fn read_payload(
    root: &std::os::fd::OwnedFd,
    name: &str,
    expected: usize,
) -> Result<Vec<u8>, McpError> {
    use rustix::fs::{FileType, Mode, OFlags};
    let descriptor = rustix::fs::openat(
        root,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(spool_error)?;
    let stat = rustix::fs::fstat(&descriptor).map_err(spool_error)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_nlink != 1
        || Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o777 != 0o600
        || usize::try_from(stat.st_size).ok() != Some(expected)
    {
        return Err(McpError::Spool(
            "overflow payload identity changed".to_owned(),
        ));
    }
    let mut bytes = Vec::with_capacity(expected);
    std::fs::File::from(descriptor)
        .take((expected as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(spool_error)?;
    if bytes.len() != expected {
        return Err(McpError::Spool("overflow payload size changed".to_owned()));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn remove_payload(root: &std::os::fd::OwnedFd, name: &str) -> Result<(), McpError> {
    use rustix::fs::AtFlags;
    match rustix::fs::unlinkat(root, name, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => rustix::fs::fsync(root).map_err(spool_error),
        Err(error) => Err(spool_error(error)),
    }
}

#[cfg(unix)]
fn purge_stale_payloads(root: &std::os::fd::OwnedFd) -> Result<(), McpError> {
    use rustix::fs::{AtFlags, FileType, Mode};
    let mut directory = rustix::fs::Dir::read_from(root).map_err(spool_error)?;
    let mut names = Vec::new();
    while let Some(entry) = directory.read() {
        let entry = entry.map_err(spool_error)?;
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| McpError::Spool("overflow filename is not UTF-8".to_owned()))?;
        if matches!(name, "." | "..") {
            continue;
        }
        if !name.ends_with(".payload") || names.len() >= MAX_RECORDS {
            return Err(McpError::Spool(
                "unexpected or excessive stale overflow payloads".to_owned(),
            ));
        }
        let stat =
            rustix::fs::statat(root, name, AtFlags::SYMLINK_NOFOLLOW).map_err(spool_error)?;
        if !FileType::from_raw_mode(stat.st_mode).is_file()
            || stat.st_uid != rustix::process::geteuid().as_raw()
            || stat.st_nlink != 1
            || Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o777 != 0o600
        {
            return Err(McpError::Spool("unsafe stale overflow payload".to_owned()));
        }
        names.push(name.to_owned());
    }
    for name in names {
        rustix::fs::unlinkat(root, name, AtFlags::empty()).map_err(spool_error)?;
    }
    rustix::fs::fsync(root).map_err(spool_error)
}

fn spool_error(error: impl std::fmt::Display) -> McpError {
    McpError::Spool(error.to_string())
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    #[tokio::test]
    async fn creates_private_child_and_writes_opaque_payload() {
        let parent = tempfile::tempdir().expect("temp");
        let spool = FilesystemSpool::new(parent.path().to_path_buf())
            .await
            .expect("spool");
        assert_eq!(
            std::fs::metadata(parent.path().join(DIRECTORY))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let reference = spool
            .write(
                &McpServerId::new("safe").expect("id"),
                "ignored",
                b"payload",
            )
            .await
            .expect("write");
        assert!(!reference.id.contains('/'));
        assert_eq!(reference.bytes, 7);
        assert_eq!(spool.read(&reference).await.expect("read"), b"payload");
        spool.remove(&reference).await.expect("remove");
        assert!(spool.read(&reference).await.is_err());
    }

    #[tokio::test]
    async fn rejects_existing_insecure_or_symlink_directory() {
        let parent = tempfile::tempdir().expect("temp");
        let path = parent.path().join(DIRECTORY);
        std::fs::create_dir(&path).expect("mkdir");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
        assert!(
            FilesystemSpool::new(parent.path().to_path_buf())
                .await
                .is_err()
        );
        std::fs::remove_dir(&path).expect("remove");
        let target = tempfile::tempdir().expect("target");
        symlink(target.path(), &path).expect("symlink");
        assert!(
            FilesystemSpool::new(parent.path().to_path_buf())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn detects_namespace_replacement_before_write() {
        let parent = tempfile::tempdir().expect("temp");
        let spool = FilesystemSpool::new(parent.path().to_path_buf())
            .await
            .expect("spool");
        let path = parent.path().join(DIRECTORY);
        std::fs::rename(&path, parent.path().join("old")).expect("rename");
        std::fs::create_dir(&path).expect("replacement");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("mode");
        assert!(
            spool
                .write(
                    &McpServerId::new("safe").expect("id"),
                    "ignored",
                    b"payload"
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reopening_purges_session_ephemeral_stale_payloads() {
        let parent = tempfile::tempdir().expect("temp");
        let reference = {
            let spool = FilesystemSpool::new(parent.path().to_path_buf())
                .await
                .expect("spool");
            spool
                .write(
                    &McpServerId::new("safe").expect("id"),
                    "ignored",
                    b"payload",
                )
                .await
                .expect("write")
        };
        let reopened = FilesystemSpool::new(parent.path().to_path_buf())
            .await
            .expect("reopen");
        assert!(reopened.read(&reference).await.is_err());
        let payloads = std::fs::read_dir(parent.path().join(DIRECTORY))
            .expect("read dir")
            .count();
        assert_eq!(payloads, 0);
    }
}
