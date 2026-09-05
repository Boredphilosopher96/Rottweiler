//! Source-derived child lifecycle and artifact authority, independent of runtime modes.
mod read;
mod reduce;
use super::{
    RecoveryError,
    encoding::encode,
    projector::{BatchRows, key},
};
pub use read::SubagentLifecycleView;
use rw_store::session::{
    SessionEventPageLimits,
    journal::JournalReadView,
    recovery_index::{
        MAX_RECOVERY_HEAD_BYTES, RecoveryIndex, RecoveryProjection, RecoveryReadView,
    },
};
use rw_types::{EngineEvent, SequenceId, SessionId, SubagentId};
use serde::{Deserialize, Serialize};

const VERSION: u32 = 3;
const PAGE: usize = 16;
const IDENTITIES: u8 = 1;
const RAW_SPAWNS: u8 = 2;
const ARTIFACT_IDENTITIES: u8 = 3;
const STATES: u8 = 1;
const VERSIONS: u8 = 2;
const PENDING: u8 = 3;
const TURN_KEYS: u8 = 4;
const TURN_EVENTS: u8 = 5;
const ARTIFACTS: u8 = 6;
const BOUNDARIES: u8 = 7;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Head {
    session: Option<SessionId>,
    next: u64,
    active_turn: Option<u64>,
    rewind: Option<(SequenceId, u64)>,
}

/// Exact effective lifecycle for a child identity. Large results remain source selectors.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentBinding {
    pub subagent_id: SubagentId,
    pub session_id: SessionId,
    pub spawned: SequenceId,
    pub spawned_turn: u64,
    pub task_preview: String,
    pub task_truncated: bool,
    pub terminal: Option<SequenceId>,
    pub latest_result: Option<SequenceId>,
    pub latest_artifact: Option<String>,
    scope: u64,
    revision: SequenceId,
    artifact_scope: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize)]
struct ArtifactIdentity {
    scope: u64,
    digest: [u8; 32],
}

