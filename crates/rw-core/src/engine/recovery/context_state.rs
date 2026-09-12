//! Latest context mutations are indexed by generation and exact item identity.
use super::{
    CanonicalHistory, RecoveryError,
    projector::{BatchRows, key},
    state::CONTEXT_ACTIONS,
};
use crate::engine::projection::ContextSurgeryAction;
use rw_store::session::recovery_index::{RecoveryKey, RecoveryRow};
use rw_types::{ContextItemId, SequenceId};

const IDENTITIES: u8 = 2;
const REVISIONS: u8 = 13;
/// Source item, state, effective turn and immutable identity-index scope.
type Mutation = (ContextItemId, bool, u64, u64);

fn identity(generation: u64, item: &ContextItemId) -> Result<Vec<u8>, RecoveryError> {
    rw_types::extension_control::validate_context_item_id(&item.0)
        .map_err(RecoveryError::Invalid)?;
    let mut key = Vec::with_capacity(8 + item.0.len());
    key.extend_from_slice(&generation.to_be_bytes());
    key.extend_from_slice(item.0.as_bytes());
    Ok(key)
}

pub(super) fn apply(
    rows: &mut BatchRows,
    generation: u64,
    sequence: SequenceId,
    item: &ContextItemId,
    pinned: bool,
    effective: u64,
) -> Result<(), RecoveryError> {
    let identity = identity(generation, item)?;
    let scope = if let Some(scope) = rows.lookup::<u64>(IDENTITIES, &identity)? {
        scope
    } else {
        rows.put_lookup(IDENTITIES, identity, &sequence.0)?;
        sequence.0
    };
    let mutation = (item, pinned, effective, scope);
    rows.put(key(CONTEXT_ACTIONS, generation, sequence.0), &mutation)?;
    rows.put(key(REVISIONS, scope, sequence.0), &mutation)
}

/// Delete the exact revision together with its ordered maintenance entry. Identity
/// bindings survive; an empty revision range means this item has no effective action.
pub(super) fn revision_key(row: &RecoveryRow) -> Result<RecoveryKey, RecoveryError> {
    let (_, _, _, scope): Mutation = serde_json::from_slice(&row.payload)?;
    Ok(key(REVISIONS, scope, row.key.ordinal))
}

impl CanonicalHistory {
    /// Resolve one effective pin/eviction with bounded index seeks. No historical
    /// action vector or unrelated message body is read.
    /// # Errors
    /// Rejects malformed identity/revision metadata and storage failures.
    pub fn context_action(
        &self,
        item: &ContextItemId,
    ) -> Result<Option<ContextSurgeryAction>, RecoveryError> {
        let identity = identity(self.head.conversation.generation, item)?;
        let Some(bytes) = self.read.lookup(IDENTITIES, &identity)? else {
            return Ok(None);
        };
        let scope: u64 = serde_json::from_slice(&bytes)?;
        let Some(row) = self
            .read
            .last_before(REVISIONS, scope, self.head.next_sequence)?
        else {
            return Ok(None);
        };
        let (stored, pinned, effective_after_agent_turn, indexed_scope): Mutation =
            serde_json::from_slice(&row.payload)?;
        if &stored != item || indexed_scope != scope {
            return Err(RecoveryError::Invalid("context item revision identity"));
        }
        Ok(Some(ContextSurgeryAction {
            item_id: stored,
            pinned,
            effective_after_agent_turn,
        }))
    }
}
