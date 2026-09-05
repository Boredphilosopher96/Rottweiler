//! Mode-independent task state indexed from authoritative commits and rewind boundaries.
use rw_store::session::{
    SessionEventPageLimits,
    journal::JournalReadView,
    recovery_index::{
        RecoveryIndex, RecoveryIndexError, RecoveryKey, RecoveryMutation, RecoveryProjection,
        RecoveryReadView, RecoveryRow,
    },
};
use rw_types::{
    EngineEvent, SequenceId, SessionId,
    todo::{TodoReadSnapshot, TodoSnapshot},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const VERSION: u32 = 2;
const BATCH_EVENTS: usize = 64;
const BOUNDARIES: u8 = 1;
#[derive(Clone, Default, Serialize, Deserialize)]
struct Head {
    session: Option<SessionId>,
    next: u64,
    source: Option<SequenceId>,
    rewind: Option<(SequenceId, u64)>,
}

/// No plugin/mode loading and no lifetime event or task snapshot collections.
pub struct TodoProjector {
    index: RecoveryIndex,
}
impl TodoProjector {
    /// Open descriptor-pinned derived task state at a verified journal prefix.
    ///
    /// # Errors
    /// Rejects unsafe, corrupt, foreign, incompatible, or concurrently owned data.
    pub fn open(source: &JournalReadView) -> Result<Self, RecoveryIndexError> {
        Ok(Self {
            index: RecoveryIndex::open(source, RecoveryProjection::Tasks, VERSION)?,
        })
    }
    /// Rebuild only task projection data. Raw journal and accounting remain authoritative.
    ///
    /// # Errors
    /// Rejects unsafe descriptors and concurrent projection ownership.
    pub fn rebuild(source: &JournalReadView) -> Result<Self, RecoveryIndexError> {
        Ok(Self {
            index: RecoveryIndex::rebuild(source, RecoveryProjection::Tasks, VERSION)?,
        })
    }
    /// Apply at most64 source events or64 rewind deletions. True means more work remains.
    ///
    /// # Errors
    /// Rejects malformed source identities, snapshots, and rewind boundaries.
    pub fn advance(&mut self, source: &JournalReadView) -> Result<bool, RecoveryIndexError> {
        let read = self.index.read()?;
        let previous = read.head().prefix;
        let mut head = decode_head(&read)?;
        if head.rewind.is_some() {
            return self.maintain(source, &read, head);
        }
        let page = source.verified_page::<EngineEvent>(
            previous.next_sequence.checked_sub(1).map(SequenceId),
            SessionEventPageLimits {
                max_page_events: BATCH_EVENTS,
                max_page_bytes: SessionEventPageLimits::default().max_line_bytes as u64 + 1,
                ..SessionEventPageLimits::default()
            },
        )?;
        let mut boundaries = BTreeMap::new();
        for envelope in &page.page.events {
            apply(
                &mut head,
                &read,
                &mut boundaries,
                envelope.sequence,
                &envelope.event,
            )?;
            if head.rewind.is_some() {
                break;
            }
        }
        if head.next == previous.next_sequence && head.rewind.is_none() {
            return Ok(false);
        }
        let advance = page
            .proof
            .advance(previous, head.next.checked_sub(1).map(SequenceId))?;
        self.index.apply(
            &advance,
            &encode(&head)?,
            &boundaries.into_values().collect::<Vec<_>>(),
            &[],
        )?;
        Ok(head.rewind.is_some() || head.next < source.prefix_identity().next_sequence)
    }
    fn maintain(
        &mut self,
        source: &JournalReadView,
        read: &RecoveryReadView,
        mut head: Head,
    ) -> Result<bool, RecoveryIndexError> {
        let (sequence, target) = head.rewind.ok_or(invalid("missing task rewind"))?;
        let page = read.page(BOUNDARIES, 0, Some(target), BATCH_EVENTS, 1024 * 1024)?;
        let mutations = page
            .rows
            .into_iter()
            .map(|row| RecoveryMutation::Delete(row.key))
            .collect::<Vec<_>>();
        if !page.has_more {
            head.rewind = None;
            head.next = sequence
                .0
                .checked_add(1)
                .ok_or(invalid("task sequence overflow"))?;
        }
        let cut = source.prefix_through(head.next.checked_sub(1).map(SequenceId))?;
        self.index.apply(
            &cut.prove_advance(read.head().prefix)?,
            &encode(&head)?,
            &mutations,
            &[],
        )?;
        Ok(head.rewind.is_some() || head.next < source.prefix_identity().next_sequence)
    }
    /// Resolve exactly one selected task snapshot; bodies never accumulate in the index.
    ///
    /// # Errors
    /// Rejects incomplete catch-up/rewind, foreign prefixes and corrupt source selectors.
    pub fn snapshot(
        &self,
        source: &JournalReadView,
    ) -> Result<TodoReadSnapshot, RecoveryIndexError> {
        let read = self.index.read()?;
        let head = decode_head(&read)?;
        if head.rewind.is_some() || head.next != source.prefix_identity().next_sequence {
            return Err(invalid("task projection is catching up"));
        }
        let source = read.bind_source(source)?;
        let snapshot = match head.source {
            None => TodoSnapshot::default(),
            Some(sequence) => {
                let page = source.page::<EngineEvent>(
                    sequence.0.checked_sub(1).map(SequenceId),
                    SessionEventPageLimits {
                        max_page_events: 1,
                        max_page_bytes: SessionEventPageLimits::default().max_line_bytes as u64 + 1,
                        ..SessionEventPageLimits::default()
                    },
                )?;
                let envelope = page
                    .events
                    .into_iter()
                    .next()
                    .ok_or(invalid("missing task state source"))?;
                let EngineEvent::TodoStateCommitted { meta, snapshot } = envelope.event else {
                    return Err(invalid("task state source kind"));
                };
                if envelope.sequence != sequence
                    || meta.sequence_id != sequence
                    || head.session.as_ref() != Some(&meta.session_id)
                {
                    return Err(invalid("task state source identity"));
                }
                snapshot.validate().map_err(|_| invalid("task snapshot"))?;
                snapshot
            }
        };
        Ok(TodoReadSnapshot {
            through: head.next.checked_sub(1).map(SequenceId),
            snapshot,
        })
    }
    /// Exact applied physical cursor, including bounded maintenance progress.
    ///
    /// # Errors
    /// Rejects corrupt checkpoint metadata.
    pub fn through(&self) -> Result<Option<SequenceId>, RecoveryIndexError> {
        Ok(self
            .index
            .head()?
            .prefix
            .next_sequence
            .checked_sub(1)
            .map(SequenceId))
    }
}
fn apply(
    head: &mut Head,
    read: &RecoveryReadView,
    changes: &mut BTreeMap<RecoveryKey, RecoveryMutation>,
    sequence: SequenceId,
    event: &EngineEvent,
) -> Result<(), RecoveryIndexError> {
    let meta = event
        .meta()
        .ok_or(invalid("non-durable task projection source"))?;
    if sequence.0 != head.next
        || meta.sequence_id != sequence
        || meta.protocol_version != rw_types::PROTOCOL_VERSION
        || head
            .session
            .as_ref()
            .is_some_and(|session| session != &meta.session_id)
    {
        return Err(invalid("task source identity"));
    }
    head.session = Some(meta.session_id.clone());
    match event {
        EngineEvent::TodoStateCommitted { snapshot, .. } => {
            snapshot.validate().map_err(|_| invalid("task snapshot"))?;
            head.source = Some(sequence);
        }
        EngineEvent::TurnFinished { turn_id, .. } => {
            let key = boundary(
                turn_id
                    .0
                    .parse()
                    .map_err(|_| invalid("task boundary turn"))?,
            );
            changes.insert(
                key,
                RecoveryMutation::Put(RecoveryRow {
                    key,
                    payload: encode(&head.source)?,
                }),
            );
        }
        EngineEvent::ConversationRewound { to_agent_turn, .. } => {
            let target = *to_agent_turn;
            let key = boundary(target);
            let row = match changes.get(&key) {
                Some(RecoveryMutation::Put(row)) => Some(row.clone()),
                Some(RecoveryMutation::Delete(_)) => None,
                None => read.get(key)?,
            }
            .ok_or(invalid("missing task rewind boundary"))?;
            head.source =
                serde_json::from_slice(&row.payload).map_err(|_| invalid("task boundary"))?;
            head.rewind = Some((sequence, target));
            return Ok(());
        }
        _ => {}
    }
    head.next = sequence
        .0
        .checked_add(1)
        .ok_or(invalid("task sequence overflow"))?;
    Ok(())
}
fn decode_head(read: &RecoveryReadView) -> Result<Head, RecoveryIndexError> {
    let head: Head = if read.head().checkpoint.is_empty() {
        Head::default()
    } else {
        serde_json::from_slice(&read.head().checkpoint).map_err(|_| invalid("task checkpoint"))?
    };
    if head.next != read.head().prefix.next_sequence
        || head.source.is_some_and(|source| source.0 >= head.next)
    {
        return Err(invalid("task checkpoint cursor"));
    }
    Ok(head)
}
fn boundary(turn: u64) -> RecoveryKey {
    RecoveryKey {
        namespace: BOUNDARIES,
        scope: 0,
        ordinal: turn,
    }
}
fn encode(value: &impl Serialize) -> Result<Vec<u8>, RecoveryIndexError> {
    serde_json::to_vec(value).map_err(|_| invalid("task projection encoding"))
}
fn invalid(message: &'static str) -> RecoveryIndexError {
    RecoveryIndexError::Invalid(message)
}

#[cfg(test)]
mod tests;
