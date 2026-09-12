use super::super::{MAX_GLOBAL_REVIEW_DIFF_BYTES, MAX_GLOBAL_REVIEW_FILES};
use rw_core::{AgentLoopError, SessionReview};
use rw_store::checkpoint::CheckpointStore;
use rw_types::SessionId;
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

pub(super) fn group_checkpoint_paths(
    stores: &[Arc<CheckpointStore>],
    paths: Vec<PathBuf>,
) -> std::result::Result<BTreeMap<usize, Vec<PathBuf>>, AgentLoopError> {
    let mut grouped = BTreeMap::<usize, Vec<PathBuf>>::new();
    for path in paths {
        let mut components = path.components();
        let first = components.next();
        let virtual_target = match first {
            Some(std::path::Component::Normal(value)) if value == "@root" => {
                let index = match components.next() {
                    Some(std::path::Component::Normal(value)) => value
                        .to_str()
                        .and_then(|value| value.parse::<usize>().ok())
                        .filter(|index| *index > 0 && *index < stores.len())
                        .ok_or_else(|| {
                            AgentLoopError::Persistence(format!(
                                "checkpoint path has an invalid workspace-root index: {}",
                                path.display()
                            ))
                        })?,
                    _ => {
                        return Err(AgentLoopError::Persistence(format!(
                            "checkpoint path has no workspace-root index: {}",
                            path.display()
                        )));
                    }
                };
                let relative = components.collect::<PathBuf>();
                if relative.as_os_str().is_empty() {
                    return Err(AgentLoopError::Persistence(format!(
                        "checkpoint path names a workspace root rather than a file: {}",
                        path.display()
                    )));
                }
                Some((index, relative))
            }
            Some(
                std::path::Component::Normal(_)
                | std::path::Component::ParentDir
                | std::path::Component::CurDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_),
            ) => None,
            _ => {
                return Err(AgentLoopError::Persistence(format!(
                    "checkpoint path is not a confined workspace-relative path: {}",
                    path.display()
                )));
            }
        };
        let (root_index, relative) = if let Some(target) = virtual_target {
            target
        } else {
            resolve_checkpoint_path(stores, &path)?
        };
        grouped.entry(root_index).or_default().push(relative);
    }
    Ok(grouped)
}

fn resolve_checkpoint_path(
    stores: &[Arc<CheckpointStore>],
    path: &Path,
) -> std::result::Result<(usize, PathBuf), AgentLoopError> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        stores[0].workspace_root().join(path)
    };
    let canonical = match std::fs::canonicalize(&candidate) {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = candidate.parent().ok_or_else(|| {
                AgentLoopError::Persistence(format!(
                    "checkpoint path has no parent: {}",
                    path.display()
                ))
            })?;
            let filename = candidate.file_name().ok_or_else(|| {
                AgentLoopError::Persistence(format!(
                    "checkpoint path has no file name: {}",
                    path.display()
                ))
            })?;
            std::fs::canonicalize(parent)
                .map(|parent| parent.join(filename))
                .map_err(|error| {
                    AgentLoopError::Persistence(format!(
                        "checkpoint path parent is unavailable for {}: {error}",
                        path.display()
                    ))
                })?
        }
        Err(error) => {
            return Err(AgentLoopError::Persistence(format!(
                "checkpoint path is unavailable for {}: {error}",
                path.display()
            )));
        }
    };
    let (root_index, root) = stores
        .iter()
        .enumerate()
        .filter(|(_, store)| canonical.starts_with(store.workspace_root()))
        .max_by_key(|(_, store)| store.workspace_root().components().count())
        .ok_or_else(|| {
            AgentLoopError::Persistence(format!(
                "checkpoint path escapes every workspace root: {}",
                path.display()
            ))
        })?;
    let relative = canonical
        .strip_prefix(root.workspace_root())
        .map_err(|_| AgentLoopError::Persistence("checkpoint root mismatch".to_owned()))?
        .to_path_buf();
    if relative.as_os_str().is_empty() {
        return Err(AgentLoopError::Persistence(format!(
            "checkpoint path names a workspace root rather than a file: {}",
            path.display()
        )));
    }
    Ok((root_index, relative))
}

pub(super) fn checkpoint_display_path(root_index: usize, path: &str) -> String {
    if root_index == 0 {
        path.to_owned()
    } else {
        format!("@root/{root_index}/{path}")
    }
}

pub(super) fn resolve_review_display_path(
    store_count: usize,
    path: &Path,
) -> std::result::Result<(usize, PathBuf), AgentLoopError> {
    if path.is_absolute() {
        return Err(AgentLoopError::Persistence(
            "review path must be workspace-relative".to_owned(),
        ));
    }
    let mut components = path.components();
    let first = components
        .next()
        .ok_or_else(|| AgentLoopError::Persistence("review path must not be empty".to_owned()))?;
    let (root_index, relative) = match first {
        Component::Normal(value) if value == "@root" => {
            let root_index = components
                .next()
                .and_then(|component| match component {
                    Component::Normal(value) => value.to_str(),
                    _ => None,
                })
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|index| *index > 0 && *index < store_count)
                .ok_or_else(|| {
                    AgentLoopError::Persistence(
                        "review path has an invalid workspace-root index".to_owned(),
                    )
                })?;
            (root_index, components.collect::<PathBuf>())
        }
        Component::Normal(_) => (0, path.to_path_buf()),
        Component::Prefix(_) | Component::RootDir | Component::CurDir | Component::ParentDir => {
            return Err(AgentLoopError::Persistence(
                "review path is not a confined relative path".to_owned(),
            ));
        }
    };
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(AgentLoopError::Persistence(
            "review path is not a confined file path".to_owned(),
        ));
    }
    Ok((root_index, relative))
}

pub(super) fn merge_root_reviews(
    session_id: SessionId,
    reviews: Vec<SessionReview>,
) -> std::result::Result<SessionReview, AgentLoopError> {
    let file_count = reviews
        .iter()
        .map(|review| review.files.len())
        .sum::<usize>();
    if file_count > MAX_GLOBAL_REVIEW_FILES {
        return Err(AgentLoopError::Persistence(
            "session review exceeds the global file limit".to_owned(),
        ));
    }
    let mut remaining = MAX_GLOBAL_REVIEW_DIFF_BYTES;
    let mut files = Vec::with_capacity(file_count);
    for (root_index, review) in reviews.into_iter().enumerate() {
        for mut file in review.files {
            file.path = checkpoint_display_path(root_index, &file.path);
            if file.unified_diff.len() > remaining {
                let mut boundary = remaining;
                while boundary > 0 && !file.unified_diff.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                file.unified_diff.truncate(boundary);
                file.truncated = true;
            }
            remaining = remaining.saturating_sub(file.unified_diff.len());
            files.push(file);
        }
    }
    Ok(SessionReview { session_id, files })
}
