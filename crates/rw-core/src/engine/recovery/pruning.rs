//! Effective output pruning is selected with bounded identity and revision seeks.
use super::{
    CanonicalHistory, RecoveryError,
    projector::{BatchRows, key},
    state::PRUNED_OUTPUTS,
};
use rw_store::session::recovery_index::{RecoveryKey, RecoveryRow};
use rw_types::{ContextBlockId, SequenceId};

const IDENTITIES: u8 = 3;
const REVISIONS: u8 = 14;
/// Source item, state, effective turn and immutable identity-index scope.
type Mutation = (ContextBlockId, u64, u64);

fn identity(generation: u64, item: ContextBlockId) -> Vec<u8> {
    let mut key = Vec::with_capacity(20);
    key.extend_from_slice(&generation.to_be_bytes());
    key.extend_from_slice(&item.sequence.0.to_be_bytes());
    key.extend_from_slice(&item.block_index.to_be_bytes());
    key
}

pub(super) fn apply(
    rows: &mut BatchRows,
    generation: u64,
    sequence: SequenceId,
    item: ContextBlockId,
    reclaimed_tokens: u64,
) -> Result<(), RecoveryError> {
    let identity = identity(generation, item);
    let scope = if let Some(scope) = rows.lookup::<u64>(IDENTITIES, &identity)? {
        scope
    } else {
        rows.put_lookup(IDENTITIES, identity, &sequence.0)?;
        sequence.0
    };
    let mutation = (item, reclaimed_tokens, scope);
    rows.put(key(PRUNED_OUTPUTS, generation, sequence.0), &mutation)?;
    rows.put(key(REVISIONS, scope, sequence.0), &mutation)
}

/// Delete the exact revision together with its ordered maintenance entry. Identity
/// bindings survive; an empty revision range means this item has no effective action.
pub(super) fn revision_key(row: &RecoveryRow) -> Result<RecoveryKey, RecoveryError> {
    let (_, _, scope): Mutation = serde_json::from_slice(&row.payload)?;
    Ok(key(REVISIONS, scope, row.key.ordinal))
}

impl CanonicalHistory {
    /// Resolve one effective output pruning entry without reading unrelated history.
    /// # Errors
    /// Rejects invalid identities, inconsistent revisions and storage failures.
    pub fn pruned_output(&self, item: ContextBlockId) -> Result<Option<u64>, RecoveryError> {
        let identity = identity(self.head.conversation.generation, item);
        let Some(bytes) = self.read.lookup(IDENTITIES, &identity)? else {
            return Ok(None);
        };
        let scope: u64 = serde_json::from_slice(&bytes)?;
        let Some(row) =
            self.read
                .last_before(REVISIONS, scope, self.head.context_cut.saturating_add(1))?
        else {
            return Ok(None);
        };
        let (stored, tokens, indexed_scope): Mutation = serde_json::from_slice(&row.payload)?;
        if stored != item || indexed_scope != scope {
            return Err(RecoveryError::Invalid("pruned output revision identity"));
        }
        Ok(Some(tokens))
    }
}
