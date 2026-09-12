//! Append-only sparse segment metadata and stable prefix lookup.
use super::Segment;
use std::sync::RwLock;

/// Append-only metadata shared by captured prefixes. Payload I/O never holds its lock.
#[derive(Debug, Default)]
pub(super) struct SegmentCatalog {
    entries: RwLock<CatalogEntries>,
}

const CATALOG_CHUNK_ENTRIES: usize = 256;

#[derive(Debug, Default)]
struct CatalogEntries {
    chunks: Vec<Vec<CatalogEntry>>,
    len: usize,
}

impl CatalogEntries {
    fn get(&self, index: usize) -> Option<&CatalogEntry> {
        self.chunks
            .get(index / CATALOG_CHUNK_ENTRIES)?
            .get(index % CATALOG_CHUNK_ENTRIES)
    }

    fn last(&self) -> Option<&CatalogEntry> {
        self.len.checked_sub(1).and_then(|index| self.get(index))
    }

    fn push(&mut self, entry: CatalogEntry) {
        let chunk = self.len / CATALOG_CHUNK_ENTRIES;
        if chunk == self.chunks.len() {
            self.chunks.push(Vec::with_capacity(CATALOG_CHUNK_ENTRIES));
        }
        self.chunks[chunk].push(entry);
        self.len += 1;
    }
}

impl std::ops::Index<usize> for CatalogEntries {
    type Output = CatalogEntry;

    fn index(&self, index: usize) -> &Self::Output {
        &self.chunks[index / CATALOG_CHUNK_ENTRIES][index % CATALOG_CHUNK_ENTRIES]
    }
}

#[derive(Debug)]
struct CatalogEntry {
    segment: Segment,
    cumulative_bytes: u64,
    cumulative_identity: blake3::Hash,
}

impl SegmentCatalog {
    pub(super) fn from_segments(segments: Vec<Segment>) -> Self {
        let catalog = Self::default();
        for segment in segments {
            catalog.push(segment);
        }
        catalog
    }

    pub(super) fn push(&self, segment: Segment) {
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (bytes, identity) = entries.last().map_or((0, blake3::hash(b"")), |entry| {
            (entry.cumulative_bytes, entry.cumulative_identity)
        });
        entries.push(CatalogEntry {
            cumulative_bytes: bytes + segment.bytes,
            cumulative_identity: segment.extend_identity(identity),
            segment,
        });
    }

    pub(super) fn len(&self) -> usize {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len
    }

    pub(super) fn get(&self, index: usize) -> Option<Segment> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(index)
            .map(|entry| entry.segment.clone())
    }

    pub(super) fn prefix(&self, count: usize) -> (u64, blake3::Hash) {
        if count == 0 {
            return (0, blake3::hash(b""));
        }
        let entries = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = &entries[count - 1];
        (entry.cumulative_bytes, entry.cumulative_identity)
    }

    pub(super) fn partition(&self, count: usize, predicate: impl Fn(&Segment) -> bool) -> usize {
        let entries = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut left = 0;
        let mut right = count;
        while left < right {
            let middle = left + (right - left) / 2;
            if predicate(&entries[middle].segment) {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        left
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::{Segment, SegmentCatalog};
    #[test]
    fn catalog_growth_keeps_existing_entries_and_prefix_queries_stable() {
        let catalog = SegmentCatalog::default();
        let segment = |index| Segment {
            first: index,
            next: index + 1,
            bytes: 1,
            digest: blake3::hash(&index.to_le_bytes()),
            name: index.to_string(),
        };
        catalog.push(segment(0));
        let pointer = {
            let entries = catalog.entries.read().expect("catalog");
            std::ptr::from_ref(&entries[0]) as usize
        };
        let first = catalog.prefix(1);
        for index in 1..16_384 {
            catalog.push(segment(index));
        }
        assert_eq!(catalog.prefix(1), first);
        assert_eq!(catalog.prefix(16_384).0, 16_384);
        assert_eq!(catalog.partition(128, |entry| entry.next <= 97), 97);
        assert_eq!(catalog.partition(128, |entry| entry.next <= 20_000), 128);
        let entries = catalog.entries.read().expect("catalog");
        assert_eq!(std::ptr::from_ref(&entries[0]) as usize, pointer);
        assert_eq!(entries.chunks.len(), 64);
    }
}
