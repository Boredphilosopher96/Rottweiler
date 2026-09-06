//! Host-approved executable bytes, retained independently of mutable installation paths.
use crate::SandboxError;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

mod identity;

const MAX_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;

/// Exact artifact receipt supplied by the trusted build or installation owner.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableArtifactIdentity {
    pub executable: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub bytes: u64,
    pub sha256: String,
}

/// Portable component checksum installed from the verified product bundle.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableDigest {
    pub bytes: u64,
    pub sha256: String,
}

/// A pinned launch path whose authority remains owned until process settlement.
#[derive(Debug)]
pub struct ExecutableLaunch {
    owner: ApprovedExecutable,
    path: PathBuf,
    #[cfg(target_os = "linux")]
    _pin: File,
}
impl ExecutableLaunch {
    /// Exact pinned executable path passed to the native process launcher.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Whether this worker executes the same approved component bytes.
    #[must_use]
    pub fn matches(&self, artifact: &ApprovedExecutable) -> bool {
        self.owner.same_artifact(artifact)
    }
}

/// Immutable bytes approved by the trusted build or installation owner.
#[derive(Clone, Debug)]
pub struct ApprovedExecutable(Arc<OwnedExecutable>);
#[derive(Debug)]
struct OwnedExecutable {
    installation: PathBuf,
    digest: Option<ExecutableDigest>,
    #[cfg(target_os = "linux")]
    executable: File,
    #[cfg(not(target_os = "linux"))]
    _executable: File,
    #[cfg(not(target_os = "linux"))]
    launch_path: PathBuf,
    #[cfg(not(target_os = "linux"))]
    _directory: Option<tempfile::TempDir>,
}

impl ApprovedExecutable {
    /// Captures the currently executing host, without granting arbitrary paths.
    ///
    /// # Errors
    /// Rejects another executable or a changed inode.
    pub(crate) fn from_running(path: &Path) -> Result<Self, SandboxError> {
        let path = path.canonicalize().map_err(invalid)?;
        let executable = File::open(&path).map_err(invalid)?;
        #[cfg(target_os = "linux")]
        let running = File::open("/proc/self/exe").map_err(invalid)?;
        #[cfg(target_os = "linux")]
        let running_identity = identity(&running.metadata().map_err(invalid)?)?;
        #[cfg(target_os = "macos")]
        let running_identity = rw_macos_bootstrap::running_image_identity().map_err(invalid)?;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let running_identity = return Err(SandboxError::UntrustedHelper);
        let metadata = executable.metadata().map_err(invalid)?;
        if identity(&metadata)? != running_identity {
            return Err(SandboxError::UntrustedHelper);
        }
        #[cfg(target_os = "macos")]
        {
            running::capture(&path, &executable, &metadata)
        }
        #[cfg(not(target_os = "macos"))]
        Ok(Self(Arc::new(OwnedExecutable {
            installation: path.clone(),
            digest: None,
            #[cfg(target_os = "linux")]
            executable,
            #[cfg(not(target_os = "linux"))]
            _executable: executable,
            #[cfg(not(target_os = "linux"))]
            launch_path: path,
            #[cfg(not(target_os = "linux"))]
            _directory: None,
        })))
    }

    /// Verifies a host-approved artifact and copies its exact bytes into an
    /// owned executable. Linux seals the copy against every later write.
    ///
    /// # Errors
    /// Rejects malformed receipts, replacement, size or digest mismatch, and
    /// failure to establish an immutable private executable.
    pub fn from_artifact(approved: &ExecutableArtifactIdentity) -> Result<Self, SandboxError> {
        if approved.bytes == 0
            || approved.bytes > MAX_EXECUTABLE_BYTES
            || approved.sha256.len() != 64
            || !approved
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || !approved.executable.is_absolute()
            || approved.executable.canonicalize().map_err(invalid)? != approved.executable
        {
            return Err(SandboxError::UntrustedHelper);
        }
        let source = File::open(&approved.executable).map_err(invalid)?;
        identity(&source.metadata().map_err(invalid)?)?;
        Self::snapshot(approved, &source)
    }

