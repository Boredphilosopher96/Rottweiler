//! Bounded, trust-aware loading for TUI-only configuration.

use std::{
    fmt,
    io::Read as _,
    path::{Path, PathBuf},
};

const MAX_KEYBINDINGS_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) struct TuiConfigError {
    path: PathBuf,
    message: &'static str,
}

impl fmt::Display for TuiConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unsafe TUI keybindings at {}: {}",
            self.path.display(),
            self.message
        )
    }
}

impl std::error::Error for TuiConfigError {}

/// Loads the first keybinding file in extension precedence order.
///
/// Project files remain inert until the exact folder inventory is trusted.
pub(crate) fn load_keybindings(
    workspace: Option<&Path>,
    project_inventory: Option<&[rw_store::trust::TrustInventoryItem]>,
    user_home: &Path,
    user_rottweiler: &Path,
) -> Result<Option<String>, TuiConfigError> {
    let mut candidates = Vec::with_capacity(4);
    if project_inventory.is_some()
        && let Some(workspace) = workspace
    {
        candidates.extend([
            (
                workspace.to_path_buf(),
                PathBuf::from(".agents/keybindings.toml"),
                Some(".agents/keybindings.toml"),
            ),
            (
                workspace.to_path_buf(),
                PathBuf::from(".rottweiler/keybindings.toml"),
                Some(".rottweiler/keybindings.toml"),
            ),
        ]);
    }
    candidates.extend([
        (
            user_home.to_path_buf(),
            PathBuf::from(".agents/keybindings.toml"),
            None,
        ),
        (
            user_rottweiler.to_path_buf(),
            PathBuf::from("keybindings.toml"),
            None,
        ),
    ]);
    for (root, relative, project_relative) in candidates {
        if let Some(bytes) = read_relative(&root, &relative)? {
            if let Some(project_relative) = project_relative {
                let trusted = project_inventory
                    .and_then(|inventory| {
                        inventory.iter().find(|item| item.path == project_relative)
                    })
                    .is_some_and(|item| {
                        item.bytes == u64::try_from(bytes.len()).unwrap_or(u64::MAX)
                            && item.content_hash == blake3::hash(&bytes).to_hex().as_str()
                    });
                if !trusted {
                    return Err(TuiConfigError {
                        path: root.join(relative),
                        message: "content does not match the trusted project inventory",
                    });
                }
            }
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| TuiConfigError {
                    path: root.join(relative),
                    message: "content is not UTF-8",
                });
        }
    }
    Ok(None)
}

#[cfg(unix)]
fn read_relative(root: &Path, relative: &Path) -> Result<Option<Vec<u8>>, TuiConfigError> {
    use std::{fs::File, os::fd::OwnedFd};

    let path = root.join(relative);
    let fail = |message| TuiConfigError {
        path: path.clone(),
        message,
    };
    let mut directory: OwnedFd = match rustix::fs::open(
        root,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    ) {
        Ok(directory) => directory,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err(fail("configuration root is unavailable or unsafe")),
    };
    let components = relative.components().collect::<Vec<_>>();
    let Some((filename, parents)) = components.split_last() else {
        return Err(fail("configuration path is empty"));
    };
    for component in parents {
        let std::path::Component::Normal(name) = component else {
            return Err(fail("configuration path is not relative"));
        };
        directory = match rustix::fs::openat(
            &directory,
            *name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        ) {
            Ok(directory) => directory,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(_) => return Err(fail("configuration directory is unavailable or unsafe")),
        };
    }
    let std::path::Component::Normal(filename) = filename else {
        return Err(fail("configuration filename is invalid"));
    };
    let file = match rustix::fs::openat(
        &directory,
        *filename,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    ) {
        Ok(file) => file,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err(fail("configuration file is unavailable or unsafe")),
    };
    let stat = rustix::fs::fstat(&file).map_err(|_| fail("configuration metadata failed"))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
        return Err(fail("configuration must be a single-link regular file"));
    }
    if usize::try_from(stat.st_size).unwrap_or(usize::MAX) > MAX_KEYBINDINGS_BYTES {
        return Err(fail("configuration exceeds 64 KiB"));
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(stat.st_size)
            .unwrap_or(MAX_KEYBINDINGS_BYTES)
            .min(MAX_KEYBINDINGS_BYTES),
    );
    let mut file = File::from(file);
    (&mut file)
        .take((MAX_KEYBINDINGS_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| fail("configuration read failed"))?;
    if bytes.len() > MAX_KEYBINDINGS_BYTES {
        return Err(fail("configuration grew beyond 64 KiB"));
    }
    let after = rustix::fs::fstat(&file).map_err(|_| fail("configuration metadata failed"))?;
    if after.st_dev != stat.st_dev || after.st_ino != stat.st_ino || after.st_size != stat.st_size {
        return Err(fail("configuration changed while it was read"));
    }
    Ok(Some(bytes))
}

