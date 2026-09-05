//! Page-owned evidence for advancing a derived projection without rereading payloads.

use super::{
    JournalPrefixIdentity, JournalReadMetrics, JournalReadView, Segment, SessionEventPage,
    SessionEventPageLimits, SessionStoreError,
};
use rw_types::SequenceId;
use serde::de::DeserializeOwned;
use std::{fs::File, sync::Arc};

const MAX_PROOF_SEGMENTS: usize = 8;
const MAX_PROOF_EVENTS: usize = 256;

/// A bounded raw page and the exact prefix cuts validated while reading it.
pub struct VerifiedJournalPage<T> {
    /// Decoded cursor-exclusive events; may stop at proof resource bounds.
    pub page: SessionEventPage<T>,
    /// Evidence for the initial cursor and each returned event cursor.
    pub proof: JournalPageProof,
    /// Actual payload I/O and decoded records.
    pub metrics: JournalReadMetrics,
}

/// An immutable, descriptor-bound prefix transition validated by the journal owner.
#[derive(Clone, Debug)]
pub struct JournalAdvance {
    previous: JournalPrefixIdentity,
    next: JournalReadView,
}

impl JournalAdvance {
    /// Previously published prefix that the consumer must still own.
    #[must_use]
    pub const fn previous(&self) -> JournalPrefixIdentity {
        self.previous
    }
    /// Validated target prefix, including its pinned directory and boundary descriptor.
    #[must_use]
    pub fn next(&self) -> &JournalReadView {
        &self.next
    }
}

/// Opaque, bounded evidence computed from one page's already validated bytes.
#[derive(Debug)]
pub struct JournalPageProof {
    origin: JournalReadView,
    segments: Vec<ProofSegment>,
    cuts: Vec<PrefixCut>,
}

#[derive(Debug)]
struct ProofSegment {
    catalog_index: usize,
    segment: Segment,
    file: Arc<File>,
}

#[derive(Debug)]
struct PrefixCut {
    next: u64,
    boundary: Option<usize>,
    catalog_count: usize,
    bytes: u64,
    digest: blake3::Hash,
}

impl JournalPageProof {
    /// Reconstitutes any cursor represented by this page without reading payloads.
    ///
    /// # Errors
    /// Rejects cursors outside the initial cut and returned events, or overflow.
    pub fn prefix_through(
        &self,
        cursor: Option<SequenceId>,
    ) -> Result<JournalReadView, SessionStoreError> {
        let next = cursor
            .map(|cursor| {
                cursor
                    .0
                    .checked_add(1)
                    .ok_or(SessionStoreError::SequenceOverflow)
            })
            .transpose()?
            .unwrap_or(0);
        let index = self
            .cuts
            .binary_search_by_key(&next, |cut| cut.next)
            .map_err(|_| SessionStoreError::EventPageCursorAhead)?;
        let cut = &self.cuts[index];
        if cut.next == self.origin.next_sequence {
            return Ok(self.origin.clone());
        }
        let (active, active_segment, total_bytes, digest) = if let Some(index) = cut.boundary {
            let boundary = &self.segments[index];
            let mut segment = boundary.segment.clone();
            segment.next = next;
            segment.bytes = cut.bytes;
            segment.digest = cut.digest;
            let (prior_bytes, prior) = self.origin.segments.prefix(boundary.catalog_index);
            let digest = segment.extend_identity(prior);
            (
                Arc::clone(&boundary.file),
                Some(segment),
                prior_bytes + cut.bytes,
                digest,
            )
        } else {
            (Arc::clone(&self.origin.active), None, cut.bytes, cut.digest)
        };
        Ok(JournalReadView {
            directory: Arc::clone(&self.origin.directory),
            segments: Arc::clone(&self.origin.segments),
            segment_count: cut.catalog_count,
            active,
            active_segment,
            next_sequence: next,
            total_bytes,
            identity: JournalPrefixIdentity {
                next_sequence: next,
                digest: *digest.as_bytes(),
            },
        })
    }

    /// Checks an expected content-bound prefix represented by the page.
    ///
    /// # Errors
    /// Rejects unrepresented or mismatched identities without reading payloads.
    pub fn verify_prefix(&self, identity: JournalPrefixIdentity) -> Result<(), SessionStoreError> {
        let cursor = identity.next_sequence.checked_sub(1).map(SequenceId);
        if self.prefix_through(cursor)?.identity != identity {
            return Err(SessionStoreError::CorruptEvent(
                "journal page prefix identity mismatch",
            ));
        }
        Ok(())
    }

