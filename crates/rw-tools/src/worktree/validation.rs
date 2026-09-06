use std::path::{Component, Path, PathBuf};

use rw_types::{Cost, DiffArtifact, TouchedFile, TouchedFileStatus, Usage};

use crate::registry::ToolError;

use super::{MAX_DIFF_BYTES, MAX_FINAL_TEXT_BYTES, MAX_TOUCHED_FILES, WorktreeLimits};

pub(super) fn parse_touched_files(
    bytes: &[u8],
    limit: usize,
) -> Result<Vec<TouchedFile>, ToolError> {
    let mut touched = Vec::new();
    let mut fields = bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    while let Some(status) = fields.next() {
        if touched.len() >= limit {
            return Err(ToolError::SizeLimit { limit });
        }
        let status = std::str::from_utf8(status)
            .map_err(|_| ToolError::Output("git emitted a non-UTF-8 change status".to_owned()))?;
        let path = fields.next().ok_or_else(|| {
            ToolError::Output("git emitted a changed status without a path".to_owned())
        })?;
        let path = std::str::from_utf8(path)
            .map_err(|_| ToolError::Output("git emitted a non-UTF-8 changed path".to_owned()))?;
        let status = match status.as_bytes().first() {
            Some(b'A') => TouchedFileStatus::Added,
            Some(b'M') => TouchedFileStatus::Modified,
            Some(b'D') => TouchedFileStatus::Deleted,
            Some(b'T') => TouchedFileStatus::TypeChanged,
            _ => {
                return Err(ToolError::Output(format!(
                    "git emitted unsupported change status {status:?}"
                )));
            }
        };
        let path = PathBuf::from(path);
        validate_relative_path(&path)?;
        touched.push(TouchedFile {
            path: path.to_string_lossy().into_owned(),
            status,
        });
    }
    touched.sort_by(|left, right| left.path.cmp(&right.path));
    touched.dedup_by(|left, right| left.path == right.path);
    Ok(touched)
}

pub(super) fn parse_untracked_paths(bytes: &[u8]) -> Result<Vec<PathBuf>, ToolError> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| {
            let field = std::str::from_utf8(field).map_err(|_| {
                ToolError::Output("git emitted a non-UTF-8 untracked path".to_owned())
            })?;
            let path = PathBuf::from(field);
            validate_relative_path(&path)?;
            Ok(path)
        })
        .collect()
}

pub(super) fn validate_relative_path(path: &Path) -> Result<(), ToolError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ToolError::InvalidInput(format!(
            "unsafe worktree artifact path: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(super) fn artifact_id(
    base_commit: &str,
    touched_files: &[TouchedFile],
    unified_diff: &str,
) -> Result<String, ToolError> {
    let manifest = serde_json::to_vec(touched_files)
        .map_err(|source| ToolError::Output(source.to_string()))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rottweiler.worktree-diff.v1\0");
    hasher.update(base_commit.as_bytes());
    hasher.update(b"\0");
    hasher.update(&manifest);
    hasher.update(b"\0");
    hasher.update(unified_diff.as_bytes());
    Ok(hasher.finalize().to_hex().to_string())
}

pub(super) fn verify_artifact(artifact: &DiffArtifact) -> Result<(), ToolError> {
    let expected = artifact_id(
        &artifact.base_commit,
        &artifact.touched_files,
        &artifact.unified_diff,
    )?;
    if artifact.id != expected {
        return Err(ToolError::InvalidInput(
            "worktree diff artifact digest did not match its contents".to_owned(),
        ));
    }
    validate_oid(&artifact.base_commit)?;
    for touched in &artifact.touched_files {
        validate_relative_path(Path::new(&touched.path))?;
    }
    Ok(())
}

pub(super) fn validate_artifact_reference_id(artifact_id: &str) -> Result<(), ToolError> {
    if artifact_id.len() != 64
        || !artifact_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ToolError::InvalidInput(
            "worktree diff artifact reference is malformed".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn validate_oid(oid: &str) -> Result<(), ToolError> {
    if !(oid.len() == 40 || oid.len() == 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ToolError::Output(
            "git returned an invalid commit id".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn validate_limits(limits: WorktreeLimits) -> Result<(), ToolError> {
    if limits.max_diff_bytes == 0 || limits.max_diff_bytes > MAX_DIFF_BYTES {
        return Err(ToolError::InvalidInput(format!(
            "worktree diff bound must be between 1 and {MAX_DIFF_BYTES} bytes"
        )));
    }
    if limits.max_final_text_bytes == 0 || limits.max_final_text_bytes > MAX_FINAL_TEXT_BYTES {
        return Err(ToolError::InvalidInput(format!(
            "child final-text bound must be between 1 and {MAX_FINAL_TEXT_BYTES} bytes"
        )));
    }
    if limits.max_touched_files == 0 || limits.max_touched_files > MAX_TOUCHED_FILES {
        return Err(ToolError::InvalidInput(format!(
            "touched-file bound must be between 1 and {MAX_TOUCHED_FILES}"
        )));
    }
    Ok(())
}

pub(super) fn canonical_directory(path: &Path, label: &'static str) -> Result<PathBuf, ToolError> {
    let canonical = path.canonicalize().map_err(|source| ToolError::Io {
        operation: "canonicalize directory",
        path: path.to_path_buf(),
        source,
    })?;
    if !canonical.is_dir() {
        return Err(ToolError::InvalidInput(format!(
            "{label} is not a directory: {}",
            path.display()
        )));
    }
    Ok(canonical)
}

pub(super) fn absolute_without_parent_components(path: &Path) -> Result<PathBuf, ToolError> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ToolError::InvalidInput(
            "private worktree storage cannot contain parent traversal".to_owned(),
        ));
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|source| ToolError::Io {
                operation: "resolve private worktree root",
                path: path.to_path_buf(),
                source,
            })
    }
}

pub(super) fn projected_canonical_path(path: &Path) -> Result<PathBuf, ToolError> {
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or_else(|| {
            ToolError::InvalidInput("private worktree root has no existing ancestor".to_owned())
        })?;
        suffix.push(name.to_os_string());
        existing = existing.parent().ok_or_else(|| {
            ToolError::InvalidInput("private worktree root has no existing ancestor".to_owned())
        })?;
    }
    let mut projected = existing.canonicalize().map_err(|source| ToolError::Io {
        operation: "canonicalize private worktree ancestor",
        path: existing.to_path_buf(),
        source,
    })?;
    for component in suffix.iter().rev() {
        projected.push(component);
    }
    Ok(projected)
}

pub(super) fn empty_accounting() -> (Usage, Cost) {
    (
        Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        },
        Cost::Unavailable {
            reason: "not applicable to worktree finalization".to_owned(),
        },
    )
}

#[cfg(unix)]
pub(super) fn set_private_permissions(path: &Path) -> Result<(), ToolError> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|source| {
        ToolError::Io {
            operation: "secure private worktree directory",
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(unix)]
pub(super) fn require_private_permissions(path: &Path) -> Result<(), ToolError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::symlink_metadata(path)
        .map_err(|source| ToolError::Io {
            operation: "inspect private worktree directory permissions",
            path: path.to_path_buf(),
            source,
        })?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(ToolError::InvalidInput(
            "private worktree storage must not be accessible by group or other users".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn set_private_permissions(_path: &Path) -> Result<(), ToolError> {
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn require_private_permissions(_path: &Path) -> Result<(), ToolError> {
    Ok(())
}