#[cfg(not(unix))]
fn read_relative(root: &Path, relative: &Path) -> Result<Option<Vec<u8>>, TuiConfigError> {
    let path = root.join(relative);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(TuiConfigError {
                path,
                message: "configuration metadata failed",
            });
        }
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || usize::try_from(metadata.len()).unwrap_or(usize::MAX) > MAX_KEYBINDINGS_BYTES
    {
        return Err(TuiConfigError {
            path,
            message: "configuration file is unsafe or oversized",
        });
    }
    std::fs::read(&path).map(Some).map_err(|_| TuiConfigError {
        path,
        message: "configuration read failed",
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn project_precedence_is_trust_gated_and_user_fallback_is_stable() {
        let root = tempfile::tempdir().expect("root");
        let project = root.path().join("project");
        let home = root.path().join("home");
        let rottweiler = home.join(".rottweiler");
        std::fs::create_dir_all(project.join(".agents")).expect("project agents");
        std::fs::create_dir_all(home.join(".agents")).expect("user agents");
        std::fs::create_dir_all(&rottweiler).expect("user rottweiler");
        std::fs::write(project.join(".agents/keybindings.toml"), "preset='vim'")
            .expect("project config");
        std::fs::write(home.join(".agents/keybindings.toml"), "preset='standard'")
            .expect("user config");
        assert_eq!(
            load_keybindings(Some(&project), None, &home, &rottweiler)
                .expect("untrusted fallback")
                .as_deref(),
            Some("preset='standard'")
        );
        let inventory = rw_store::trust::FolderTrustStore::new(root.path().join("trust.json"))
            .assess(&project)
            .expect("project inventory");
        assert_eq!(
            load_keybindings(
                Some(&project),
                Some(inventory.inventory()),
                &home,
                &rottweiler
            )
            .expect("trusted project")
            .as_deref(),
            Some("preset='vim'")
        );
        std::fs::write(project.join(".agents/keybindings.toml"), "preset='changed'")
            .expect("changed project config");
        assert!(
            load_keybindings(
                Some(&project),
                Some(inventory.inventory()),
                &home,
                &rottweiler
            )
            .is_err(),
            "project bytes must still match the exact trusted inventory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_oversized_files_fail_closed() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let home = root.path().join("home");
        let rottweiler = home.join(".rottweiler");
        std::fs::create_dir_all(home.join(".agents")).expect("agents");
        std::fs::create_dir_all(&rottweiler).expect("rottweiler");
        let outside = root.path().join("outside");
        std::fs::write(&outside, "preset='vim'").expect("outside");
        symlink(&outside, home.join(".agents/keybindings.toml")).expect("symlink");
        assert!(load_keybindings(None, None, &home, &rottweiler).is_err());
        std::fs::remove_file(home.join(".agents/keybindings.toml")).expect("remove symlink");
        std::fs::write(
            home.join(".agents/keybindings.toml"),
            vec![b'x'; MAX_KEYBINDINGS_BYTES + 1],
        )
        .expect("oversized");
        assert!(load_keybindings(None, None, &home, &rottweiler).is_err());
    }
}
