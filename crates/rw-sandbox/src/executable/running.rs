//! One verified snapshot per running image; requested descriptors are checked on every call.
use super::{
    ApprovedExecutable, ExecutableArtifactIdentity, MAX_EXECUTABLE_BYTES, hex_digest, invalid,
};
use crate::SandboxError;
use sha2::{Digest as _, Sha256};
use std::{
    fs::{File, Metadata},
    io::Read as _,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Eq, PartialEq)]
struct ImageIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
    bytes: u64,
    mode: u32,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl ImageIdentity {
    fn new(path: &Path, metadata: &Metadata) -> Self {
        Self {
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
            bytes: metadata.len(),
            mode: metadata.mode(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}
struct RunningImage {
    identity: ImageIdentity,
    executable: ApprovedExecutable,
}
static IMAGE: Mutex<Option<RunningImage>> = Mutex::new(None);

pub(super) fn capture(
    path: &Path,
    source: &File,
    metadata: &Metadata,
) -> Result<ApprovedExecutable, SandboxError> {
    let identity = ImageIdentity::new(path, metadata);
    let mut cache = IMAGE.lock().map_err(|_| SandboxError::UntrustedHelper)?;
    if let Some(image) = cache.as_ref() {
        if image.identity != identity
            || ImageIdentity::new(path, &source.metadata().map_err(invalid)?) != identity
        {
            return Err(SandboxError::UntrustedHelper);
        }
        return Ok(image.executable.clone());
    }
    if identity.bytes == 0 || identity.bytes > MAX_EXECUTABLE_BYTES {
        return Err(SandboxError::UntrustedHelper);
    }
    let mut hasher = Sha256::new();
    let mut input = source.take(identity.bytes + 1);
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = input.read(&mut buffer).map_err(invalid)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let executable = ApprovedExecutable::from_artifact(&ExecutableArtifactIdentity {
        executable: path.to_path_buf(),
        device: identity.device,
        inode: identity.inode,
        bytes: identity.bytes,
        sha256: hex_digest(&hasher.finalize()),
    })?;
    if ImageIdentity::new(path, &source.metadata().map_err(invalid)?) != identity {
        return Err(SandboxError::UntrustedHelper);
    }
    *cache = Some(RunningImage {
        identity,
        executable: executable.clone(),
    });
    Ok(executable)
}
