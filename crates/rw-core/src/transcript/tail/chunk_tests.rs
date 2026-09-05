#![allow(clippy::expect_used)]
use super::*;
use rw_store::session::transcript_index::{TranscriptIndexError, TranscriptIndexRow};
use std::{cell::Cell, collections::BTreeMap};

#[derive(Default)]
struct Cells {
    data: BTreeMap<u16, Vec<u8>>,
    reads: Cell<usize>,
    bytes_read: Cell<usize>,
}
impl TranscriptRowLookup for Cells {
    fn bound_row(&self, _: &str) -> Result<Option<TranscriptIndexRow>, TranscriptIndexError> {
        Ok(None)
    }
    fn auxiliary_cell(&self, key: u16) -> Result<Option<Vec<u8>>, TranscriptIndexError> {
        self.reads.set(self.reads.get() + 1);
        let value = self.data.get(&key).cloned();
        self.bytes_read
            .set(self.bytes_read.get() + value.as_ref().map_or(0, Vec::len));
        Ok(value)
    }
}
#[test]
fn fragmented_append_reads_only_one_partial_cell_and_never_copies_the_history_prefix() {
    let mut cells = Cells::default();
    let mut extent = 0;
    let fragment = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut written = 0;
    for _ in 0..1500 {
        let reads = cells.reads.get();
        let copied = cells.bytes_read.get();
        let mut writer =
            chunks::CellAppender::new(TEXT_FIRST, extent, TRANSCRIPT_TAIL_TEXT_BYTES, &cells)
                .expect("writer");
        writer.write_all(fragment).expect("append");
        let (next, mutations) = writer.finish().expect("finish");
        assert!(cells.reads.get() - reads <= 1);
        assert!(cells.bytes_read.get() - copied < MAX_AUXILIARY_CELL_BYTES);
        assert!(mutations.len() <= 2);
        for change in mutations {
            let TranscriptIndexMutation::PutAuxiliary { key, payload } = change else {
                panic!("cell");
            };
            written += payload.len();
            cells.data.insert(key, payload);
        }
        extent = next;
    }
    assert_eq!(extent, fragment.len() * 1500);
    assert!(written < MAX_AUXILIARY_CELL_BYTES * 1500 + extent);
    let assembled: Vec<u8> = cells.data.into_values().flatten().collect();
    assert_eq!(assembled, fragment.repeat(1500));
}
