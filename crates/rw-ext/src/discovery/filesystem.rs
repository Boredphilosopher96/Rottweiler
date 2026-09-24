use super::{Component, ExtensionDiscoveryError, Path, PathBuf, fs};

#[derive(Debug)]
pub(super) struct ScanDiagnostic {
    pub(super) path: PathBuf,
    pub(super) error: ExtensionDiscoveryError,
}

#[derive(Debug, Default)]
pub(super) struct ScanResult {
    pub(super) paths: Vec<PathBuf>,
    pub(super) diagnostics: Vec<ScanDiagnostic>,
}

pub(super) fn regular_children_with_extension(directory: &Path, extension: &str) -> ScanResult {
    let mut result = ScanResult::default();
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return result,
        Err(source) => {
            result.diagnostics.push(ScanDiagnostic {
                path: directory.to_owned(),
                error: ExtensionDiscoveryError::Io {
                    path: directory.to_owned(),
                    source,
                },
            });
            return result;
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        result.diagnostics.push(ScanDiagnostic {
            path: directory.to_owned(),
            error: ExtensionDiscoveryError::UnsafeEntry {
                path: directory.to_owned(),
            },
        });
        return result;
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(source) => {
            result.diagnostics.push(ScanDiagnostic {
                path: directory.to_owned(),
                error: ExtensionDiscoveryError::Io {
                    path: directory.to_owned(),
                    source,
                },
            });
            return result;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                result.diagnostics.push(ScanDiagnostic {
                    path: directory.to_owned(),
                    error: ExtensionDiscoveryError::Io {
                        path: directory.to_owned(),
                        source,
                    },
                });
                continue;
            }
        };
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(source) => {
                result.diagnostics.push(ScanDiagnostic {
                    path: path.clone(),
                    error: ExtensionDiscoveryError::Io {
                        path: path.clone(),
                        source,
                    },
                });
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            result.diagnostics.push(ScanDiagnostic {
                path: path.clone(),
                error: ExtensionDiscoveryError::UnsafeEntry { path },
            });
        } else if metadata.is_file() {
            if path.extension().is_some_and(|value| value == extension) {
                result.paths.push(path);
            }
        } else if path.extension().is_some_and(|value| value == extension) {
            result.diagnostics.push(ScanDiagnostic {
                path: path.clone(),
                error: ExtensionDiscoveryError::UnsafeEntry { path },
            });
        }
    }
    result.paths.sort();
    result
}

/// One SKILL.md manifest located under a `skills/` directory.
#[derive(Clone, Debug)]
pub(super) struct SkillManifest {
    /// Manifest path as found under the skills directory; it may traverse a
    /// resolved symbolic link and is what diagnostics and origins report.
    pub(super) path: PathBuf,
    /// Canonical skill directory. Every later read is anchored here.
    pub(super) root: PathBuf,
    /// Skills directory entry name, the default skill name.
    pub(super) entry_name: String,
}

#[derive(Debug, Default)]
pub(super) struct SkillScan {
    pub(super) manifests: Vec<SkillManifest>,
    pub(super) diagnostics: Vec<ScanDiagnostic>,
}

/// Where a symbolic link inside an extension root may resolve.
#[derive(Clone, Copy, Debug)]
pub(super) enum LinkPolicy<'a> {
    /// User configuration: any target owned by the current user.
    User,
    /// Project configuration: a user-owned target inside the project root or
    /// the user's home directory.
    Project {
        project_root: &'a Path,
        user_home: &'a Path,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinkTarget {
    Directory,
    File,
}

/// Resolves one symbolic link once and returns its canonical target when the
/// policy admits it.
fn resolve_link(
    path: &Path,
    policy: LinkPolicy<'_>,
    expected: LinkTarget,
) -> Result<PathBuf, ExtensionDiscoveryError> {
    let unsafe_link = |reason: &'static str| ExtensionDiscoveryError::UnsafeLink {
        path: path.to_owned(),
        reason,
    };
    let canonical = fs::canonicalize(path).map_err(|_| unsafe_link("does not resolve"))?;
    let metadata = fs::metadata(&canonical).map_err(|source| ExtensionDiscoveryError::Io {
        path: path.to_owned(),
        source,
    })?;
    let kind_matches = match expected {
        LinkTarget::Directory => metadata.is_dir(),
        LinkTarget::File => metadata.is_file(),
    };
    if !kind_matches {
        return Err(unsafe_link(match expected {
            LinkTarget::Directory => "does not resolve to a directory",
            LinkTarget::File => "does not resolve to a regular file",
        }));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(unsafe_link("resolves to a target owned by another user"));
        }
    }
    if let LinkPolicy::Project {
        project_root,
        user_home,
    } = policy
    {
        let inside = [project_root, user_home]
            .into_iter()
            .any(|bound| fs::canonicalize(bound).is_ok_and(|bound| canonical.starts_with(bound)));
        if !inside {
            return Err(unsafe_link(
                "resolves outside the project and the user's home directory",
            ));
        }
    }
    Ok(canonical)
}

