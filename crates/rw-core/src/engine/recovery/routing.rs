//! Mode-independent physical workspace routes and effective completed boundaries.
//! The selected prefix is subsequently validated by canonical recovery with its exact registry.
use super::{
    RecoveryError,
    encoding::encode,
    projector::{BatchRows, key},
};
use rw_store::session::{
    SessionEventPageLimits,
    journal::JournalReadView,
    recovery_index::{
        MAX_RECOVERY_HEAD_BYTES, RecoveryIndex, RecoveryProjection, RecoveryReadView,
    },
};
use rw_types::{EngineEvent, SequenceId, SessionId};
use serde::{Deserialize, Serialize};
const VERSION: u32 = 1;
const BOUNDARIES: u8 = 1;
const WORKSPACES: u8 = 2;
const PAGE: usize = 32;
#[derive(Default, Serialize, Deserialize)]
struct Head {
    session: Option<SessionId>,
    next: u64,
    workspace_generation: u64,
    rewind: Option<(SequenceId, u64)>,
}

/// Source-qualified scalar routing indexes; no conversation, attachments or mode policy.
pub struct SessionRoutingIndex {
    index: RecoveryIndex,
}
impl SessionRoutingIndex {
    /// # Errors
    /// Rejects unsafe, inconsistent or concurrently owned derived storage.
    pub fn open(source: &JournalReadView) -> Result<Self, RecoveryError> {
        Ok(Self {
            index: RecoveryIndex::open(source, RecoveryProjection::Routing, VERSION)?,
        })
    }
    /// Apply one bounded raw page or one bounded boundary-removal transaction.
    /// # Errors
    /// Rejects invalid source identity, route generations or boundary transitions.
    pub fn advance(&mut self, source: &JournalReadView) -> Result<bool, RecoveryError> {
        let read = self.index.read()?;
        let mut head = decode_head(&read)?;
        if head.rewind.is_some() {
            return self.maintain(source, read, head);
        }
        let previous = read.head().prefix;
        if previous == source.prefix_identity() {
            return Ok(false);
        }
        let page = source.verified_page::<EngineEvent>(
            head.next.checked_sub(1).map(SequenceId),
            SessionEventPageLimits {
                max_page_events: PAGE,
                max_page_bytes: SessionEventPageLimits::default().max_line_bytes as u64 + 1,
                ..SessionEventPageLimits::default()
            },
        )?;
        let mut rows = BatchRows::new(read);
        for envelope in &page.page.events {
            apply(&mut head, &mut rows, envelope.sequence, &envelope.event)?;
            if head.rewind.is_some() {
                break;
            }
        }
        let through = head.next.checked_sub(1).map(SequenceId);
        let proof = page.proof.advance(previous, through)?;
        self.index.apply(
            &proof,
            &encode(&head, MAX_RECOVERY_HEAD_BYTES)?,
            &rows.changes.into_values().collect::<Vec<_>>(),
            &[],
        )?;
        Ok(head.rewind.is_some() || head.next < source.prefix_identity().next_sequence)
    }
    fn maintain(
        &mut self,
        source: &JournalReadView,
        read: RecoveryReadView,
        mut head: Head,
    ) -> Result<bool, RecoveryError> {
        let previous = read.head().prefix;
        let (sequence, turn) = head
            .rewind
            .ok_or(RecoveryError::Invalid("routing rewind state"))?;
        let page = read.page(BOUNDARIES, 0, Some(turn), PAGE, 64 * 1024)?;
        let mut rows = BatchRows::new(read);
        for row in page.rows {
            rows.delete(row.key);
        }
        if !page.has_more {
            head.rewind = None;
            head.next = sequence
                .0
                .checked_add(1)
                .ok_or(RecoveryError::Invalid("routing sequence overflow"))?;
        }
        let cut = source.prefix_through(head.next.checked_sub(1).map(SequenceId))?;
        self.index.apply(
            &cut.prove_advance(previous)?,
            &encode(&head, MAX_RECOVERY_HEAD_BYTES)?,
            &rows.changes.into_values().collect::<Vec<_>>(),
            &[],
        )?;
        Ok(head.rewind.is_some() || head.next < source.prefix_identity().next_sequence)
    }
    fn read(&self, source: &JournalReadView) -> Result<RecoveryReadView, RecoveryError> {
        let read = self.index.read()?;
        let head = decode_head(&read)?;
        if head.rewind.is_some() || read.head().prefix != source.prefix_identity() {
            return Err(RecoveryError::Maintenance);
        }
        Ok(read)
    }
    /// Select only a currently effective completed turn; a reused turn has a new source.
    /// # Errors
    /// Rejects unfinished catch-up and unavailable boundaries.
    pub fn completed(
        &self,
        source: &JournalReadView,
        turn: u64,
    ) -> Result<Option<SequenceId>, RecoveryError> {
        let read = self.read(source)?;
        if turn == 0 {
            return Ok(None);
        }
        let row = read
            .get(key(BOUNDARIES, 0, turn))?
            .ok_or(RecoveryError::Invalid(
                "fork turn is not an effective completed boundary",
            ))?;
        Ok(Some(serde_json::from_slice(&row.payload)?))
    }
    /// Read physical workspace generation at one exact historical source cut.
    /// # Errors
    /// Rejects future/unqualified prefixes and malformed route metadata.
    pub fn workspace_at(
        &self,
        source: &JournalReadView,
        through: Option<SequenceId>,
    ) -> Result<u64, RecoveryError> {
        let read = self.read(source)?;
        let prefix = source.prefix_through(through)?;
        let row = read.last_before(WORKSPACES, 0, prefix.prefix_identity().next_sequence)?;
        row.map_or(Ok(0), |row| {
            serde_json::from_slice(&row.payload).map_err(RecoveryError::from)
        })
    }
}
fn decode_head(read: &RecoveryReadView) -> Result<Head, RecoveryError> {
    let head: Head = if read.head().checkpoint.is_empty() {
        Head::default()
    } else {
        serde_json::from_slice(&read.head().checkpoint)?
    };
    if head.next != read.head().prefix.next_sequence {
        return Err(RecoveryError::Invalid("routing source prefix"));
    }
    Ok(head)
}
fn apply(
    head: &mut Head,
    rows: &mut BatchRows,
    sequence: SequenceId,
    event: &EngineEvent,
) -> Result<(), RecoveryError> {
    let meta = event
        .meta()
        .ok_or(RecoveryError::Invalid("routing durable metadata"))?;
    if meta.protocol_version != rw_types::PROTOCOL_VERSION
        || meta.sequence_id != sequence
        || sequence.0 != head.next
        || head
            .session
            .as_ref()
            .is_some_and(|session| session != &meta.session_id)
    {
        return Err(RecoveryError::Invalid("routing source identity"));
    }
    head.session.get_or_insert_with(|| meta.session_id.clone());
    match event {
        EngineEvent::WorkspaceRootsChanged { generation, .. } => {
            if head.workspace_generation.checked_add(1) != Some(*generation) {
                return Err(RecoveryError::Invalid("routing workspace generation"));
            }
            head.workspace_generation = *generation;
            rows.put(key(WORKSPACES, 0, sequence.0), generation)?;
        }
        EngineEvent::TurnFinished { turn_id, .. } => {
            let turn = turn_id
                .0
                .parse::<u64>()
                .map_err(|_| RecoveryError::Invalid("routing turn identity"))?;
            rows.put(key(BOUNDARIES, 0, turn), &sequence)?;
        }
        EngineEvent::ConversationRewound { to_agent_turn, .. } => {
            if *to_agent_turn != 0
                && rows
                    .get::<SequenceId>(key(BOUNDARIES, 0, *to_agent_turn))?
                    .is_none()
            {
                return Err(RecoveryError::Invalid("routing rewind boundary"));
            }
            head.rewind = Some((sequence, *to_agent_turn));
            return Ok(());
        }
        _ => {}
    }
    head.next = sequence
        .0
        .checked_add(1)
        .ok_or(RecoveryError::Invalid("routing sequence overflow"))?;
    Ok(())
}

#[cfg(test)]
mod tests;
