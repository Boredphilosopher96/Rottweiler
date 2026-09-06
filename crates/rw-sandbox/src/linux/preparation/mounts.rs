//! Bounded snapshot of inherited mount boundaries in the private namespace.
use crate::SandboxError;
use std::{
    fs::File,
    io::Read as _,
    os::unix::ffi::OsStringExt as _,
    path::{Path, PathBuf},
};

const MAX_MOUNTINFO_BYTES: u64 = 2 * 1024 * 1024;

pub(super) struct Mounts(Vec<PathBuf>);
impl Mounts {
    pub(super) fn capture() -> Result<Self, SandboxError> {
        let mut bytes = Vec::new();
        File::open("/proc/self/mountinfo")
            .map_err(super::sandbox_backend)?
            .take(MAX_MOUNTINFO_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(super::sandbox_backend)?;
        if bytes.len() as u64 > MAX_MOUNTINFO_BYTES {
            return Err(SandboxError::Unavailable(
                "source preparation mount table exceeds its byte limit".into(),
            ));
        }
        Self::parse(&bytes)
    }
    fn parse(bytes: &[u8]) -> Result<Self, SandboxError> {
        let mut paths = Vec::new();
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            if paths.len() >= super::MAX_VIEW_ENTRIES {
                return Err(SandboxError::Unavailable(
                    "source preparation mount table exceeds its entry limit".into(),
                ));
            }
            let field = line
                .split(|byte| *byte == b' ')
                .nth(4)
                .ok_or(SandboxError::MalformedHelper)?;
            let path = decode_path(field)?;
            if !path.is_absolute() {
                return Err(SandboxError::MalformedHelper);
            }
            paths.push(path);
        }
        if paths.is_empty() {
            return Err(SandboxError::MalformedHelper);
        }
        Ok(Self(paths))
    }
    pub(super) fn has_descendant(&self, path: &Path) -> bool {
        self.0
            .iter()
            .any(|mount| mount != path && mount.starts_with(path))
    }
}

// Bind-remount must preserve inherited locked flags as it adds read-only.
pub(super) fn readonly_flags(source: &File) -> Result<rustix::mount::MountFlags, SandboxError> {
    use rustix::{fs::StatVfsMountFlags as Vfs, mount::MountFlags as Mount};
    let existing = rustix::fs::fstatvfs(source)
        .map_err(super::sandbox_backend)?
        .f_flag;
    let mut flags = Mount::BIND | Mount::RDONLY | Mount::NOSUID;
    for (before, after) in [
        (Vfs::NODEV, Mount::NODEV),
        (Vfs::NOEXEC, Mount::NOEXEC),
        (Vfs::NOATIME, Mount::NOATIME),
        (Vfs::NODIRATIME, Mount::NODIRATIME),
        (Vfs::RELATIME, Mount::RELATIME),
        (Vfs::SYNCHRONOUS, Mount::SYNCHRONOUS),
        (Vfs::MANDLOCK, Mount::PERMIT_MANDATORY_FILE_LOCKING),
    ] {
        if existing.contains(before) {
            flags |= after;
        }
    }
    Ok(flags)
}

fn decode_path(field: &[u8]) -> Result<PathBuf, SandboxError> {
    let mut decoded = Vec::with_capacity(field.len());
    let mut remaining = field;
    while let Some((&byte, tail)) = remaining.split_first() {
        if byte == b'\\' {
            let escaped = tail.get(..3).ok_or(SandboxError::MalformedHelper)?;
            decoded.push(match escaped {
                b"040" => b' ',
                b"011" => b'\t',
                b"012" => b'\n',
                b"134" => b'\\',
                _ => return Err(SandboxError::MalformedHelper),
            });
            remaining = &tail[3..];
        } else {
            decoded.push(byte);
            remaining = tail;
        }
    }
    Ok(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
}

#[cfg(test)]
mod tests {
    use super::Mounts;
    use std::path::Path;

    #[test]
    fn nested_mounts_are_component_scoped_and_kernel_escapes_are_decoded() {
        let mounts = Mounts::parse(b"1 0 0:1 / / rw - overlay overlay rw\n2 1 0:2 / /usr/local/cache\\040volume rw - tmpfs tmpfs rw\n")
            .unwrap_or_else(|error| panic!("mount fixture: {error}"));
        assert!(mounts.has_descendant(Path::new("/usr")));
        assert!(mounts.has_descendant(Path::new("/usr/local")));
        assert!(!mounts.has_descendant(Path::new("/usr/local/cache volume")));
        assert!(!mounts.has_descendant(Path::new("/usr/local/cache")));
        assert!(!mounts.has_descendant(Path::new("/plugin")));
        assert!(Mounts::parse(b"1 0 0:1 / /bad\\999 rw\n").is_err());
        assert!(Mounts::parse(b"1 0 0:1 / relative rw\n").is_err());
    }
}