/// Returns the directory to scan, following a permitted link for the
/// `skills/` directory itself.
fn scan_directory(
    directory: &Path,
    policy: LinkPolicy<'_>,
) -> Result<Option<PathBuf>, ExtensionDiscoveryError> {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ExtensionDiscoveryError::Io {
                path: directory.to_owned(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        return resolve_link(directory, policy, LinkTarget::Directory).map(Some);
    }
    if metadata.is_dir() {
        Ok(Some(directory.to_owned()))
    } else {
        Err(ExtensionDiscoveryError::UnsafeEntry {
            path: directory.to_owned(),
        })
    }
}

pub(super) fn skill_manifests(directory: &Path, policy: LinkPolicy<'_>) -> SkillScan {
    let mut result = SkillScan::default();
    let scanned = match scan_directory(directory, policy) {
        Ok(Some(scanned)) => scanned,
        Ok(None) => return result,
        Err(error) => {
            result.diagnostics.push(ScanDiagnostic {
                path: directory.to_owned(),
                error,
            });
            return result;
        }
    };
    let entries = match fs::read_dir(&scanned) {
        Ok(entries) => entries,
        Err(source) => {
            result.diagnostics.push(ScanDiagnostic {
                path: directory.to_owned(),
                error: ExtensionDiscoveryError::Io {
                    path: directory.to_owned(),
                    source,
                },
            });
            return result;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                result.diagnostics.push(ScanDiagnostic {
                    path: directory.to_owned(),
                    error: ExtensionDiscoveryError::Io {
                        path: directory.to_owned(),
                        source,
                    },
                });
                continue;
            }
        };
        let path = directory.join(entry.file_name());
        let Some(entry_name) = entry.file_name().to_str().map(str::to_owned) else {
            result.diagnostics.push(ScanDiagnostic {
                path: path.clone(),
                error: ExtensionDiscoveryError::InvalidPath { path },
            });
            continue;
        };
        if entry_name.starts_with('.') {
            continue;
        }
        match skill_manifest(&scanned.join(&entry_name), &path, entry_name, policy) {
            Ok(Some(manifest)) => result.manifests.push(manifest),
            Ok(None) => {}
            Err(error) => result.diagnostics.push(ScanDiagnostic {
                path: super::discovery_error_path(&error).to_owned(),
                error,
            }),
        }
    }
    result
        .manifests
        .sort_by(|left, right| left.path.cmp(&right.path));
    result
}

/// Resolves one skill directory entry. Plain files beside skill directories
/// are ignored; directories without SKILL.md are not skills.
fn skill_manifest(
    entry: &Path,
    display: &Path,
    entry_name: String,
    policy: LinkPolicy<'_>,
) -> Result<Option<SkillManifest>, ExtensionDiscoveryError> {
    let metadata = fs::symlink_metadata(entry).map_err(|source| ExtensionDiscoveryError::Io {
        path: display.to_owned(),
        source,
    })?;
    let directory = if metadata.file_type().is_symlink() {
        match fs::metadata(entry) {
            Ok(target) if !target.is_dir() => return Ok(None),
            _ => resolve_link(display, policy, LinkTarget::Directory)?,
        }
    } else if metadata.is_dir() {
        entry.to_owned()
    } else {
        return Ok(None);
    };
    let manifest = directory.join("SKILL.md");
    let display_manifest = display.join("SKILL.md");
    let manifest_metadata = match fs::symlink_metadata(&manifest) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ExtensionDiscoveryError::Io {
                path: display_manifest,
                source,
            });
        }
    };
    let root = if manifest_metadata.file_type().is_symlink() {
        let target = resolve_link(&display_manifest, policy, LinkTarget::File)?;
        if target.file_name() != Some(std::ffi::OsStr::new("SKILL.md")) {
            return Err(ExtensionDiscoveryError::UnsafeLink {
                path: display_manifest,
                reason: "must resolve to a file named SKILL.md",
            });
        }
        target
            .parent()
            .ok_or_else(|| ExtensionDiscoveryError::InvalidPath {
                path: display_manifest.clone(),
            })?
            .to_owned()
    } else if manifest_metadata.is_file() {
        fs::canonicalize(&directory).map_err(|source| ExtensionDiscoveryError::Io {
            path: display.to_owned(),
            source,
        })?
    } else {
        return Err(ExtensionDiscoveryError::UnsafeEntry {
            path: display_manifest,
        });
    };
    Ok(Some(SkillManifest {
        path: display_manifest,
        root,
        entry_name,
    }))
}

pub(super) fn strict_regular_children_with_extension(
    directory: &Path,
    extension: &str,
) -> Result<Vec<PathBuf>, ExtensionDiscoveryError> {
    let result = regular_children_with_extension(directory, extension);
    if let Some(diagnostic) = result.diagnostics.into_iter().next() {
        Err(diagnostic.error)
    } else {
        Ok(result.paths)
    }
}

