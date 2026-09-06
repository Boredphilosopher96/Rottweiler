//! A read operation's cumulative source work and conservatively retained metadata.
use super::{CheckpointError, CheckpointOperation, MAX_METADATA_BYTES, Read, validate_metadata};
use crate::checkpoint::CheckpointFileState;
use std::{fs::File, path::Path};

const MAX_READ_ITEMS: usize = 100_000;
const MAX_RETAINED_BYTES: usize = 32 * 1024 * 1024;
const MAX_SOURCE_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct ReadAllowance {
    items: usize,
    retained: usize,
    source: usize,
    max_items: usize,
    max_retained: usize,
    max_source: usize,
}
impl Default for ReadAllowance {
    fn default() -> Self {
        Self {
            items: 0,
            retained: 4096,
            source: 0,
            max_items: MAX_READ_ITEMS,
            max_retained: MAX_RETAINED_BYTES,
            max_source: MAX_SOURCE_BYTES,
        }
    }
}

impl CheckpointOperation {
    pub(in crate::checkpoint) fn retain_read<T>(
        &mut self,
        heap: usize,
    ) -> Result<(), CheckpointError> {
        self.check()?;
        // Four inline slots cover geometric Vec capacity and BTree node spare
        // slots; the initial4KiB covers the first allocation and tree headers.
        let bytes = std::mem::size_of::<T>()
            .saturating_mul(4)
            .saturating_add(128)
            .saturating_add(heap);
        if self.read.items >= self.read.max_items
            || bytes > self.read.max_retained.saturating_sub(self.read.retained)
        {
            return Err(CheckpointError::OperationLimit(
                "retained checkpoint read items or bytes",
            ));
        }
        self.read.items += 1;
        self.read.retained += bytes;
        Ok(())
    }

    pub(in crate::checkpoint) fn retain_state<T>(
        &mut self,
        path_bytes: usize,
        state: &CheckpointFileState,
    ) -> Result<(), CheckpointError> {
        let heap = match state {
            CheckpointFileState::Present { blob, .. } => blob.capacity(),
            CheckpointFileState::Unrestorable { reason } => reason.capacity(),
            CheckpointFileState::Absent => 0,
        };
        self.retain_read::<T>(path_bytes.saturating_add(heap))
    }

    pub(in crate::checkpoint) fn read_metadata(
        &mut self,
        path: &Path,
    ) -> Result<Vec<u8>, CheckpointError> {
        self.path(&path.to_string_lossy())?;
        let mut file = File::open(path)?;
        let length = usize::try_from(file.metadata()?.len())
            .map_err(|_| CheckpointError::OperationLimit("32 MiB metadata"))?;
        if length > MAX_METADATA_BYTES
            || length > self.read.max_source.saturating_sub(self.read.source)
        {
            return Err(CheckpointError::OperationLimit(
                "checkpoint source metadata bytes",
            ));
        }
        self.read.source += length;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)?;
        if file.read(&mut [0; 1])? != 0 {
            return Err(CheckpointError::CaptureChanged);
        }
        self.check()?;
        validate_metadata(&bytes)?;
        Ok(bytes)
    }

    #[cfg(test)]
    pub(in crate::checkpoint) fn read_limits(
        mut self,
        items: usize,
        retained: usize,
        source: usize,
    ) -> Self {
        self.read.max_items = items;
        self.read.max_retained = retained;
        self.read.max_source = source;
        self
    }
}
