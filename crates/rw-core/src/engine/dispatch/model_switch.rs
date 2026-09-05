use crate::engine::AgentLoopError;
use crate::engine::RoutedEvent;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::ActorState;
use crate::engine::session::PreparedModelSwitch;
use crate::engine::session::SessionActorConfig;
use crate::engine::turn::emit_batch;
use rw_types::ModelContextTransfer;
use rw_types::Role;
use std::sync::Arc;
use tokio::sync::broadcast;

pub(in crate::engine) async fn commit_prepared_model_switch(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    events: &broadcast::Sender<RoutedEvent>,
    prepared: PreparedModelSwitch,
    clear_context: bool,
) -> Result<(), AgentLoopError> {
    let mut durable = Vec::with_capacity(if clear_context { 2 } else { 1 });
    if clear_context {
        durable.push(PendingEvent::ModelContextCleared {
            strategy: ModelContextTransfer::StartWithoutContext,
        });
    }
    durable.push(PendingEvent::ModelChanged {
        model: prepared.model.clone(),
        provider: prepared.provider.clone(),
        thinking: prepared.thinking,
    });
    let result = emit_batch(state, events, &config.event_sink, durable)
        .await
        .map(|_| ());
    if result.is_ok() {
        if clear_context {
            state.conversation.retain(|turn| turn.role == Role::System);
            state.context_surgery.clear();
            state.pruned_tool_outputs.clear();
        }
        config.model.commit_prepared_model(&prepared.model.0);
        state.model_alias = prepared.model.0;
        state.provider = prepared.provider;
        state.thinking = prepared.thinking;
    } else {
        config.model.discard_prepared_model(&prepared.model.0);
    }
    result
}
