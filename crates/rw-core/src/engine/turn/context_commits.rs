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
    pruned: &[(u32, u64)],
) -> Result<SequenceId, AgentLoopError> {
    if !matches!(value.role, Role::User | Role::Tool) {
        return persist_conversation_turn(signals, turn, value).await;
    }
    let selection = if let Some(source) = source {
        if source.role != value.role {
            return Err(invalid(
                "retained source-owned role differs from its source",
            ));
        }
        ContextSelection::Retained {
            selected_source: source.sequence,
            body_source: source.body_source,
        }
    } else if value == &rw_context::auto_continue_turn() {
        ContextSelection::Continuation {}
    } else {
        return Err(invalid("retained source-owned context has no source"));
    };
    let sequence = persist_event(
        signals,
        PendingEvent::ConversationContextCommitted {
            agent_turn: turn,
            selection,
        },
    )
    .await
    .map(|meta| meta.sequence_id)?;
    for &(block_index, reclaimed_tokens) in pruned {
        persist_event(
            signals,
            PendingEvent::ToolOutputPruned {
                source: rw_types::ContextBlockId {
                    sequence,
                    block_index,
                },
                reclaimed_tokens,
            },
        )
        .await?;
    }
    Ok(sequence)
}

pub(super) fn retained_pruning(
    value: &Turn,
    source: SequenceId,
    pruned: &std::collections::BTreeMap<String, u64>,
) -> Vec<(u32, u64)> {
    value
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            if !matches!(block, rw_types::Block::ToolResult { .. }) {
                return None;
            }
            let index32 = u32::try_from(index).ok()?;
            pruned
                .get(&super::context::block_key(source, index))
                .map(|tokens| (index32, *tokens))
        })
        .collect()
}
fn invalid(message: &str) -> AgentLoopError {
    AgentLoopError::Persistence(message.into())
}
