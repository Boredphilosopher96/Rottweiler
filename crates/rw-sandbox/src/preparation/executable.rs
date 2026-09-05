//! Approved compiler bytes become an immutable executable before entering the view.

use crate::SandboxError;
use rustix::fs::{MemfdFlags, Mode, SealFlags};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read as _,
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _},
    path::{Path, PathBuf},
};

const MAX_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;

/// Identity supplied by the process approval owner, before sandbox launch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparationExecutable {
    path: PathBuf,
    device: u64,
    inode: u64,
    length: u64,
    content_blake3: String,
}
impl PreparationExecutable {
    /// Creates a compiler identity from the host's approved process configuration.
    ///
    /// # Errors
    /// Rejects invalid paths, excessive file sizes, or malformed digests.
    pub fn from_identity(
        path: PathBuf,
        device: u64,
        inode: u64,
        length: u64,
        content_blake3: String,
    ) -> Result<Self, SandboxError> {
        let identity = Self {
            path,
            device,
            inode,
            length,
            content_blake3,
        };
        identity.validate()?;
        Ok(identity)
    }
    /// Captures an executable for a trusted caller without a separate approval store.
    ///
    /// # Errors
    /// Rejects non-executable files, excessive sizes, or failed reads.
    pub fn capture(path: &Path) -> Result<Self, SandboxError> {
        let path = path.canonicalize().map_err(super::invalid)?;
        let file = open(&path)?;
        let metadata = file.metadata().map_err(super::invalid)?;
        if !metadata.is_file()
            || metadata.len() > MAX_EXECUTABLE_BYTES
            || metadata.mode() & 0o111 == 0
        {
            return Err(SandboxError::UntrustedHelper);
        }
        let content_blake3 = digest(&file, metadata.len(), &mut std::io::sink())?;
        Self::from_identity(
            path,
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            content_blake3,
        )
    }
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn validate(&self) -> Result<(), SandboxError> {
        if !self.path.is_absolute()
            || self.path.as_os_str().len() > 4096
            || self
                .path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
            || self.length > MAX_EXECUTABLE_BYTES
            || blake3::Hash::from_hex(&self.content_blake3).is_err()
        {
            return Err(SandboxError::MalformedHelper);
        }
        Ok(())
    }
    pub(crate) fn snapshot_approved(&self) -> Result<File, SandboxError> {
        self.validate()?;
        let file = open(&self.path)?;
        let metadata = file.metadata().map_err(super::invalid)?;
        if !metadata.is_file()
            || metadata.mode() & 0o111 == 0
            || (metadata.dev(), metadata.ino(), metadata.len())
                != (self.device, self.inode, self.length)
        {
            return Err(SandboxError::UntrustedHelper);
        }
        let mut snapshot = File::from(
            rustix::fs::memfd_create(
                "rottweiler-source-host",
                MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
            )
            .map_err(super::invalid)?,
        );
        if digest(&file, self.length, &mut snapshot)? != self.content_blake3 {
            return Err(SandboxError::UntrustedHelper);
        }
        rustix::fs::fchmod(&snapshot, Mode::RUSR | Mode::XUSR).map_err(super::invalid)?;
        rustix::fs::fcntl_add_seals(
            &snapshot,
            SealFlags::WRITE | SealFlags::SHRINK | SealFlags::GROW | SealFlags::SEAL,
        )
        .map_err(super::invalid)?;
        Ok(snapshot)
    }
}
fn open(path: &Path) -> Result<File, SandboxError> {
    File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(super::invalid)
}
fn digest(
    file: &File,
    length: u64,
    output: &mut impl std::io::Write,
) -> Result<String, SandboxError> {
    let mut bounded = file.take(length + 1);
    let mut hasher = blake3::Hasher::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = bounded.read(&mut buffer).map_err(super::invalid)?;
        if read == 0 {
            break;
        }
        bytes += u64::try_from(read).map_err(super::invalid)?;
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read]).map_err(super::invalid)?;
    }
    if bytes != length {
        return Err(SandboxError::UntrustedHelper);
    }
    Ok(hasher.finalize().to_hex().to_string())
}
