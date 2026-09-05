//! Fixed-slot opaque cells share the projection prefix transaction.
use super::{TranscriptIndex, TranscriptIndexError, storage};
use redb::{ReadableDatabase as _, ReadableTable as _, TableDefinition};

/// Cells are overwritten in place across logical epochs; keys cannot accumulate.
pub const MAX_AUXILIARY_CELLS: u16 = 2048;
/// One chunk can be copied or updated without materializing a complete preview.
pub const MAX_AUXILIARY_CELL_BYTES: usize = 4096;
pub(super) const CELLS: TableDefinition<u16, &[u8]> = TableDefinition::new("transcript_cells");

impl TranscriptIndex {
    /// Read one bounded cell for an incremental append.
    ///
    /// # Errors
    /// Rejects a key outside the fixed slot space, corruption, or storage failure.
    pub fn auxiliary_cell(&self, key: u16) -> Result<Option<Vec<u8>>, TranscriptIndexError> {
        validate(key, &[])?;
        let transaction = self.database.begin_read().map_err(storage)?;
        let table = transaction.open_table(CELLS).map_err(storage)?;
        let value = table.get(key).map_err(storage)?;
        value
            .map(|value| {
                validate(key, value.value())?;
                Ok(value.value().to_vec())
            })
            .transpose()
    }

    /// Read a contiguous bounded range through one index read transaction.
    ///
    /// # Errors
    /// Rejects missing or corrupt cells and ranges exceeding the read byte budget.
    pub fn auxiliary_range(
        &self,
        first: u16,
        count: u16,
        max_bytes: usize,
    ) -> Result<Vec<u8>, TranscriptIndexError> {
        let end = first
            .checked_add(count)
            .filter(|end| *end <= MAX_AUXILIARY_CELLS)
            .ok_or(TranscriptIndexError::Limit("auxiliary range"))?;
        if max_bytes > super::MAX_PAGE_BYTES {
            return Err(TranscriptIndexError::Limit("auxiliary read bytes"));
        }
        let transaction = self.database.begin_read().map_err(storage)?;
        let table = transaction.open_table(CELLS).map_err(storage)?;
        let mut result = Vec::new();
        let capacity = usize::from(count)
            .saturating_mul(MAX_AUXILIARY_CELL_BYTES)
            .min(max_bytes);
        result
            .try_reserve_exact(capacity)
            .map_err(|_| TranscriptIndexError::Limit("auxiliary allocation"))?;
        for key in first..end {
            let value = table
                .get(key)
                .map_err(storage)?
                .ok_or(TranscriptIndexError::Invalid("missing auxiliary cell"))?;
            validate(key, value.value())?;
            if result.len().saturating_add(value.value().len()) > max_bytes {
                return Err(TranscriptIndexError::Limit("auxiliary read bytes"));
            }
            result.extend_from_slice(value.value());
        }
        Ok(result)
    }
}

pub(super) fn validate(key: u16, value: &[u8]) -> Result<(), TranscriptIndexError> {
    if key >= MAX_AUXILIARY_CELLS || value.len() > MAX_AUXILIARY_CELL_BYTES {
        return Err(TranscriptIndexError::Limit("auxiliary cell"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{journal::SegmentedJournal, transcript_index::TranscriptIndexMutation};

    #[test]
    #[allow(clippy::expect_used)]
    fn fixed_cells_replace_across_epochs_and_rejections_leave_prefix_and_bytes_unchanged() {
        let root = tempfile::tempdir().expect("root");
        let mut journal = SegmentedJournal::open(root.path(), "cells").expect("journal");
        journal.append_batch([1]).expect("source");
        let view = journal.read_view();
        let mut index = TranscriptIndex::open(&view, 1).expect("index");
        for epoch in 0..16_u8 {
            let head = index.head().expect("head");
            let advance = view.prove_advance(head.prefix).expect("advance");
            index
                .apply(
                    &advance,
                    0,
                    &[epoch],
                    false,
                    &[
                        TranscriptIndexMutation::PutAuxiliary {
                            key: 0,
                            payload: vec![epoch; MAX_AUXILIARY_CELL_BYTES],
                        },
                        TranscriptIndexMutation::PutAuxiliary {
                            key: 1,
                            payload: vec![epoch; 17],
                        },
                    ],
                )
                .expect("replace fixed cells");
        }
        assert_eq!(
            index.auxiliary_range(0, 2, 8192).expect("range"),
            vec![15; MAX_AUXILIARY_CELL_BYTES + 17]
        );
        assert!(
            index
                .auxiliary_range(0, 2, MAX_AUXILIARY_CELL_BYTES)
                .is_err()
        );
        assert!(index.auxiliary_range(2, 1, 4096).is_err());
        let head = index.head().expect("head");
        let advance = view.prove_advance(head.prefix).expect("advance");
        let mut overallocated = Vec::with_capacity(MAX_AUXILIARY_CELL_BYTES + 1);
        overallocated.push(1);
        for invalid in [
            TranscriptIndexMutation::PutAuxiliary {
                key: 0,
                payload: overallocated,
            },
            TranscriptIndexMutation::PutAuxiliary {
                key: MAX_AUXILIARY_CELLS,
                payload: vec![0],
            },
            TranscriptIndexMutation::PutAuxiliary {
                key: 0,
                payload: vec![0; MAX_AUXILIARY_CELL_BYTES + 1],
            },
        ] {
            assert!(
                index
                    .apply(&advance, 0, b"rejected", false, &[invalid])
                    .is_err()
            );
            assert_eq!(index.head().expect("head"), head);
            assert_eq!(
                index.auxiliary_cell(0).expect("cell"),
                Some(vec![15; MAX_AUXILIARY_CELL_BYTES])
            );
        }
        let transaction = index.database.begin_read().expect("read");
        let cells = transaction.open_table(CELLS).expect("cells");
        use redb::ReadableTableMetadata as _;
        assert_eq!(cells.len().expect("cell count"), 2);
    }
}