pub(super) fn strict_skill_manifests(
    directory: &Path,
    policy: LinkPolicy<'_>,
) -> Result<Vec<SkillManifest>, ExtensionDiscoveryError> {
    let result = skill_manifests(directory, policy);
    if let Some(diagnostic) = result.diagnostics.into_iter().next() {
        Err(diagnostic.error)
    } else {
        Ok(result.manifests)
    }
}

pub(super) fn validate_relative_resource(path: &Path) -> Result<(), ExtensionDiscoveryError> {
    let valid = !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if valid {
        Ok(())
    } else {
        Err(ExtensionDiscoveryError::InvalidResourcePath {
            path: path.to_owned(),
        })
    }
}

pub(super) fn ensure_regular_file(path: &Path) -> Result<fs::Metadata, ExtensionDiscoveryError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| ExtensionDiscoveryError::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.is_file() && !metadata.file_type().is_symlink() {
        Ok(metadata)
    } else {
        Err(ExtensionDiscoveryError::UnsafeEntry {
            path: path.to_owned(),
        })
    }
}

pub(super) fn read_bounded_utf8(
    path: &Path,
    limit: u64,
) -> Result<String, ExtensionDiscoveryError> {
    let bytes = read_bounded_regular_file(path, limit)?;
    String::from_utf8(bytes).map_err(|_| ExtensionDiscoveryError::NotUtf8 {
        path: path.to_owned(),
    })
}

pub(crate) fn read_bounded_relative_utf8(
    root: &Path,
    relative: &Path,
    limit: u64,
) -> Result<String, ExtensionDiscoveryError> {
    let bytes = read_bounded_relative_file(root, relative, limit)?;
    String::from_utf8(bytes).map_err(|_| ExtensionDiscoveryError::NotUtf8 {
        path: root.join(relative),
    })
}

pub(super) fn read_bounded_regular_file(
    path: &Path,
    limit: u64,
) -> Result<Vec<u8>, ExtensionDiscoveryError> {
    let metadata = ensure_regular_file(path)?;
    if metadata.len() > limit {
        return Err(ExtensionDiscoveryError::TooLarge {
            path: path.to_owned(),
            limit,
        });
    }
    let bytes = fs::read(path).map_err(|source| ExtensionDiscoveryError::Io {
        path: path.to_owned(),
        source,
    })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(ExtensionDiscoveryError::TooLarge {
            path: path.to_owned(),
            limit,
        });
    }
    Ok(bytes)
}

pub(super) fn read_bounded_relative_file(
    root: &Path,
    relative: &Path,
    limit: u64,
) -> Result<Vec<u8>, ExtensionDiscoveryError> {
    validate_relative_resource(relative)?;
    #[cfg(unix)]
    {
        use std::io::Read;

        let mut directory = rustix::fs::open(
            root,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|source| ExtensionDiscoveryError::Io {
            path: root.to_owned(),
            source: source.into(),
        })?;
        let components = relative.components().collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            let Component::Normal(name) = component else {
                return Err(ExtensionDiscoveryError::InvalidResourcePath {
                    path: relative.to_owned(),
                });
            };
            let final_component = index.saturating_add(1) == components.len();
            let mut flags = rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC;
            if !final_component {
                flags |= rustix::fs::OFlags::DIRECTORY;
            }
            let opened = rustix::fs::openat(&directory, *name, flags, rustix::fs::Mode::empty())
                .map_err(|source| ExtensionDiscoveryError::Io {
                    path: root.join(relative),
                    source: source.into(),
                })?;
            if final_component {
                let file = fs::File::from(opened);
                let metadata = file
                    .metadata()
                    .map_err(|source| ExtensionDiscoveryError::Io {
                        path: root.join(relative),
                        source,
                    })?;
                if !metadata.is_file() {
                    return Err(ExtensionDiscoveryError::UnsafeEntry {
                        path: root.join(relative),
                    });
                }
                if metadata.len() > limit {
                    return Err(ExtensionDiscoveryError::TooLarge {
                        path: root.join(relative),
                        limit,
                    });
                }
                let take_limit = limit.saturating_add(1);
                let mut bytes = Vec::new();
                file.take(take_limit)
                    .read_to_end(&mut bytes)
                    .map_err(|source| ExtensionDiscoveryError::Io {
                        path: root.join(relative),
                        source,
                    })?;
                if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
                    return Err(ExtensionDiscoveryError::TooLarge {
                        path: root.join(relative),
                        limit,
                    });
                }
                return Ok(bytes);
            }
            directory = opened;
        }
        Err(ExtensionDiscoveryError::InvalidResourcePath {
            path: relative.to_owned(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = limit;
        Err(ExtensionDiscoveryError::UnsafeEntry {
            path: root.join(relative),
        })
    }
}
