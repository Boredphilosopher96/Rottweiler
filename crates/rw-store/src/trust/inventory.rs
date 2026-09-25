//! Bounded, non-executing fingerprint of project files that change agent
//! behavior.
//!
//! The inventory covers every file under `.agents` and `.rottweiler`, plus the
//! `.claude/skills` and `.claude/commands` trees that extension discovery
//! reads in place. Symbolic links are admitted only inside a `skills` tree,
//! matching discovery: the link must resolve to a target owned by the current
//! user inside the project root or the user's home directory, and the target's
//! content is fingerprinted under the link's in-project path.

use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

use super::{
    FolderTrustError, MAX_INVENTORY_FILE_BYTES, MAX_INVENTORY_FILES, MAX_INVENTORY_TOTAL_BYTES,
    TrustInventoryItem,
};

/// Discovery locations and, when only part of a location is read, the
/// subdirectories that participate.
const LOCATIONS: [(&str, Option<&[&str]>); 3] = [
    (".agents", None),
    (".rottweiler", None),
    (".claude", Some(&["commands", "skills"])),
];

/// The only subtree where discovery follows symbolic links.
const LINKED_SUBTREE: &str = "skills";

pub(super) fn executable_inventory(
    workspace: &Path,
    user_home: Option<&Path>,
) -> Result<Vec<TrustInventoryItem>, FolderTrustError> {
    let mut walk = InventoryWalk {
        link_bounds: std::iter::once(workspace.to_owned())
            .chain(user_home.and_then(|home| fs::canonicalize(home).ok()))
            .collect(),
        files: Vec::new(),
        ancestors: Vec::new(),
    };
    for (location, subdirectories) in LOCATIONS {
        let root = workspace.join(location);
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(FolderTrustError::UnsafeEntry(root));
            }
            Ok(_) => match subdirectories {
                None => walk.directory(&root, &root, Tree::LocationRoot)?,
                Some(names) => {
                    for name in names {
                        walk.entry(&root.join(name), &root.join(name), Tree::LocationRoot)?;
                    }
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(FolderTrustError::Workspace { path: root, source });
            }
        }
    }
    let mut files = walk.files;
    files.sort_by(|left, right| left.display.cmp(&right.display));
    let mut total = 0_u64;
    let mut inventory = Vec::with_capacity(files.len());
    for file in files {
        let relative_path = file
            .display
            .strip_prefix(workspace)
            .map_err(|_| FolderTrustError::UnsafeEntry(file.display.clone()))?;
        let bytes = match &file.resolved {
            None => read_workspace_file(workspace, relative_path)?,
            Some(target) => read_resolved_file(target, &file.display)?,
        };
        let byte_count = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        total = total.saturating_add(byte_count);
        if total > MAX_INVENTORY_TOTAL_BYTES {
            return Err(FolderTrustError::TotalSize {
                limit: MAX_INVENTORY_TOTAL_BYTES,
            });
        }
        let relative = relative_path
            .to_str()
            .ok_or_else(|| FolderTrustError::NonUtf8Path(relative_path.to_owned()))?
            .replace('\\', "/");
        inventory.push(TrustInventoryItem {
            kind: inventory_kind(&relative).to_owned(),
            path: relative,
            content_hash: blake3::hash(&bytes).to_hex().to_string(),
            bytes: byte_count,
        });
    }
    Ok(inventory)
}

/// Where the walk currently is relative to the link-admitting subtree.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Tree {
    /// Direct children of a discovery location; only `skills` admits links.
    LocationRoot,
    /// Plain project content outside a `skills` tree.
    Plain,
    /// Inside a `skills` tree, still at an in-project path.
    Skills,
    /// Inside the resolved target of an admitted link.
    Resolved,
}

struct InventoryFile {
    /// In-project path the file is reported under.
    display: PathBuf,
    /// Canonical target when the file was reached through an admitted link.
    resolved: Option<PathBuf>,
}

