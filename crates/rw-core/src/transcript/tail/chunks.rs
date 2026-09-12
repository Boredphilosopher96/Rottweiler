use super::super::{TranscriptProjectionError, TranscriptRowLookup};
use rw_store::session::transcript_index::{MAX_AUXILIARY_CELL_BYTES, TranscriptIndexMutation};
use std::io::{self, Write};

/// A single partial cell plus completed cells; no complete-string rewrite or temporary body.
pub(super) struct CellAppender {
    first: u16,
    limit: usize,
    previous_bytes: usize,
    written: usize,
    cell: Vec<u8>,
    mutations: Vec<TranscriptIndexMutation>,
}
impl CellAppender {
    pub(super) fn new(
        first: u16,
        bytes: usize,
        limit: usize,
        rows: &impl TranscriptRowLookup,
    ) -> Result<Self, TranscriptProjectionError> {
        if bytes > limit {
            return Err(TranscriptProjectionError::Invalid("tail byte counter"));
        }
        let partial = bytes % MAX_AUXILIARY_CELL_BYTES;
        let mut cell = Vec::with_capacity(MAX_AUXILIARY_CELL_BYTES);
        if partial != 0 {
            let key = first
                + u16::try_from(bytes / MAX_AUXILIARY_CELL_BYTES)
                    .map_err(|_| TranscriptProjectionError::Invalid("tail slot"))?;
            let previous = rows
                .auxiliary_cell(key)?
                .ok_or(TranscriptProjectionError::Invalid("missing tail cell"))?;
            if previous.len() != partial {
                return Err(TranscriptProjectionError::Invalid("tail cell extent"));
            }
            cell.extend_from_slice(&previous);
        }
        Ok(Self {
            first,
            limit,
            previous_bytes: bytes,
            written: 0,
            cell,
            mutations: Vec::new(),
        })
    }
    fn publish(&mut self) -> io::Result<()> {
        let position =
            (self.previous_bytes + self.written - self.cell.len()) / MAX_AUXILIARY_CELL_BYTES;
        let key = self
            .first
            .checked_add(u16::try_from(position).map_err(io::Error::other)?)
            .ok_or_else(|| io::Error::other("tail slot overflow"))?;
        self.mutations.push(TranscriptIndexMutation::PutAuxiliary {
            key,
            payload: std::mem::replace(
                &mut self.cell,
                Vec::with_capacity(MAX_AUXILIARY_CELL_BYTES),
            ),
        });
        Ok(())
    }
    pub(super) fn finish(
        mut self,
    ) -> Result<(usize, Vec<TranscriptIndexMutation>), TranscriptProjectionError> {
        if self.written > 0 && !self.cell.is_empty() {
            self.publish()
                .map_err(|_| TranscriptProjectionError::Invalid("tail cell publication"))?;
        }
        Ok((self.previous_bytes + self.written, self.mutations))
    }
}
impl Write for CellAppender {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .previous_bytes
            .saturating_add(self.written)
            .saturating_add(bytes.len())
            > self.limit
        {
            return Err(io::Error::other("tail byte limit"));
        }
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let count = remaining
                .len()
                .min(MAX_AUXILIARY_CELL_BYTES - self.cell.len());
            self.cell.extend_from_slice(&remaining[..count]);
            self.written += count;
            remaining = &remaining[count..];
            if self.cell.len() == MAX_AUXILIARY_CELL_BYTES {
                self.publish()?;
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn utf8_prefix(value: &str, limit: usize) -> &str {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}
