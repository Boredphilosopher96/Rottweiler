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
        EngineEvent::ConversationInputCommitted {
            agent_turn,
            accepted_source,
            ..
        } => {
            let input = head
                .control
                .accepted
                .iter()
                .find(|input| input.sequence == *accepted_source)
                .ok_or(RecoveryError::Invalid("input is not pending"))?;
            if input.claimed_turn != *agent_turn || input.retained {
                return Err(RecoveryError::Invalid(
                    "input commit must own its active claim",
                ));
            }
            if input.agent_turn != *agent_turn
                && head
                    .control
                    .active
                    .as_ref()
                    .is_none_or(|active| active.turn != *agent_turn)
            {
                return Err(RecoveryError::Invalid(
                    "retained input requires an active turn",
                ));
            }
        }
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
                || selected.role != Role::User
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
        EngineEvent::ConversationTurnCommitted { turn, .. } if turn.role == Role::User => {
            return Err(RecoveryError::Invalid(
                "user conversation requires an explicit input or context source",
            ));
        }
        _ => {}
    }
    Ok(sequence)
}
