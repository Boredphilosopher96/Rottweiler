//! Explicit fixture inputs are converted to durable events before an actor starts.
#![cfg(test)]
use crate::engine::{AgentLoopError, PendingEvent, SessionActorConfig};
use rw_types::{EngineEvent, EventMeta, SequenceId};

pub(super) fn events(
    config: &SessionActorConfig,
    recovered: &crate::engine::SessionRecoveredState,
) -> Result<Vec<EngineEvent>, AgentLoopError> {
    if recovered.last_sequence.is_some() {
        return Err(AgentLoopError::InvalidConfiguration(
            "recovered fixture prefix requires its actual source events".into(),
        ));
    }
    let mut pending = Vec::new();
    if let Some(title) = &recovered.title {
        pending.push(PendingEvent::SessionTitleUpdated {
            title: title.clone(),
            usage: None,
            cost: None,
        });
    }
    if let Some(driver_client_id) = &recovered.driver_client_id {
        pending.push(PendingEvent::SessionCreated {
            driver_client_id: driver_client_id.clone(),
        });
    }
    if let Some(model) = &recovered.model_alias {
        pending.push(PendingEvent::ModelChanged {
            model: rw_types::ModelAlias(model.clone()),
            provider: recovered.provider.clone(),
            thinking: recovered.thinking.unwrap_or(config.thinking),
        });
    }
    if let Some(mode) = &recovered.mode_id {
        let definition = config
            .modes
            .get(&mode.0)
            .ok_or_else(|| AgentLoopError::InvalidConfiguration("fixture mode is absent".into()))?;
        pending.push(PendingEvent::ModeChanged {
            mode: mode.clone(),
            definition_fingerprint: definition.semantic_fingerprint(),
        });
    }
    for turn in &recovered.conversation {
        pending.push(PendingEvent::ConversationTurnCommitted {
            agent_turn: 0,
            turn: turn.clone(),
        });
    }
    for action in &recovered.context_surgery {
        pending.push(if action.pinned {
            PendingEvent::ContextItemPinned {
                item_id: action.item_id.clone(),
                effective_after_agent_turn: action.effective_after_agent_turn,
            }
        } else {
            PendingEvent::ContextItemEvicted {
                item_id: action.item_id.clone(),
                effective_after_agent_turn: action.effective_after_agent_turn,
            }
        });
    }
    for (index, content) in recovered.queued_messages.iter().enumerate() {
        pending.push(PendingEvent::MessageQueued {
            position: recovered
                .queued_message_positions
                .get(index)
                .copied()
                .unwrap_or(index as u64 + 1),
            content: content.clone(),
            attachments: vec![],
        });
    }
    Ok(pending
        .into_iter()
        .enumerate()
        .map(|(sequence, pending)| {
            pending.stamp(EventMeta {
                protocol_version: rw_types::PROTOCOL_VERSION,
                session_id: config.session_id.clone(),
                sequence_id: SequenceId(sequence as u64),
                emitted_at: config.event_clock.emitted_at(),
                caused_by: None,
            })
        })
        .collect())
}
