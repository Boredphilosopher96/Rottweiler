//! One streamed identity/copy implementation for approval and immutable artifacts.
use super::{ExecutableArtifactIdentity, MAX_EXECUTABLE_BYTES, SandboxError, hex_digest, invalid};
use sha2::{Digest as _, Sha256};
use std::{
    fs::File,
    io::{Read as _, Seek as _, Write},
    path::Path,
};

impl ExecutableArtifactIdentity {
    /// Captures file identity for an approval decision, without approving execution.
    ///
    /// # Errors
    /// Rejects non-regular files, paths or byte counts outside admission, and
    /// files changed during capture. Callers must still obtain approval.
    pub fn capture(path: &Path, max_bytes: u64) -> Result<Self, SandboxError> {
        if !path.is_absolute() || path.canonicalize().map_err(invalid)?.as_path() != path {
            return Err(SandboxError::UntrustedHelper);
        }
        let file = File::open(path).map_err(invalid)?;
        let metadata = file.metadata().map_err(invalid)?;
        if !metadata.is_file() || metadata.len() > max_bytes.min(MAX_EXECUTABLE_BYTES) {
            return Err(SandboxError::UntrustedHelper);
        }
        let (device, inode) = file_identity(&metadata)?;
        let sha256 = copy_digest(&file, metadata.len(), &mut std::io::sink())?;
        Ok(Self {
            executable: path.to_path_buf(),
            device,
            inode,
            bytes: metadata.len(),
            sha256,
        })
    }

    /// Copies the approved regular file into a caller-owned private destination.
    /// The caller retains the destination and controls its publication and lifetime.
    ///
    /// # Errors
    /// Rejects malformed or substituted source identity and digest/size mismatch.
    pub fn copy_verified(&self, destination: &mut File) -> Result<(), SandboxError> {
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || self.bytes > MAX_EXECUTABLE_BYTES
            || !self.executable.is_absolute()
            || self.executable.canonicalize().map_err(invalid)? != self.executable
        {
            return Err(SandboxError::UntrustedHelper);
        }
        let source = File::open(&self.executable).map_err(invalid)?;
        verify_copy(self, &source, destination)
    }
}

pub(super) fn verify_copy(
    approved: &ExecutableArtifactIdentity,
    source: &File,
    destination: &mut impl Write,
) -> Result<(), SandboxError> {
    let before = source.metadata().map_err(invalid)?;
    if !before.is_file()
        || before.len() != approved.bytes
        || file_identity(&before)? != (approved.device, approved.inode)
    {
        return Err(SandboxError::UntrustedHelper);
    }
    if copy_digest(source, approved.bytes, destination)? != approved.sha256 {
        return Err(SandboxError::UntrustedHelper);
    }
    Ok(())
}

fn file_identity(metadata: &std::fs::Metadata) -> Result<(u64, u64), SandboxError> {
    if !metadata.is_file() {
        return Err(SandboxError::UntrustedHelper);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(SandboxError::UntrustedHelper)
    }
}

fn copy_digest(
    mut source: &File,
    bytes: u64,
    destination: &mut impl Write,
) -> Result<String, SandboxError> {
    let before = source.metadata().map_err(invalid)?;
    source.rewind().map_err(invalid)?;
    let mut remaining = source.take(bytes.checked_add(1).ok_or(SandboxError::UntrustedHelper)?);
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = remaining.read(&mut buffer).map_err(invalid)?;
        if count == 0 {
            break;
        }
        copied += u64::try_from(count).map_err(invalid)?;
        if copied > bytes {
            return Err(SandboxError::UntrustedHelper);
        }
        hasher.update(&buffer[..count]);
        destination.write_all(&buffer[..count]).map_err(invalid)?;
    }
    let after = source.metadata().map_err(invalid)?;
    if copied != bytes
        || after.len() != bytes
        || before.modified().map_err(invalid)? != after.modified().map_err(invalid)?
    {
        return Err(SandboxError::UntrustedHelper);
    }
    Ok(hex_digest(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::ExecutableArtifactIdentity;

    #[test]
    #[allow(clippy::expect_used)]
    fn copied_content_requires_the_exact_captured_identity_and_digest() {
        let directory = tempfile::tempdir().expect("private fixture");
        let path = directory.path().join("input");
        std::fs::write(&path, b"approved code").expect("input");
        let path = path.canonicalize().expect("canonical input");
        let identity = ExecutableArtifactIdentity::capture(&path, 13).expect("identity");
        assert!(ExecutableArtifactIdentity::capture(&path, 12).is_err());
        let output = directory.path().join("output");
        let mut destination = std::fs::File::create(&output).expect("private destination");
        identity
            .copy_verified(&mut destination)
            .expect("approved copy");
        assert_eq!(
            std::fs::read(output).expect("copied bytes"),
            b"approved code"
        );
        std::fs::write(&path, b"replaced code").expect("same-inode same-length replacement");
        assert!(identity.copy_verified(&mut destination).is_err());
        let _original = std::fs::File::open(&path).expect("retain old inode");
        std::fs::remove_file(&path).expect("replace inode");
        std::fs::write(&path, b"approved code").expect("same bytes new inode");
        assert!(identity.copy_verified(&mut destination).is_err());
    }
}
