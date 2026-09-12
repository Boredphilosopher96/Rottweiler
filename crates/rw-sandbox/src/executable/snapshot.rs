//! Descriptor-backed COW snapshots, verified before executable publication.
use super::{ExecutableArtifactIdentity, SandboxError, identity, invalid};
use rustix::fs::{CloneFlags, Mode, OFlags};
use std::{
    fs::{File, Metadata, Permissions},
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::PathBuf,
};

const NAME: &str = "approved-executable";

pub(super) fn create(
    approved: &ExecutableArtifactIdentity,
    source: &File,
) -> Result<(tempfile::TempDir, PathBuf, File), SandboxError> {
    create_with(approved, source, |source, directory| {
        rustix::fs::fclonefileat(
            source,
            directory,
            NAME,
            CloneFlags::NOFOLLOW | CloneFlags::NOOWNERCOPY,
        )
    })
}

fn create_with(
    approved: &ExecutableArtifactIdentity,
    source: &File,
    clone: impl FnOnce(&File, &File) -> rustix::io::Result<()>,
) -> Result<(tempfile::TempDir, PathBuf, File), SandboxError> {
    let before = source.metadata().map_err(invalid)?;
    verify_source(approved, &before)?;
    let directory = tempfile::Builder::new()
        .prefix("rw-image-")
        .permissions(Permissions::from_mode(0o700))
        .tempdir()
        .map_err(invalid)?;
    let parent = File::from(
        rustix::fs::open(
            directory.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(invalid)?,
    );
    let executable = match clone(source, &parent) {
        Ok(()) => {
            let file = File::from(
                rustix::fs::openat(
                    &parent,
                    NAME,
                    OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
                .map_err(invalid)?,
            );
            if !file.metadata().map_err(invalid)?.is_file()
                || identity::copy_digest(&file, approved.bytes, &mut std::io::sink())?
                    != approved.sha256
            {
                return Err(SandboxError::UntrustedHelper);
            }
            file
        }
        Err(rustix::io::Errno::XDEV | rustix::io::Errno::NOTSUP | rustix::io::Errno::NOSYS) => {
            // Unsupported filesystem operations preserve the same byte contract.
            // CREATE|EXCL also rejects any partial destination from a failed clone.
            let mut file = File::from(
                rustix::fs::openat(
                    &parent,
                    NAME,
                    OFlags::RDWR
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::CLOEXEC
                        | OFlags::NOFOLLOW,
                    Mode::RUSR | Mode::WUSR,
                )
                .map_err(invalid)?,
            );
            identity::verify_copy(approved, source, &mut file)?;
            file
        }
        Err(error) => return Err(invalid(error)),
    };
    let after = source.metadata().map_err(invalid)?;
    verify_source(approved, &after)?;
    if source_state(&before) != source_state(&after) {
        return Err(SandboxError::UntrustedHelper);
    }
    let path = directory.path().join(NAME);
    Ok((directory, path, executable))
}

fn verify_source(
    approved: &ExecutableArtifactIdentity,
    metadata: &Metadata,
) -> Result<(), SandboxError> {
    if super::identity(metadata)? != (approved.device, approved.inode)
        || metadata.len() != approved.bytes
    {
        return Err(SandboxError::UntrustedHelper);
    }
    Ok(())
}

fn source_state(metadata: &Metadata) -> (u64, u64, u64, u32, i64, i64, i64, i64) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mode(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

#[cfg(test)]
mod tests;