    /// Validates a monotonic transition between two represented prefix cuts.
    ///
    /// # Errors
    /// Rejects mismatched, backwards or unrepresented transitions.
    pub fn advance(
        &self,
        previous: JournalPrefixIdentity,
        through: Option<SequenceId>,
    ) -> Result<JournalAdvance, SessionStoreError> {
        self.verify_prefix(previous)?;
        let next = self.prefix_through(through)?;
        if next.next_sequence < previous.next_sequence {
            return Err(SessionStoreError::EventPageCursorAhead);
        }
        Ok(JournalAdvance { previous, next })
    }
}

pub(super) struct ProofBuilder {
    proof: JournalPageProof,
    first: u64,
    hash: blake3::Hasher,
    bytes: u64,
}

impl ProofBuilder {
    fn new(origin: JournalReadView, first: u64) -> Self {
        let mut cuts = Vec::new();
        if first == 0 {
            cuts.push(PrefixCut {
                next: 0,
                boundary: None,
                catalog_count: 0,
                bytes: 0,
                digest: blake3::hash(b""),
            });
        } else if first == origin.next_sequence {
            // The existing view is already a validated capture; no payload read is needed.
            let (count, bytes, digest) = (
                origin.segment_count,
                origin.total_bytes,
                blake3::Hash::from(origin.identity.digest),
            );
            cuts.push(PrefixCut {
                next: first,
                boundary: None,
                catalog_count: count,
                bytes,
                digest,
            });
        }
        Self {
            proof: JournalPageProof {
                origin,
                segments: Vec::new(),
                cuts,
            },
            first,
            hash: blake3::Hasher::new(),
            bytes: 0,
        }
    }

    pub(super) fn can_read_segment(&self) -> bool {
        self.proof.segments.len() < MAX_PROOF_SEGMENTS
    }

    pub(super) fn begin_segment(
        &mut self,
        catalog_index: usize,
        segment: &Segment,
        file: Arc<File>,
    ) {
        if self.proof.cuts.is_empty() && self.first == segment.first {
            let (bytes, digest) = self.proof.origin.segments.prefix(catalog_index);
            self.proof.cuts.push(PrefixCut {
                next: self.first,
                boundary: None,
                catalog_count: catalog_index,
                bytes,
                digest,
            });
        }
        self.hash = blake3::Hasher::new();
        self.bytes = 0;
        self.proof.segments.push(ProofSegment {
            catalog_index,
            segment: segment.clone(),
            file,
        });
    }

    pub(super) fn line(&mut self, sequence: u64, line: &[u8], returned: bool) {
        self.hash.update(line);
        self.bytes += line.len() as u64;
        if returned || (self.proof.cuts.is_empty() && sequence + 1 == self.first) {
            let boundary = self.proof.segments.len() - 1;
            self.proof.cuts.push(PrefixCut {
                next: sequence + 1,
                boundary: Some(boundary),
                catalog_count: self.proof.segments[boundary].catalog_index,
                bytes: self.bytes,
                digest: self.hash.finalize(),
            });
        }
    }
}

impl JournalReadView {
    /// Validates an earlier prefix before authorizing a derived index transition.
    /// Use page proof advancement when the transition comes from a just-read page.
    ///
    /// # Errors
    /// Rejects mismatched or future prefixes and corrupt boundary data.
    pub fn prove_advance(
        &self,
        previous: JournalPrefixIdentity,
    ) -> Result<JournalAdvance, SessionStoreError> {
        if previous != self.identity {
            self.at_prefix(previous)?;
        }
        Ok(JournalAdvance {
            previous,
            next: self.clone(),
        })
    }

    /// Reads one page and computes prefix evidence in the same payload pass.
    /// At most 256 events and eight segment descriptors belong to one proof.
    ///
    /// # Errors
    /// Has the same integrity, cursor and allocation errors as ordinary paging.
    pub fn verified_page<T: DeserializeOwned>(
        &self,
        after: Option<SequenceId>,
        mut limits: SessionEventPageLimits,
    ) -> Result<VerifiedJournalPage<T>, SessionStoreError> {
        let first = after
            .map(|cursor| {
                cursor
                    .0
                    .checked_add(1)
                    .ok_or(SessionStoreError::SequenceOverflow)
            })
            .transpose()?
            .unwrap_or(0);
        let mut proof = ProofBuilder::new(self.clone(), first);
        limits.max_page_events = limits.max_page_events.min(MAX_PROOF_EVENTS);
        let (page, metrics) = self.page_internal(after, limits, Some(&mut proof))?;
        Ok(VerifiedJournalPage {
            page,
            metrics,
            proof: proof.proof,
        })
    }
}