struct InventoryWalk {
    link_bounds: Vec<PathBuf>,
    files: Vec<InventoryFile>,
    /// Canonical directories on the current descent, for link-cycle refusal.
    ancestors: Vec<PathBuf>,
}

impl InventoryWalk {
    fn directory(
        &mut self,
        display: &Path,
        actual: &Path,
        tree: Tree,
    ) -> Result<(), FolderTrustError> {
        let entries = fs::read_dir(actual).map_err(|source| FolderTrustError::Workspace {
            path: display.to_owned(),
            source,
        })?;
        self.ancestors.push(actual.to_owned());
        for entry in entries {
            let entry = entry.map_err(|source| FolderTrustError::Workspace {
                path: display.to_owned(),
                source,
            })?;
            let name = entry.file_name();
            self.entry(&display.join(&name), &actual.join(&name), tree)?;
        }
        self.ancestors.pop();
        Ok(())
    }

    fn entry(
        &mut self,
        display: &Path,
        actual: &Path,
        parent: Tree,
    ) -> Result<(), FolderTrustError> {
        let tree = match parent {
            Tree::LocationRoot
                if display.file_name() == Some(std::ffi::OsStr::new(LINKED_SUBTREE)) =>
            {
                Tree::Skills
            }
            Tree::LocationRoot => Tree::Plain,
            tree => tree,
        };
        let metadata = match fs::symlink_metadata(actual) {
            Ok(metadata) => metadata,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && parent == Tree::LocationRoot =>
            {
                return Ok(());
            }
            Err(source) => {
                return Err(FolderTrustError::Workspace {
                    path: display.to_owned(),
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink() {
            if tree == Tree::Plain {
                return Err(FolderTrustError::UnsafeEntry(display.to_owned()));
            }
            let target = self.resolve_link(display, actual)?;
            let target_metadata =
                fs::metadata(&target).map_err(|source| FolderTrustError::Workspace {
                    path: display.to_owned(),
                    source,
                })?;
            return if target_metadata.is_dir() {
                if self.ancestors.iter().any(|ancestor| ancestor == &target) {
                    return Err(FolderTrustError::UnsafeLink {
                        path: display.to_owned(),
                        reason: "forms a directory cycle",
                    });
                }
                self.directory(display, &target, Tree::Resolved)
            } else if target_metadata.is_file() {
                self.push_file(display, Some(target))
            } else {
                Err(FolderTrustError::UnsafeLink {
                    path: display.to_owned(),
                    reason: "does not resolve to a file or directory",
                })
            };
        }
        if metadata.is_dir() {
            self.directory(display, actual, tree)
        } else if metadata.is_file() {
            let resolved = (tree == Tree::Resolved).then(|| actual.to_owned());
            self.push_file(display, resolved)
        } else {
            Err(FolderTrustError::UnsafeEntry(display.to_owned()))
        }
    }

    fn push_file(
        &mut self,
        display: &Path,
        resolved: Option<PathBuf>,
    ) -> Result<(), FolderTrustError> {
        self.files.push(InventoryFile {
            display: display.to_owned(),
            resolved,
        });
        if self.files.len() > MAX_INVENTORY_FILES {
            return Err(FolderTrustError::FileLimit {
                limit: MAX_INVENTORY_FILES,
            });
        }
        Ok(())
    }

    /// Applies discovery's project link rule: a user-owned target inside the
    /// project root or the user's home directory.
    fn resolve_link(&self, display: &Path, actual: &Path) -> Result<PathBuf, FolderTrustError> {
        let unsafe_link = |reason| FolderTrustError::UnsafeLink {
            path: display.to_owned(),
            reason,
        };
        let target = fs::canonicalize(actual).map_err(|_| unsafe_link("does not resolve"))?;
        if !self
            .link_bounds
            .iter()
            .any(|bound| target.starts_with(bound))
        {
            return Err(unsafe_link(
                "resolves outside the project and the user's home directory",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = fs::metadata(&target).map_err(|_| unsafe_link("does not resolve"))?;
            if metadata.uid() != rustix::process::geteuid().as_raw() {
                return Err(unsafe_link("resolves to a target owned by another user"));
            }
        }
        Ok(target)
    }
}

/// Reads an in-project file without following any link on its path.
fn read_workspace_file(workspace: &Path, relative: &Path) -> Result<Vec<u8>, FolderTrustError> {
    let display = workspace.join(relative);
    #[cfg(unix)]
    let file = {
        use std::os::fd::OwnedFd;

        let directory_flags = rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW;
        let mut directory: OwnedFd =
            rustix::fs::open(workspace, directory_flags, rustix::fs::Mode::empty()).map_err(
                |source| FolderTrustError::Workspace {
                    path: workspace.to_owned(),
                    source: source.into(),
                },
            )?;
        if let Some(parent) = relative.parent() {
            for component in parent.components() {
                let std::path::Component::Normal(name) = component else {
                    return Err(FolderTrustError::UnsafeEntry(relative.to_owned()));
                };
                directory = rustix::fs::openat(
                    &directory,
                    name,
                    directory_flags,
                    rustix::fs::Mode::empty(),
                )
                .map_err(|source| FolderTrustError::Workspace {
                    path: display.clone(),
                    source: source.into(),
                })?;
            }
        }
        let file_name = relative
            .file_name()
            .ok_or_else(|| FolderTrustError::UnsafeEntry(relative.to_owned()))?;
        let descriptor = rustix::fs::openat(
            &directory,
            file_name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(|source| FolderTrustError::Workspace {
            path: display.clone(),
            source: source.into(),
        })?;
        fs::File::from(descriptor)
    };
    #[cfg(not(unix))]
    let file = fs::File::open(&display).map_err(|source| FolderTrustError::Workspace {
        path: display.clone(),
        source,
    })?;
    read_regular_file(file, &display)
}

/// Reads the canonical target of an admitted link. The target path contains
/// no links, so the final component is opened without following one.
fn read_resolved_file(target: &Path, display: &Path) -> Result<Vec<u8>, FolderTrustError> {
    let open_error = |source: std::io::Error| FolderTrustError::Workspace {
        path: display.to_owned(),
        source,
    };
    #[cfg(unix)]
    let file = fs::File::from(
        rustix::fs::open(
            target,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(|source| open_error(source.into()))?,
    );
    #[cfg(not(unix))]
    let file = fs::File::open(target).map_err(open_error)?;
    read_regular_file(file, display)
}

fn read_regular_file(file: fs::File, display: &Path) -> Result<Vec<u8>, FolderTrustError> {
    let metadata = file
        .metadata()
        .map_err(|source| FolderTrustError::Workspace {
            path: display.to_owned(),
            source,
        })?;
    if !metadata.is_file() {
        return Err(FolderTrustError::UnsafeEntry(display.to_owned()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_INVENTORY_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| FolderTrustError::Workspace {
            path: display.to_owned(),
            source,
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_INVENTORY_FILE_BYTES {
        return Err(FolderTrustError::FileSize {
            path: display.to_owned(),
            limit: MAX_INVENTORY_FILE_BYTES,
        });
    }
    Ok(bytes)
}

pub(super) fn inventory_kind(path: &str) -> &'static str {
    let normalized = [".agents/", ".rottweiler/", ".claude/"]
        .into_iter()
        .find_map(|prefix| path.strip_prefix(prefix))
        .unwrap_or(path);
    if normalized.starts_with("commands/") {
        "command"
    } else if normalized.starts_with("skills/") {
        "skill"
    } else if normalized.starts_with("agents/") {
        "agent"
    } else if normalized.starts_with("modes/") {
        "mode"
    } else if normalized.starts_with("workflows/") {
        "workflow"
    } else if normalized == "hooks.toml" {
        "hook"
    } else if normalized == "toolchain.toml" {
        "toolchain"
    } else if normalized == "plugins.toml" {
        "plugin"
    } else if matches!(normalized, "mcp.toml" | "mcp.json") {
        "mcp"
    } else if normalized == "config.toml" {
        "project_config"
    } else {
        "project_extension"
    }
}
