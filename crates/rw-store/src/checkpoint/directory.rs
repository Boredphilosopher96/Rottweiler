//! Durable publication of a bounded newly created directory chain.
use super::CheckpointError;
use std::{
    fs::{self, File},
    io,
    path::Path,
};

pub(super) fn create_directory_durable(path: &Path) -> Result<(), CheckpointError> {
    Creation::plan(path)?.publish(|directory| File::open(directory)?.sync_all())
}

struct Creation<'a> {
    // Deepest first; the existing parent is not part of the new chain.
    missing: Vec<&'a Path>,
    parent: &'a Path,
}

impl<'a> Creation<'a> {
    fn plan(path: &'a Path) -> Result<Self, CheckpointError> {
        let mut missing = Vec::new();
        let mut current = path;
        loop {
            match fs::metadata(current) {
                Ok(metadata) if metadata.is_dir() => break,
                Ok(_) => return Err(CheckpointError::UnsafePath),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            if missing.len() >= 64 {
                return Err(CheckpointError::OperationLimit("directory depth"));
            }
            missing.push(current);
            current = current.parent().ok_or(CheckpointError::UnsafePath)?;
        }
        Ok(Self {
            missing,
            parent: current,
        })
    }

    fn publish(self, mut sync: impl FnMut(&Path) -> io::Result<()>) -> Result<(), CheckpointError> {
        if self.missing.is_empty() {
            return Ok(());
        }
        for directory in self.missing.iter().rev() {
            match fs::create_dir(directory) {
                Ok(()) => {}
                Err(error)
                    if error.kind() == io::ErrorKind::AlreadyExists && directory.is_dir() => {}
                Err(error) => return Err(error.into()),
            }
        }
        // Persist final child entries before their ancestor linkages. Each
        // directory is synced once, including the existing publication parent.
        // No caller may publish clean checkpoint state until every sync succeeds.
        // On failure, propagate the error and leave created directories in place:
        // another publisher may already use them. Their presence is not a receipt
        // that this failed operation completed its durability fence.
        for directory in self
            .missing
            .iter()
            .copied()
            .chain(std::iter::once(self.parent))
        {
            sync(directory)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
