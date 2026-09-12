//! Approved compiler bytes become an immutable executable before entering the view.

use crate::SandboxError;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
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
    content_sha256: String,
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
        content_sha256: String,
    ) -> Result<Self, SandboxError> {
        let identity = Self {
            path,
            device,
            inode,
            length,
            content_sha256,
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
        let captured = crate::ExecutableArtifactIdentity::capture(&path, MAX_EXECUTABLE_BYTES)?;
        Self::from_identity(
            path,
            captured.device,
            captured.inode,
            captured.bytes,
            captured.sha256,
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
            || self.length == 0
            || self.length > MAX_EXECUTABLE_BYTES
            || self.content_sha256.len() != 64
            || !self
                .content_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
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
        crate::ApprovedExecutable::from_artifact(&crate::ExecutableArtifactIdentity {
            executable: self.path.clone(),
            device: self.device,
            inode: self.inode,
            bytes: self.length,
            sha256: self.content_sha256.clone(),
        })?
        .sealed_file()
    }
}
fn open(path: &Path) -> Result<File, SandboxError> {
    File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(super::invalid)
}
