//! Commits finished child results into the parent conversation before a provider call.
use super::{provider_messages::persist_event, signals::TurnSignal};
use crate::engine::{AgentLoopError, PendingEvent, session::SessionActorConfig};
use rw_types::{Turn, conversation_input::ContextSelection};
use tokio::sync::mpsc;

/// Appends each undelivered child result as a user-role `<child-agent-result>`
/// turn. The durable commit is the delivery record, so every result reaches the
/// model exactly once, in this or a later call.
pub(super) async fn deliver(
    config: &SessionActorConfig,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    conversation: &mut Vec<Turn>,
) -> Result<usize, AgentLoopError> {
    let view = config.history.capture_history().await?;
    let notices = view.completion_notices().await?;
    for notice in notices.iter() {
        persist_event(
            signals,
            PendingEvent::ConversationContextCommitted {
                agent_turn: turn,
                selection: ContextSelection::ChildResult {
                    source: notice.sequence,
                },
            },
        )
        .await?;
        conversation.push(crate::engine::recovery::child_result_turn(
            notice.text.clone(),
        ));
    }
    Ok(notices.len())
}
