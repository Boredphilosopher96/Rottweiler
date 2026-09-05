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

pub(super) fn skill_manifests(directory: &Path) -> ScanResult {
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
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            result.diagnostics.push(ScanDiagnostic {
                path: path.clone(),
                error: ExtensionDiscoveryError::UnsafeEntry { path },
            });
            continue;
        }
        let manifest = path.join("SKILL.md");
        match fs::symlink_metadata(&manifest) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                result.paths.push(manifest);
            }
            Ok(_) => result.diagnostics.push(ScanDiagnostic {
                path: manifest.clone(),
                error: ExtensionDiscoveryError::UnsafeEntry { path: manifest },
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => result.diagnostics.push(ScanDiagnostic {
                path: manifest.clone(),
                error: ExtensionDiscoveryError::Io {
                    path: manifest,
                    source,
                },
            }),
        }
    }
    result.paths.sort();
    result
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
) -> Result<Vec<PathBuf>, ExtensionDiscoveryError> {
    let result = skill_manifests(directory);
    if let Some(diagnostic) = result.diagnostics.into_iter().next() {
        Err(diagnostic.error)
    } else {
        Ok(result.paths)
    }
}

pub(super) fn collect_resource_paths(
    root: &Path,
    directory: &Path,
    paths: &mut Vec<PathBuf>,
) -> Result<(), ExtensionDiscoveryError> {
    ensure_directory(directory)?;
    for entry in fs::read_dir(directory).map_err(|source| ExtensionDiscoveryError::Io {
        path: directory.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| ExtensionDiscoveryError::Io {
            path: directory.to_owned(),
            source,
        })?;
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| ExtensionDiscoveryError::Io {
                path: path.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink() {
            return Err(ExtensionDiscoveryError::UnsafeEntry { path });
        }
        if metadata.is_dir() {
            collect_resource_paths(root, &path, paths)?;
        } else if metadata.is_file() {
            paths.push(
                path.strip_prefix(root)
                    .map_err(|_| ExtensionDiscoveryError::InvalidResourcePath {
                        path: path.clone(),
                    })?
                    .to_owned(),
            );
        } else {
            return Err(ExtensionDiscoveryError::UnsafeEntry { path });
        }
    }
    Ok(())
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

pub(super) fn ensure_directory(path: &Path) -> Result<(), ExtensionDiscoveryError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| ExtensionDiscoveryError::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(ExtensionDiscoveryError::UnsafeEntry {
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
