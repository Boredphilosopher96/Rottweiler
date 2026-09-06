//! Trusted bootstrap role over the shared immutable executable-byte owner.
use crate::{ApprovedExecutable, ExecutableArtifactIdentity, SandboxError};
use std::path::Path;

/// Authority to execute the host's internal sandbox bootstrap entrypoint.
#[derive(Clone, Debug)]
pub struct SandboxHelper(ApprovedExecutable);
impl SandboxHelper {
    /// Captures the currently executing host, without granting arbitrary paths.
    ///
    /// # Errors
    /// Rejects another executable or a changed inode.
    pub fn from_running(path: &Path) -> Result<Self, SandboxError> {
        ApprovedExecutable::from_running(path).map(Self)
    }
    /// Captures the exact bootstrap artifact approved by the trusted host.
    ///
    /// # Errors
    /// Rejects invalid identity, changed bytes, or failure to pin the executable.
    pub fn from_artifact(identity: &ExecutableArtifactIdentity) -> Result<Self, SandboxError> {
        ApprovedExecutable::from_artifact(identity).map(Self)
    }
    /// Installed bundle location used to resolve other components.
    #[must_use]
    pub fn installation_path(&self) -> &Path {
        self.0.installation_path()
    }
    #[cfg(target_os = "linux")]
    pub(crate) fn pin(&self) -> Result<(std::path::PathBuf, std::fs::File), SandboxError> {
        self.0.pin()
    }
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn launch_path(&self) -> &Path {
        self.0.launch_path()
    }
}
#[cfg(all(test, unix))]
mod tests;