    // Callers establish authority: external receipts have a hard size bound;
    // kernel-proven running images stream their actual descriptor size.
    fn snapshot(
        approved: &ExecutableArtifactIdentity,
        source: &File,
    ) -> Result<Self, SandboxError> {
        #[cfg(target_os = "linux")]
        let mut executable = File::from(
            rustix::fs::memfd_create(
                "rottweiler-approved-executable",
                rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
            )
            .map_err(invalid)?,
        );
        #[cfg(not(target_os = "linux"))]
        let directory = tempfile::tempdir().map_err(invalid)?;
        #[cfg(not(target_os = "linux"))]
        let launch_path = directory.path().join("approved-executable");
        #[cfg(not(target_os = "linux"))]
        let mut executable = File::options()
            .create_new(true)
            .write(true)
            .read(true)
            .open(&launch_path)
            .map_err(invalid)?;
        identity::verify_copy(approved, source, &mut executable)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            executable
                .set_permissions(std::fs::Permissions::from_mode(0o500))
                .map_err(invalid)?;
        }
        #[cfg(target_os = "linux")]
        rustix::fs::fcntl_add_seals(
            &executable,
            rustix::fs::SealFlags::WRITE
                | rustix::fs::SealFlags::GROW
                | rustix::fs::SealFlags::SHRINK
                | rustix::fs::SealFlags::SEAL,
        )
        .map_err(invalid)?;
        Ok(Self(Arc::new(OwnedExecutable {
            installation: approved.executable.clone(),
            digest: Some(ExecutableDigest {
                bytes: approved.bytes,
                sha256: approved.sha256.clone(),
            }),
            #[cfg(target_os = "linux")]
            executable,
            #[cfg(not(target_os = "linux"))]
            _executable: executable,
            #[cfg(not(target_os = "linux"))]
            launch_path,
            #[cfg(not(target_os = "linux"))]
            _directory: Some(directory),
        })))
    }

    /// Opens the fixed installed component and verifies its approved portable digest.
    ///
    /// # Errors
    /// Rejects unsafe paths, invalid identity, changed content, or snapshot failure.
    pub fn from_installed(path: &Path, approved: &ExecutableDigest) -> Result<Self, SandboxError> {
        let metadata = std::fs::symlink_metadata(path).map_err(invalid)?;
        let (device, inode) = identity(&metadata)?;
        Self::from_artifact(&ExecutableArtifactIdentity {
            executable: path.to_path_buf(),
            device,
            inode,
            bytes: approved.bytes,
            sha256: approved.sha256.clone(),
        })
    }

    /// Captures a launch descriptor; the process owner must retain it through settlement.
    ///
    /// # Errors
    /// Returns an error if the approved descriptor cannot be duplicated.
    pub fn launch(&self) -> Result<ExecutableLaunch, SandboxError> {
        #[cfg(target_os = "linux")]
        let (path, pin) = self.pin()?;
        #[cfg(not(target_os = "linux"))]
        let path = self.launch_path().to_path_buf();
        Ok(ExecutableLaunch {
            owner: self.clone(),
            path,
            #[cfg(target_os = "linux")]
            _pin: pin,
        })
    }
    #[cfg(target_os = "linux")]
    pub(crate) fn sealed_file(&self) -> Result<File, SandboxError> {
        self.0.executable.try_clone().map_err(invalid)
    }

    fn same_artifact(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
            || self
                .0
                .digest
                .as_ref()
                .is_some_and(|digest| Some(digest) == other.0.digest.as_ref())
    }

    /// Original bundle location used to resolve other installed components.
    #[must_use]
    pub fn installation_path(&self) -> &Path {
        &self.0.installation
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn pin(&self) -> Result<(PathBuf, File), SandboxError> {
        use std::os::fd::AsRawFd as _;
        let file = self.0.executable.try_clone().map_err(invalid)?;
        rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty()).map_err(invalid)?;
        Ok((
            PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd())),
            file,
        ))
    }
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn launch_path(&self) -> &Path {
        &self.0.launch_path
    }
}
fn identity(metadata: &std::fs::Metadata) -> Result<(u64, u64), SandboxError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            return Err(SandboxError::UntrustedHelper);
        }
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(SandboxError::UntrustedHelper)
    }
}
fn invalid(error: impl std::fmt::Display) -> SandboxError {
    SandboxError::Backend(error.to_string())
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(char::from(DIGITS[usize::from(byte >> 4)]));
        value.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    value
}

#[cfg(target_os = "macos")]
mod running;
