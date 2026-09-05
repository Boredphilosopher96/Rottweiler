//! Workspace discovery reads never authorize a session or interpret a mode policy.
use super::{RecoveryControl, RecoveryError, RecoveryHead};
use rw_store::session::{
    SessionEventPageLimits, journal::JournalReadView, recovery_index::RecoveryIndex,
};
use rw_types::{EngineEvent, SequenceId, WorkspaceRootDescriptor};

/// Committed workspace generation needed to discover the registry for full recovery.
/// This value carries no permission, mode, or execution authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceBootstrap {
    pub generation: u64,
    pub root_count: usize,
    pub scanned_events: usize,
}

impl WorkspaceBootstrap {
    /// Read a validated index checkpoint and its bounded raw suffix.
    ///
    /// # Errors
    /// Rejects unsafe indexes, invalid source identities, or malformed root transitions.
    pub fn read(source: &JournalReadView) -> Result<Self, RecoveryError> {
        let index = RecoveryIndex::open(source, super::projector::VERSION)?;
        let stored = index.head()?;
        let mut head = if stored.checkpoint.is_empty() {
            if stored.prefix.next_sequence != 0 {
                return Err(RecoveryError::Invalid("missing recovery checkpoint"));
            }
            RecoveryHead::new([0; 32])
        } else {
            let head: RecoveryHead = serde_json::from_slice(&stored.checkpoint)?;
            head.validate()?;
            if head.next_sequence != stored.prefix.next_sequence {
                return Err(RecoveryError::Invalid("head/source prefix mismatch"));
            }
            head
        };
        let mut scanned_events = 0_usize;
        while head.next_sequence < source.prefix_identity().next_sequence {
            let page = source.page::<EngineEvent>(
                head.next_sequence.checked_sub(1).map(SequenceId),
                SessionEventPageLimits {
                    max_page_events: 64,
                    max_page_bytes: SessionEventPageLimits::default().max_line_bytes as u64 + 1,
                    ..SessionEventPageLimits::default()
                },
            )?;
            if page.events.is_empty() {
                return Err(RecoveryError::Invalid(
                    "workspace bootstrap made no progress",
                ));
            }
            for envelope in page.events {
                let meta = envelope
                    .event
                    .meta()
                    .ok_or(RecoveryError::Invalid("non-durable event"))?;
                if meta.protocol_version != crate::SESSION_EVENT_VERSION
                    || meta.sequence_id != envelope.sequence
                    || meta.sequence_id.0 != head.next_sequence
                    || head
                        .session_id
                        .as_ref()
                        .is_some_and(|id| id != &meta.session_id)
                {
                    return Err(RecoveryError::Invalid("workspace source identity"));
                }
                head.session_id = Some(meta.session_id.clone());
                if let EngineEvent::WorkspaceRootsChanged {
                    generation, roots, ..
                } = &envelope.event
                {
                    apply_workspace_generation(
                        &mut head.control,
                        envelope.sequence,
                        *generation,
                        roots,
                    )?;
                }
                head.next_sequence = head
                    .next_sequence
                    .checked_add(1)
                    .ok_or(RecoveryError::Invalid("sequence overflow"))?;
                scanned_events = scanned_events
                    .checked_add(1)
                    .ok_or(RecoveryError::Limit("bootstrap event counter"))?;
            }
        }
        Ok(Self {
            generation: head.control.workspace_generation,
            root_count: head.control.workspace_root_count,
            scanned_events,
        })
    }
}

pub(super) fn apply_workspace_generation(
    control: &mut RecoveryControl,
    sequence: SequenceId,
    generation: u64,
    roots: &[WorkspaceRootDescriptor],
) -> Result<(), RecoveryError> {
    if generation != control.workspace_generation.saturating_add(1)
        || roots.is_empty()
        || roots.iter().enumerate().any(|(index, root)| {
            root.index as usize != index
                || root.machine_local
                || root.path != format!("@root/{index}")
        })
        || (control.workspace_root_count > 0 && roots.len() != control.workspace_root_count + 1)
    {
        return Err(RecoveryError::Invalid("workspace generation"));
    }
    let mut hash = blake3::Hasher::new();
    for (index, root) in roots.iter().enumerate() {
        if index == control.workspace_root_count
            && *hash.finalize().as_bytes() != control.workspace_digest
        {
            return Err(RecoveryError::Invalid("workspace prefix changed"));
        }
        let encoded = super::encoding::encode(root, 64 * 1024)?;
        hash.update(&(encoded.len() as u64).to_le_bytes());
        hash.update(&encoded);
    }
    control.workspace_digest = *hash.finalize().as_bytes();
    control.workspace_root_count = roots.len();
    control.workspace_generation = generation;
    control.workspace = Some(sequence);
    Ok(())
}