/// Bounded writer of lifecycle selectors. Rewind removes only discarded logical
/// turns; physical publication evidence remains queryable for private cleanup.
pub struct SubagentLifecycleIndex {
    index: RecoveryIndex,
}
impl SubagentLifecycleIndex {
    /// # Errors
    /// Rejects unsafe, incompatible, foreign or concurrently owned derived storage.
    pub fn open(source: &JournalReadView) -> Result<Self, RecoveryError> {
        Ok(Self {
            index: RecoveryIndex::open(source, RecoveryProjection::Subagents, VERSION)?,
        })
    }
    /// # Errors
    /// Rebuilds only derived child selectors; raw source and accounting are unchanged.
    pub fn rebuild(source: &JournalReadView) -> Result<Self, RecoveryError> {
        Ok(Self {
            index: RecoveryIndex::rebuild(source, RecoveryProjection::Subagents, VERSION)?,
        })
    }
    /// Apply at most16 events or16 discarded lifecycle mutations. True means more work remains.
    /// # Errors
    /// Rejects inconsistent lifecycle identities or source metadata.
    pub fn advance(&mut self, source: &JournalReadView) -> Result<bool, RecoveryError> {
        let read = self.index.read()?;
        let mut head = decode_head(&read)?;
        if head.rewind.is_some() {
            return self.maintain(source, read, head);
        }
        let previous = read.head().prefix;
        let page = source.verified_page::<EngineEvent>(
            previous.next_sequence.checked_sub(1).map(SequenceId),
            SessionEventPageLimits {
                max_page_events: PAGE,
                max_page_bytes: SessionEventPageLimits::default().max_line_bytes as u64 + 1,
                ..SessionEventPageLimits::default()
            },
        )?;
        let mut rows = BatchRows::new(read);
        for envelope in &page.page.events {
            let mut next = head.clone();
            rows.begin_event();
            reduce::apply(&mut next, &mut rows, envelope.sequence, &envelope.event)?;
            if !rows.fits(encode(&next, MAX_RECOVERY_HEAD_BYTES)?.len()) {
                rows.rollback_event();
                if head.next == previous.next_sequence {
                    return Err(RecoveryError::Limit("child lifecycle metadata batch"));
                }
                break;
            }
            rows.commit_event();
            head = next;
            if head.rewind.is_some() {
                break;
            }
        }
        if head.next == previous.next_sequence && head.rewind.is_none() {
            return Ok(false);
        }
        let proof = page
            .proof
            .advance(previous, head.next.checked_sub(1).map(SequenceId))?;
        self.index.apply(
            &proof,
            &encode(&head, MAX_RECOVERY_HEAD_BYTES)?,
            &rows.changes.into_values().collect::<Vec<_>>(),
            &rows.lookups.into_values().collect::<Vec<_>>(),
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
        let (sequence, target) = head
            .rewind
            .ok_or(RecoveryError::Invalid("missing child rewind"))?;
        let turn = read
            .page(TURN_KEYS, 0, Some(target), 1, 1024)?
            .rows
            .into_iter()
            .next();
        let mut rows = BatchRows::new(read);
        if let Some(turn) = turn {
            let page = rows
                .read
                .page(TURN_EVENTS, turn.key.ordinal, None, PAGE, 1024 * 1024)?;
            for row in &page.rows {
                let scope: u64 = serde_json::from_slice(&row.payload)?;
                let revision: SubagentBinding = rows
                    .get(key(VERSIONS, scope, row.key.ordinal))?
                    .ok_or(RecoveryError::Invalid("child revision missing"))?;
                rows.delete(key(VERSIONS, scope, row.key.ordinal));
                rows.delete(row.key);
                if let Some(artifact) = revision.artifact_scope {
                    rows.delete(key(ARTIFACTS, artifact, row.key.ordinal));
                }
                let current: Option<SubagentBinding> = rows.get(key(STATES, 0, scope))?;
                if current
                    .as_ref()
                    .is_some_and(|current| current.revision == revision.revision)
                {
                    if let Some(current) = current {
                        rows.delete(key(PENDING, 0, current.spawned.0));
                    }
                    let restored = rows.last_before(VERSIONS, scope, head.next)?;
                    if let Some(restored) = restored {
                        let restored: SubagentBinding = serde_json::from_slice(&restored.payload)?;
                        reduce::publish(&mut rows, &restored)?;
                    } else {
                        rows.delete(key(STATES, 0, scope));
                    }
                }
            }
            if !page.has_more {
                rows.delete(turn.key);
            }
        } else {
            let boundaries = rows
                .read
                .page(BOUNDARIES, 0, Some(target), PAGE, 1024 * 1024)?;
            for row in &boundaries.rows {
                rows.delete(row.key);
            }
            if !boundaries.has_more {
                head.rewind = None;
                head.next = sequence
                    .0
                    .checked_add(1)
                    .ok_or(RecoveryError::Invalid("child sequence overflow"))?;
            }
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
    /// Capture source-qualified reads. Maintenance must complete before publication.
    /// # Errors
    /// Rejects a mismatched source or incomplete rewind.
    pub fn snapshot(
        &self,
        source: &JournalReadView,
    ) -> Result<SubagentLifecycleView, RecoveryError> {
        let read = self.index.read()?;
        let head = decode_head(&read)?;
        if head.rewind.is_some() {
            return Err(RecoveryError::Maintenance);
        }
        let source = source.prefix_through(head.next.checked_sub(1).map(SequenceId))?;
        if source.prefix_identity() != read.head().prefix {
            return Err(RecoveryError::Invalid("child source prefix"));
        }
        Ok(SubagentLifecycleView { read, source, head })
    }
}
fn decode_head(read: &RecoveryReadView) -> Result<Head, RecoveryError> {
    let head = if read.head().checkpoint.is_empty() {
        Head::default()
    } else {
        serde_json::from_slice(&read.head().checkpoint)?
    };
    if head.next != read.head().prefix.next_sequence {
        return Err(RecoveryError::Invalid("child projection prefix"));
    }
    Ok(head)
}
fn identity(id: &SubagentId) -> Result<&[u8], RecoveryError> {
    if id.0.is_empty() || id.0.len() > 128 || id.0.chars().any(char::is_control) {
        return Err(RecoveryError::Invalid("child identity"));
    }
    Ok(id.0.as_bytes())
}
fn raw_identity(id: &SubagentId, session: &SessionId) -> Result<Vec<u8>, RecoveryError> {
    SessionId::validate(&session.0)
        .map_err(|_| RecoveryError::Invalid("child session identity"))?;
    let mut result = identity(id)?.to_vec();
    result.push(0);
    result.extend_from_slice(session.0.as_bytes());
    Ok(result)
}

#[cfg(test)]
mod tests;
