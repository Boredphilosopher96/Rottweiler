//! Read grants distinguish declared roots from discovered safe siblings.
use super::{RootKind, SandboxError, sandbox_backend};
use std::os::fd::{AsFd as _, OwnedFd};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy)]
pub(super) enum ReadGrant {
    Required(RootKind),
    Discovered(RootKind),
}
impl ReadGrant {
    pub(super) fn kind(self) -> RootKind {
        match self {
            Self::Required(kind) | Self::Discovered(kind) => kind,
        }
    }
    pub(super) fn open(self, path: &Path) -> Result<Option<OwnedFd>, SandboxError> {
        match self {
            Self::Required(kind) => open_landlock_root(path, kind).map(Some),
            Self::Discovered(kind) => open_existing_landlock_root(path, kind),
        }
    }
}

pub(super) fn collect_system_read_root(
    root: &Path,
    homes: &[PathBuf],
    grants: &mut BTreeMap<PathBuf, ReadGrant>,
) -> Result<(), SandboxError> {
    let Some((root, kind)) = absolute_existing_root(root)? else {
        return Ok(());
    };
    if homes.iter().any(|home| root.starts_with(home)) {
        return Ok(());
    }
    let excluded = homes
        .iter()
        .filter(|home| home.starts_with(&root))
        .cloned()
        .collect::<Vec<_>>();
    if kind == RootKind::Directory && !excluded.is_empty() {
        return collect_directory_except(&root, &excluded, grants).map_err(sandbox_backend);
    }
    grants.insert(root, ReadGrant::Required(kind));
    Ok(())
}

/// Adds an explicitly authorized root while carving credential paths out
/// of any parent grant. Landlock has no deny rule, so a home-directory
/// workspace is represented by rules for its existing safe siblings. This
/// preserves reads of existing workspace content, but the home directory
/// itself cannot be listed and new top-level entries are not readable until
/// a future sandbox invocation rebuilds the snapshot.
pub(super) fn collect_authorized_read_root(
    root: &Path,
    kind: RootKind,
    sensitive: &[PathBuf],
    grants: &mut BTreeMap<PathBuf, ReadGrant>,
) -> Result<(), SandboxError> {
    if sensitive.iter().any(|secret| root.starts_with(secret)) {
        return Ok(());
    }
    let excluded = sensitive
        .iter()
        .filter(|secret| secret.starts_with(root))
        .cloned()
        .collect::<Vec<_>>();
    if kind == RootKind::Directory && !excluded.is_empty() {
        collect_directory_except(root, &excluded, grants).map_err(sandbox_backend)
    } else {
        grants.insert(root.to_path_buf(), ReadGrant::Required(kind));
        Ok(())
    }
}

fn collect_directory_except(
    root: &Path,
    excluded: &[PathBuf],
    grants: &mut BTreeMap<PathBuf, ReadGrant>,
) -> io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        collect_discovered_root(&entry.path(), root, excluded, grants)?;
    }
    Ok(())
}

pub(super) fn collect_discovered_root(
    path: &Path,
    root: &Path,
    excluded: &[PathBuf],
    grants: &mut BTreeMap<PathBuf, ReadGrant>,
) -> io::Result<()> {
    let mut collect = || -> io::Result<()> {
        // Following a sibling symlink could grant an object outside the root.
        if path.symlink_metadata()?.file_type().is_symlink() {
            return Ok(());
        }
        let canonical = path.canonicalize()?;
        if !canonical.starts_with(root)
            || excluded
                .iter()
                .any(|secret| path == secret || canonical == *secret)
        {
            return Ok(());
        }
        let nested = excluded
            .iter()
            .filter(|secret| secret.starts_with(path) || secret.starts_with(&canonical))
            .cloned()
            .collect::<Vec<_>>();
        let kind = RootKind::for_metadata(&canonical.metadata()?);
        if kind == RootKind::Directory && !nested.is_empty() {
            collect_directory_except(path, &nested, grants)
        } else {
            // An explicit grant for this path must retain its stricter contract.
            grants
                .entry(canonical)
                .or_insert(ReadGrant::Discovered(kind));
            Ok(())
        }
    };
    match collect() {
        // Optional safe siblings can disappear during enumeration. Omitting
        // them grants no new authority; declared roots never use this path.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

pub(super) fn absolute_existing_root(
    root: &Path,
) -> Result<Option<(PathBuf, RootKind)>, SandboxError> {
    if !root.is_absolute() {
        return Ok(None);
    }
    let canonical = match root.canonicalize() {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(sandbox_backend(error)),
    };
    let metadata = canonical.metadata().map_err(sandbox_backend)?;
    Ok(Some((canonical, RootKind::for_metadata(&metadata))))
}

pub(super) fn open_landlock_root(root: &Path, expected: RootKind) -> Result<OwnedFd, SandboxError> {
    open_existing_landlock_root(root, expected)?
        .ok_or_else(|| sandbox_backend(io::Error::from(io::ErrorKind::NotFound)))
}

fn open_existing_landlock_root(
    root: &Path,
    expected: RootKind,
) -> Result<Option<OwnedFd>, SandboxError> {
    // Policy roots are canonical paths. Refuse a substituted symlink in any
    // component, including a same-kind replacement that a type check cannot
    // detect. The descriptor used for classification is the actual rule parent.
    let root = match rustix::fs::openat2(
        rustix::fs::CWD,
        root,
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
        rustix::fs::ResolveFlags::NO_SYMLINKS,
    ) {
        Ok(root) => root,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(sandbox_backend(error)),
    };
    let metadata = rustix::fs::fstat(root.as_fd()).map_err(sandbox_backend)?;
    let actual = if rustix::fs::FileType::from_raw_mode(metadata.st_mode).is_dir() {
        RootKind::Directory
    } else {
        RootKind::NonDirectory
    };
    if actual != expected {
        return Err(SandboxError::RootTypeChanged);
    }
    Ok(Some(root))
}
