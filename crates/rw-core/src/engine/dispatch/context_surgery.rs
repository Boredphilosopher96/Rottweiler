use crate::engine::AgentLoopError;
use crate::engine::RoutedEvent;
use crate::engine::durability::SessionEventSink;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::ActorState;
use crate::engine::session::SessionActorConfig;
use crate::engine::turn::emit;
use rw_types::ContextItemId;
use std::sync::Arc;
use tokio::sync::broadcast;

pub(super) async fn apply_context_surgery(
    state: &mut ActorState,
    events: &broadcast::Sender<RoutedEvent>,
    sink: &Arc<dyn SessionEventSink>,
    item_id: ContextItemId,
    pinned: bool,
) -> Result<(), AgentLoopError> {
    let effective_after_agent_turn = state.next_turn;
    let pending = if pinned {
        PendingEvent::ContextItemPinned {
            item_id: item_id.clone(),
            effective_after_agent_turn,
        }
    } else {
        PendingEvent::ContextItemEvicted {
            item_id: item_id.clone(),
            effective_after_agent_turn,
        }
    };
    emit(state, events, sink, pending).await.map(|_| ())?;
    Ok(())
}

pub(super) async fn apply_registered_context_surgery(
    state: &mut ActorState,
    config: &SessionActorConfig,
    events: &broadcast::Sender<RoutedEvent>,
    item_id: ContextItemId,
    pinned: bool,
) -> Result<(), AgentLoopError> {
    if !item_id.0.starts_with("conversation:") {
        return Err(AgentLoopError::InvalidConfiguration(
            "protected_context_item: only conversation-resident context items support pin or eviction"
                .to_owned(),
        ));
    }
    let source = item_id
        .0
        .strip_prefix("conversation:")
        .and_then(|value| value.parse::<u64>().ok())
        .map(rw_types::SequenceId);
    let view = crate::engine::turn::history_context::capture(
        config,
        state.sequence.map(rw_types::SequenceId),
    )
    .await?;
    let known = if let Some(source) = source {
        rw_types::context_source::conversation_item(source) == item_id
            && view.source_turn(source).await?.is_some()
    } else {
        false
    };
    if !known {
        return Err(AgentLoopError::InvalidConfiguration(
            "unknown_context_item: context item is not present in the current inventory".to_owned(),
        ));
    }
    apply_context_surgery(state, events, &config.event_sink, item_id, pinned).await
}
