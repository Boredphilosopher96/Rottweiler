use super::{
    TranscriptEventProjection, TranscriptProjectionError, TranscriptProjectionState,
    TranscriptRowLookup, project_transcript_event,
};
use rw_store::session::transcript_index::{
    MAX_BATCH_BYTES, MAX_BATCH_ROWS, TranscriptIndex, TranscriptIndexError,
    TranscriptIndexMutation, TranscriptIndexRow,
};
use rw_store::session::{SessionEventPageLimits, journal::JournalReadView};
use rw_types::transcript::TRANSCRIPT_PROJECTION_VERSION;
use rw_types::{EngineEvent, SequenceId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const EVENTS_PER_BATCH: usize = 64;
const REPAIR_ROWS_PER_BATCH: usize = 16;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Checkpoint {
    state: TranscriptProjectionState,
    rewind: Option<PendingRewind>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct PendingRewind {
    sequence: SequenceId,
    target: u64,
    first_removed: Option<u64>,
    phase: RewindPhase,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
enum RewindPhase {
    Delete,
    Repack { read_from: u64, write_to: u64 },
}

/// One bounded catch-up transaction, suitable for cancellable runtime scheduling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TranscriptProjectionProgress {
    pub applied_next_sequence: u64,
    pub interpreted_events: usize,
    pub rebuilding: bool,
    pub has_more: bool,
}

/// The sole semantic writer of a session's rebuildable transcript index.
pub struct TranscriptProjector {
    index: TranscriptIndex,
}

impl TranscriptProjector {
    /// Open an existing projection or initialize an empty derived index.
    ///
    /// # Errors
    /// Fails for corrupt/incompatible projections, unsafe storage or another owner.
    pub fn open(view: &JournalReadView) -> Result<Self, TranscriptProjectionError> {
        let projector = Self {
            index: TranscriptIndex::open(view, TRANSCRIPT_PROJECTION_VERSION)?,
        };
        projector.checkpoint()?;
        Ok(projector)
    }

    /// Explicitly discard incompatible derived state, leaving canonical data intact.
    ///
    /// # Errors
    /// Fails if another owner holds the index or storage cannot be reset safely.
    pub fn rebuild(view: &JournalReadView) -> Result<Self, TranscriptProjectionError> {
        Ok(Self {
            index: TranscriptIndex::rebuild(view, TRANSCRIPT_PROJECTION_VERSION)?,
        })
    }

    /// Read the published projection through the bounded index API.
    #[must_use]
    pub fn index(&self) -> &TranscriptIndex {
        &self.index
    }

    /// Read the bounded live-preview checkpoint at the published semantic prefix.
    ///
    /// # Errors
    /// Rejects incomplete rewinds or corrupt checkpoint metadata.
    pub fn tail_state(&self) -> Result<super::TailState, TranscriptProjectionError> {
        Self::tail_for(&self.index)
    }
    pub(super) fn tail_for(
        index: &TranscriptIndex,
    ) -> Result<super::TailState, TranscriptProjectionError> {
        let checkpoint = Self::checkpoint_for(index)?;
        if checkpoint.rewind.is_some() {
            return Err(TranscriptIndexError::Rebuilding.into());
        }
        Ok(checkpoint.state.tail)
    }

    /// Interpret one bounded raw page or one bounded hidden rewind transaction.
    /// The caller may cancel between calls; persisted checkpoints resume exactly.
    ///
    /// # Errors
    /// Rejects invalid canonical data, changed prefixes, corruption and storage limits.
    pub fn advance(
        &mut self,
        view: &JournalReadView,
    ) -> Result<TranscriptProjectionProgress, TranscriptProjectionError> {
        let mut checkpoint = self.checkpoint()?;
        if checkpoint.rewind.is_some() {
            return self.advance_rewind(view, checkpoint);
        }
        let before = self.index.head()?;
        let limits = projection_page_limits();
        let after = checkpoint
            .state
            .next_sequence
            .checked_sub(1)
            .map(SequenceId);
        let verified = view
            .verified_page::<EngineEvent>(after, limits)
            .map_err(TranscriptIndexError::from)?;
        let page = &verified.page;
        let mut overlay = BatchRows {
            index: &self.index,
            rows: BTreeMap::new(),
            bindings: BTreeMap::new(),
            cells: BTreeMap::new(),
            mutations: Vec::with_capacity(EVENTS_PER_BATCH * 2),
        };
        let mut interpreted = 0;
        let mut charged_bytes = 0;
        for envelope in &page.events {
            if envelope
                .event
                .meta()
                .is_none_or(|meta| meta.sequence_id != envelope.sequence)
            {
                return Err(TranscriptProjectionError::Invalid(
                    "envelope/event sequence mismatch",
                ));
            }
            match project_transcript_event(&envelope.event, &checkpoint.state, &overlay)? {
                TranscriptEventProjection::Update {
                    state,
                    mutations: changes,
                } => {
                    let change_bytes = changes
                        .iter()
                        .map(TranscriptIndexMutation::charged_bytes)
                        .sum::<usize>();
                    if overlay.mutations.len() + changes.len() > MAX_BATCH_ROWS
                        || charged_bytes + change_bytes > MAX_BATCH_BYTES
                    {
                        if interpreted == 0 {
                            return Err(TranscriptProjectionError::Invalid(
                                "event exceeds projection batch",
                            ));
                        }
                        break;
                    }
                    charged_bytes += change_bytes;
                    for change in changes {
                        overlay.apply(change);
                    }
                    checkpoint.state = state;
                    interpreted += 1;
                }
                TranscriptEventProjection::Rewind {
                    target_turn,
                    sequence,
                } => {
                    if interpreted > 0 {
                        break;
                    }
                    checkpoint.state.session_id =
                        envelope.event.meta().map(|meta| meta.session_id.clone());
                    return self.begin_rewind(view, checkpoint, target_turn, sequence);
                }
            }
        }
        let mutations = overlay.mutations;
        if interpreted > 0 {
            let applied_cursor = checkpoint
                .state
                .next_sequence
                .checked_sub(1)
                .map(SequenceId);
            let advance = verified
                .proof
                .advance(before.prefix, applied_cursor)
                .map_err(TranscriptIndexError::from)?;
            self.index.apply(
                &advance,
                before.generation,
                &serde_json::to_vec(&checkpoint)?,
                false,
                &mutations,
            )?;
        } else {
            verified
                .proof
                .verify_prefix(before.prefix)
                .map_err(TranscriptIndexError::from)?;
        }
        self.progress(view, interpreted)
    }

    fn begin_rewind(
        &mut self,
        view: &JournalReadView,
        mut checkpoint: Checkpoint,
        target: u64,
        sequence: SequenceId,
    ) -> Result<TranscriptProjectionProgress, TranscriptProjectionError> {
        let before = self.index.head()?;
        checkpoint.rewind = Some(PendingRewind {
            sequence,
            target,
            first_removed: None,
            phase: RewindPhase::Delete,
        });
        let generation = before
            .generation
            .checked_add(1)
            .ok_or(TranscriptProjectionError::Invalid("generation overflow"))?;
        self.index.apply(
            &view
                .at_prefix(before.prefix)
                .and_then(|prefix| prefix.prove_advance(before.prefix))
                .map_err(TranscriptIndexError::from)?,
            generation,
            &serde_json::to_vec(&checkpoint)?,
            true,
            &[],
        )?;
        self.progress(view, 0)
    }

    fn checkpoint(&self) -> Result<Checkpoint, TranscriptProjectionError> {
        Self::checkpoint_for(&self.index)
    }
    fn checkpoint_for(index: &TranscriptIndex) -> Result<Checkpoint, TranscriptProjectionError> {
        let head = index.head()?;
        let checkpoint: Checkpoint = if head.state.is_empty() {
            Checkpoint::default()
        } else {
            serde_json::from_slice(&head.state)?
        };
        checkpoint
            .state
            .tail
            .validate(checkpoint.state.next_sequence)?;
        if checkpoint.state.next_sequence != head.prefix.next_sequence
            || checkpoint.rewind.is_some() != head.rebuilding
            || (!head.rebuilding && checkpoint.state.next_ordinal != head.total_rows)
        {
            return Err(TranscriptProjectionError::Invalid(
                "semantic checkpoint watermark",
            ));
        }
        Ok(checkpoint)
    }

    fn progress(
        &self,
        view: &JournalReadView,
        interpreted_events: usize,
    ) -> Result<TranscriptProjectionProgress, TranscriptProjectionError> {
        let head = self.index.head()?;
        Ok(TranscriptProjectionProgress {
            applied_next_sequence: head.prefix.next_sequence,
            interpreted_events,
            rebuilding: head.rebuilding,
            has_more: head.rebuilding
                || head.prefix.next_sequence < view.prefix_identity().next_sequence,
        })
    }

    fn advance_rewind(
        &mut self,
        view: &JournalReadView,
        mut checkpoint: Checkpoint,
    ) -> Result<TranscriptProjectionProgress, TranscriptProjectionError> {
        let mut pending = checkpoint
            .rewind
            .take()
            .ok_or(TranscriptProjectionError::Invalid(
                "missing rewind checkpoint",
            ))?;
        let before = self.index.head()?;
        let mut mutations = Vec::with_capacity(REPAIR_ROWS_PER_BATCH);
        let complete = match pending.phase {
            RewindPhase::Delete => {
                let rows = self
                    .index
                    .rows_after_turn(pending.target, REPAIR_ROWS_PER_BATCH)?;
                for row in &rows {
                    pending.first_removed = Some(
                        pending
                            .first_removed
                            .map_or(row.ordinal, |first| first.min(row.ordinal)),
                    );
                    mutations.push(TranscriptIndexMutation::Delete(row.key.clone()));
                }
                if rows.is_empty() {
                    if let Some(first) = pending.first_removed {
                        pending.phase = RewindPhase::Repack {
                            read_from: first,
                            write_to: first,
                        };
                        false
                    } else {
                        true
                    }
                } else {
                    false
                }
            }
            RewindPhase::Repack {
                read_from,
                mut write_to,
            } => {
                let page = self.index.maintenance_page(
                    read_from,
                    REPAIR_ROWS_PER_BATCH,
                    rw_store::session::transcript_index::MAX_PAGE_BYTES,
                )?;
                let mut next_read = read_from;
                for row in &page.rows {
                    if row.ordinal < write_to {
                        return Err(TranscriptProjectionError::Invalid("rewind ordinal order"));
                    }
                    if row.ordinal != write_to {
                        mutations.push(TranscriptIndexMutation::Move {
                            key: row.key.clone(),
                            ordinal: write_to,
                        });
                    }
                    next_read = row
                        .ordinal
                        .checked_add(1)
                        .ok_or(TranscriptProjectionError::Invalid("ordinal overflow"))?;
                    write_to = write_to
                        .checked_add(1)
                        .ok_or(TranscriptProjectionError::Invalid("ordinal overflow"))?;
                }
                pending.phase = RewindPhase::Repack {
                    read_from: next_read,
                    write_to,
                };
                page.rows.is_empty()
            }
        };
        let applied = if complete {
            checkpoint.state.next_sequence = pending
                .sequence
                .0
                .checked_add(1)
                .ok_or(TranscriptProjectionError::Invalid("sequence overflow"))?;
            checkpoint.state.next_ordinal = before.total_rows;
            checkpoint.state.active_turn = Some(pending.target);
            checkpoint.state.tail.reset(pending.sequence.0);
            view.prefix_through(Some(pending.sequence))
                .map_err(TranscriptIndexError::from)?
        } else {
            checkpoint.rewind = Some(pending);
            view.at_prefix(before.prefix)
                .map_err(TranscriptIndexError::from)?
        };
        self.index.apply(
            &applied
                .prove_advance(before.prefix)
                .map_err(TranscriptIndexError::from)?,
            before.generation,
            &serde_json::to_vec(&checkpoint)?,
            !complete,
            &mutations,
        )?;
        self.progress(view, usize::from(complete))
    }
}

struct BatchRows<'a> {
    cells: BTreeMap<u16, usize>,
    mutations: Vec<TranscriptIndexMutation>,
    index: &'a TranscriptIndex,
    rows: BTreeMap<String, usize>,
    bindings: BTreeMap<String, String>,
}
impl BatchRows<'_> {
    fn apply(&mut self, change: TranscriptIndexMutation) {
        if let TranscriptIndexMutation::PutAuxiliary { key, .. } = &change {
            if let Some(position) = self.cells.get(key) {
                self.mutations[*position] = change;
                return;
            }
        }
        let position = self.mutations.len();
        match &change {
            TranscriptIndexMutation::Put(row) => {
                self.rows.insert(row.key.clone(), position);
            }
            TranscriptIndexMutation::Bind { binding, key } => {
                self.bindings.insert(binding.clone(), key.clone());
            }
            TranscriptIndexMutation::PutAuxiliary { key, .. } => {
                self.cells.insert(*key, position);
            }
            TranscriptIndexMutation::Delete(_) | TranscriptIndexMutation::Move { .. } => {}
        }
        self.mutations.push(change);
    }
    fn row(&self, key: &str) -> Option<TranscriptIndexRow> {
        let position = self.rows.get(key)?;
        match &self.mutations[*position] {
            TranscriptIndexMutation::Put(row) => Some(row.clone()),
            _ => None,
        }
    }
}
impl TranscriptRowLookup for BatchRows<'_> {
    fn auxiliary_cell(&self, key: u16) -> Result<Option<Vec<u8>>, TranscriptIndexError> {
        let Some(position) = self.cells.get(&key) else {
            return self.index.auxiliary_cell(key);
        };
        match &self.mutations[*position] {
            TranscriptIndexMutation::PutAuxiliary { payload, .. } => Ok(Some(payload.clone())),
            _ => Err(TranscriptIndexError::Invalid("auxiliary overlay identity")),
        }
    }
    fn bound_row(&self, binding: &str) -> Result<Option<TranscriptIndexRow>, TranscriptIndexError> {
        if let Some(key) = self.bindings.get(binding) {
            return Ok(self.row(key));
        }
        let prior = self.index.bound_row(binding)?;
        Ok(prior.map(|row| self.row(&row.key).unwrap_or(row)))
    }
}

fn projection_page_limits() -> SessionEventPageLimits {
    let limits = SessionEventPageLimits::default();
    SessionEventPageLimits {
        max_page_events: EVENTS_PER_BATCH,
        max_page_bytes: limits.max_line_bytes as u64 + 1,
        max_scan_bytes: limits.max_line_bytes as u64 * 2,
        ..limits
    }
}
