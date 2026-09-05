use super::{
    RecoveryError, RecoveryHead,
    encoding::encode,
    projector::{CanonicalRecovery, RecoveryProgress, key, progress},
    state::{
        BOUNDARIES, CONTEXT_ACTIONS, CONVERSATION, ConversationSource, Maintenance, PRUNED_OUTPUTS,
        RewindPhase, SOURCE_ORDINAL,
    },
};
use rw_store::session::{
    journal::JournalReadView,
    recovery_index::{MAX_RECOVERY_HEAD_BYTES, RecoveryMutation, RecoveryReadView, RecoveryRow},
};

impl CanonicalRecovery {
    #[expect(
        clippy::too_many_lines,
        reason = "The two resumable maintenance transitions share one atomic publication."
    )]
    pub(super) fn maintain(
        &mut self,
        source: &JournalReadView,
        read: &RecoveryReadView,
        mut head: RecoveryHead,
    ) -> Result<RecoveryProgress, RecoveryError> {
        let previous = read.head().prefix;
        let mut mutations = Vec::new();
        let pending = head
            .maintenance
            .take()
            .ok_or(RecoveryError::Invalid("missing maintenance state"))?;
        match pending {
            Maintenance::Rewind {
                sequence,
                target,
                phase,
            } => {
                let (namespace, scope, after) = match phase {
                    RewindPhase::Boundaries => (BOUNDARIES, 0, target),
                    RewindPhase::Context => (
                        CONTEXT_ACTIONS,
                        head.conversation.generation,
                        head.context_cut,
                    ),
                    RewindPhase::Prunes => (
                        PRUNED_OUTPUTS,
                        head.conversation.generation,
                        head.context_cut,
                    ),
                };
                let page = read.page(namespace, scope, Some(after), 64, 1024 * 1024)?;
                for row in page.rows {
                    mutations.push(RecoveryMutation::Delete(row.key));
                }
                if page.has_more {
                    head.maintenance = Some(Maintenance::Rewind {
                        sequence,
                        target,
                        phase,
                    });
                } else if let Some(next) = match phase {
                    RewindPhase::Boundaries => Some(RewindPhase::Context),
                    RewindPhase::Context => Some(RewindPhase::Prunes),
                    RewindPhase::Prunes => None,
                } {
                    head.maintenance = Some(Maintenance::Rewind {
                        sequence,
                        target,
                        phase: next,
                    });
                } else {
                    head.next_sequence = sequence
                        .0
                        .checked_add(1)
                        .ok_or(RecoveryError::Invalid("sequence overflow"))?;
                }
            }
            Maintenance::Clear {
                sequence,
                from,
                after,
                mut to,
            } => {
                let page = read.page(CONVERSATION, from.generation, after, 64, 1024 * 1024)?;
                let mut last = after;
                for row in &page.rows {
                    if row.key.ordinal >= from.turns {
                        break;
                    }
                    last = Some(row.key.ordinal);
                    let mut item: ConversationSource = serde_json::from_slice(&row.payload)?;
                    if item.role != rw_types::Role::System {
                        continue;
                    }
                    if item.has_resolved_model {
                        to.resolved_model_source = Some(item.sequence);
                    }
                    to.serialized_bytes = to
                        .serialized_bytes
                        .checked_add(item.serialized_bytes)
                        .ok_or(RecoveryError::Limit("conversation byte counter"))?;
                    to.estimated_tokens = to
                        .estimated_tokens
                        .checked_add(item.estimated_tokens)
                        .ok_or(RecoveryError::Limit("conversation token counter"))?;
                    to.decoded_bytes = to
                        .decoded_bytes
                        .checked_add(item.decoded_bytes)
                        .ok_or(RecoveryError::Limit("clear decoded byte counter"))?;
                    item.cumulative_bytes = to.serialized_bytes;
                    item.cumulative_decoded_bytes = to.decoded_bytes;
                    item.cumulative_tokens = to.estimated_tokens;
                    mutations.push(RecoveryMutation::Put(RecoveryRow {
                        key: key(CONVERSATION, to.generation, to.turns),
                        payload: encode(&item, 64 * 1024)?,
                    }));
                    mutations.push(RecoveryMutation::Put(RecoveryRow {
                        key: key(SOURCE_ORDINAL, to.generation, item.sequence.0),
                        payload: encode(&to.turns, 64 * 1024)?,
                    }));
                    to.turns += 1;
                }
                if last.is_some_and(|ordinal| ordinal.saturating_add(1) >= from.turns)
                    || !page.has_more
                    || from.turns == 0
                {
                    head.conversation = to;
                    if let Some(active) = &mut head.control.active {
                        active.replace_conversation(sequence);
                    }
                    head.context_cut = 0;
                    head.next_sequence = sequence
                        .0
                        .checked_add(1)
                        .ok_or(RecoveryError::Invalid("sequence overflow"))?;
                } else {
                    if last == after {
                        return Err(RecoveryError::Invalid("clear maintenance did not advance"));
                    }
                    head.maintenance = Some(Maintenance::Clear {
                        sequence,
                        from,
                        after: last,
                        to,
                    });
                }
            }
        }
        let cut =
            source.prefix_through(head.next_sequence.checked_sub(1).map(rw_types::SequenceId))?;
        self.index.apply(
            &cut.prove_advance(previous)?,
            &encode(&head, MAX_RECOVERY_HEAD_BYTES)?,
            &mutations,
            &[],
        )?;
        Ok(progress(&head, 0, source))
    }
}
