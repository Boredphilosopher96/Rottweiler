//! Deterministic depth-first traversal with a bounded directory sort frontier.
use crate::{CancellationToken, ToolError};
use ignore::WalkBuilder;
use std::{
    fs::{FileType, Metadata},
    path::{Path, PathBuf},
};

const MAX_PENDING_ENTRIES: usize = 16_384;
const MAX_PENDING_PATH_BYTES: usize = 4 * 1024 * 1024;

pub(super) struct Entry {
    path: PathBuf,
    kind: Option<FileType>,
}
impl Entry {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn file_type(&self) -> Option<FileType> {
        self.kind
    }
    pub fn metadata(&self) -> std::io::Result<Metadata> {
        std::fs::symlink_metadata(&self.path)
    }
}

pub(super) struct BoundedWalk {
    pending: Vec<Entry>,
    children: Vec<Entry>,
    expand: Option<PathBuf>,
    path_bytes: usize,
    recursive: bool,
    root: PathBuf,
    cancellation: CancellationToken,
    failed: bool,
    max_entries: usize,
    max_path_bytes: usize,
}
impl BoundedWalk {
    pub fn new(
        root: &Path,
        recursive: bool,
        cancellation: &CancellationToken,
    ) -> Result<Self, ToolError> {
        Self::with_limits(
            root,
            recursive,
            cancellation,
            MAX_PENDING_ENTRIES,
            MAX_PENDING_PATH_BYTES,
        )
    }
    fn with_limits(
        root: &Path,
        recursive: bool,
        cancellation: &CancellationToken,
        max_entries: usize,
        max_path_bytes: usize,
    ) -> Result<Self, ToolError> {
        cancellation.check()?;
        if root.as_os_str().len() > max_path_bytes || max_entries == 0 {
            return Err(exhausted());
        }
        let first = WalkBuilder::new(root)
            .max_depth(Some(0))
            .standard_filters(true)
            .follow_links(false)
            .build()
            .find_map(Result::ok);
        let pending = first.map_or_else(Vec::new, |entry| {
            vec![Entry {
                path: entry.path().to_owned(),
                kind: entry.file_type(),
            }]
        });
        Ok(Self {
            pending,
            children: Vec::new(),
            expand: None,
            path_bytes: root.as_os_str().len(),
            recursive,
            root: root.to_owned(),
            cancellation: cancellation.clone(),
            failed: false,
            max_entries,
            max_path_bytes,
        })
    }
    fn expand(&mut self, directory: &Path) -> Result<(), ToolError> {
        // Walk only one unsorted level. ignore owns the same standard parent,
        // repository, hidden and nested ignore decisions; no library directory
        // sorter can collect entries before our admission check.
        for entry in WalkBuilder::new(directory)
            .max_depth(Some(1))
            .standard_filters(true)
            .follow_links(false)
            .build()
            .filter_map(Result::ok)
        {
            self.cancellation.check()?;
            if entry.depth() == 0 {
                continue;
            }
            let bytes = entry.path().as_os_str().len();
            if self.pending.len() + self.children.len() >= self.max_entries
                || self
                    .path_bytes
                    .checked_add(bytes)
                    .is_none_or(|bytes| bytes > self.max_path_bytes)
            {
                return Err(exhausted());
            }
            self.path_bytes += bytes;
            self.children.push(Entry {
                path: entry.path().to_owned(),
                kind: entry.file_type(),
            });
        }
        self.children
            .sort_unstable_by(|left, right| left.path.cmp(&right.path));
        // Both vector backings are independently capped by max_entries. Paths
        // move between them; their combined owned bytes remain path_bytes.
        self.pending.extend(self.children.drain(..).rev());
        Ok(())
    }
}
impl Iterator for BoundedWalk {
    type Item = Result<Entry, ToolError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let next = (|| {
            self.cancellation.check()?;
            if let Some(directory) = self.expand.take() {
                self.expand(&directory)?;
            }
            let Some(entry) = self.pending.pop() else {
                return Ok(None);
            };
            self.path_bytes -= entry.path.as_os_str().len();
            if entry.kind.is_some_and(|kind| kind.is_dir())
                && (self.recursive || entry.path == self.root)
            {
                self.expand = Some(entry.path.clone());
            }
            Ok(Some(entry))
        })();
        match next {
            Ok(entry) => entry.map(Ok),
            Err(error) => {
                self.failed = true;
                Some(Err(error))
            }
        }
    }
}
fn exhausted() -> ToolError {
    ToolError::Command(
        "directory sorting exceeds its bounded entry/path allowance; search a narrower path".into(),
    )
}

#[cfg(test)]
mod tests;
