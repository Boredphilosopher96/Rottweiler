//! Validate explicit user-context authority before resolving any body.
use super::{
    ConversationSource, RecoveryError, RecoveryHead,
    projector::{BatchRows, key},
    state::{CONVERSATION, SOURCE_ORDINAL},
};
use rw_types::{EngineEvent, Role, SequenceId, conversation_input::ContextSelection};

pub(super) fn validate(
    head: &RecoveryHead,
    event: &EngineEvent,
    rows: &BatchRows,
) -> Result<SequenceId, RecoveryError> {
    let sequence = event
        .meta()
        .ok_or(RecoveryError::Invalid("context metadata"))?
        .sequence_id;
    match event {
        EngineEvent::ConversationContextCommitted {
            selection:
                ContextSelection::Retained {
                    selected_source,
                    body_source,
                },
            ..
        } => {
            if head.compacting.is_none() {
                return Err(RecoveryError::Invalid(
                    "retained context requires an active compaction",
                ));
            }
            let generation = head.conversation.generation;
            let ordinal: u64 = rows
                .get(key(SOURCE_ORDINAL, generation, selected_source.0))?
                .ok_or(RecoveryError::Invalid("retained context is not effective"))?;
            let selected: ConversationSource = rows
                .get(key(CONVERSATION, generation, ordinal))?
                .ok_or(RecoveryError::Invalid("retained context row"))?;
            if ordinal >= head.conversation.turns
                || selected.sequence != *selected_source
                || selected.body_source != *body_source
                || !matches!(selected.role, Role::User | Role::Tool)
            {
                return Err(RecoveryError::Invalid("retained context source identity"));
            }
            return Ok(*body_source);
        }
        EngineEvent::ConversationContextCommitted {
            selection: ContextSelection::Continuation {},
            ..
        } if head.compacting.is_none() => {
            return Err(RecoveryError::Invalid(
                "continuation context requires an active compaction",
            ));
        }
        EngineEvent::ConversationTurnCommitted { turn, .. }
            if matches!(turn.role, Role::User | Role::Tool) =>
        {
            return Err(RecoveryError::Invalid(
                "user/tool conversation requires an explicit source",
            ));
        }
        _ => {}
    }
    Ok(sequence)
}
