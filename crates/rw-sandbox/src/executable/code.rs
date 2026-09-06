//! Private hierarchy containing only the code files covered by approval.
use super::{ExecutableArtifactIdentity, invalid};
use crate::SandboxError;
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

/// Approved code copied before launch and retained through physical settlement.
#[derive(Debug)]
pub struct ApprovedCode {
    _directory: tempfile::TempDir,
    root: PathBuf,
    cwd: PathBuf,
    args: Vec<OsString>,
}
impl ApprovedCode {
    /// Copies at most64 attested files/256MiB into a private read-only hierarchy.
    /// Explicit file arguments must use their captured canonical paths.
    ///
    /// # Errors
    /// Rejects changed file identity/content, unsafe paths or exceeded bounds.
    pub fn capture(
        cwd: &Path,
        args: &[OsString],
        files: &[ExecutableArtifactIdentity],
    ) -> Result<Self, SandboxError> {
        if files.len() > 64
            || args.len() > 256
            || args
                .iter()
                .try_fold(0usize, |total, arg| total.checked_add(arg.len()))
                .is_none_or(|bytes| bytes > 1024 * 1024)
            || files
                .iter()
                .try_fold(0u64, |total, file| total.checked_add(file.bytes))
                .is_none_or(|bytes| bytes > 256 * 1024 * 1024)
        {
            return Err(SandboxError::UntrustedHelper);
        }
        let directory = tempfile::Builder::new()
            .prefix("rw-approved-code-")
            .tempdir()
            .map_err(invalid)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .map_err(invalid)?;
        }
        let root = directory.path().canonicalize().map_err(invalid)?;
        let project = |path: &Path| -> Result<PathBuf, SandboxError> {
            if path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return Err(SandboxError::UntrustedHelper);
            }
            Ok(root.join(path.strip_prefix("/").map_err(invalid)?))
        };
        for identity in files {
            let destination = project(&identity.executable)?;
            fs::create_dir_all(destination.parent().ok_or(SandboxError::UntrustedHelper)?)
                .map_err(invalid)?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .map_err(invalid)?;
            identity.copy_verified(&mut file)?;
            let mut permissions = file.metadata().map_err(invalid)?.permissions();
            permissions.set_readonly(true);
            file.set_permissions(permissions).map_err(invalid)?;
        }
        let cwd = project(cwd)?;
        fs::create_dir_all(&cwd).map_err(invalid)?;
        let args = args
            .iter()
            .map(|argument| {
                if files
                    .iter()
                    .any(|identity| identity.executable == Path::new(argument))
                {
                    project(Path::new(argument)).map(PathBuf::into_os_string)
                } else {
                    Ok(argument.clone())
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            _directory: directory,
            root,
            cwd,
            args,
        })
    }
    /// Private read-only root; never grant writes beneath or above it.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Projected code working directory for code-only execution.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    /// Literal argv with approved file arguments projected to their private copy.
    #[must_use]
    pub fn args(&self) -> &[OsString] {
        &self.args
    }
}
