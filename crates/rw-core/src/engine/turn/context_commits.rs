//! Source selections for user context retained by compaction.
use super::{
    provider_messages::{persist_conversation_turn, persist_event},
    signals::TurnSignal,
};
use crate::engine::{AgentLoopError, PendingEvent, recovery::ConversationSource};
use rw_types::{ContextItemId, Role, SequenceId, Turn, conversation_input::ContextSelection};
use tokio::sync::mpsc;

pub(in crate::engine) struct RetainedUser {
    pub turn: Turn,
    pub source: SequenceId,
}

pub(super) fn selected_source(
    item: &ContextItemId,
    sources: &[ConversationSource],
) -> Result<ConversationSource, AgentLoopError> {
    let sequence = item
        .0
        .strip_prefix("conversation:")
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| invalid("compaction pin source identity"))?;
    sources
        .iter()
        .find(|source| source.sequence.0 == sequence)
        .cloned()
        .ok_or_else(|| invalid("compaction pin is outside the captured source"))
}

pub(super) async fn commit(
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    value: &Turn,
    source: Option<&ConversationSource>,
) -> Result<SequenceId, AgentLoopError> {
    if value.role != Role::User {
        return persist_conversation_turn(signals, turn, value).await;
    }
    let selection = if let Some(source) = source {
        if source.role != Role::User {
            return Err(invalid("retained user role differs from its source"));
        }
        ContextSelection::Retained {
            selected_source: source.sequence,
            body_source: source.body_source,
        }
    } else if value == &rw_context::auto_continue_turn() {
        ContextSelection::Continuation {}
    } else {
        return Err(invalid("retained user context has no source"));
    };
    persist_event(
        signals,
        PendingEvent::ConversationContextCommitted {
            agent_turn: turn,
            selection,
        },
    )
    .await
    .map(|meta| meta.sequence_id)
}
fn invalid(message: &str) -> AgentLoopError {
    AgentLoopError::Persistence(message.into())
}
