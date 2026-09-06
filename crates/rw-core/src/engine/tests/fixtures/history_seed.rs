//! Explicit fixture inputs are converted to durable events before an actor starts.
#![cfg(test)]
use crate::engine::{AgentLoopError, PendingEvent, SessionActorConfig};
use rw_types::{Block, EngineEvent, EventMeta, Role, SequenceId, TurnMeta};
use std::collections::BTreeMap;

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
    let sources = append_conversation(&mut pending, &recovered.conversation)?;
    for action in &recovered.context_surgery {
        let item_id = if let Some(sequence) = action.item_id.0.strip_prefix("conversation:") {
            let original = sequence.parse::<u64>().map_err(|_| {
                AgentLoopError::InvalidConfiguration("fixture context source".into())
            })?;
            let source = sources.get(&original).ok_or_else(|| {
                AgentLoopError::InvalidConfiguration("fixture context source is absent".into())
            })?;
            rw_types::ContextItemId(format!("conversation:{source}"))
        } else {
            action.item_id.clone()
        };
        pending.push(if action.pinned {
            PendingEvent::ContextItemPinned {
                item_id: item_id.clone(),
                effective_after_agent_turn: action.effective_after_agent_turn,
            }
        } else {
            PendingEvent::ContextItemEvicted {
                item_id: item_id.clone(),
                effective_after_agent_turn: action.effective_after_agent_turn,
            }
        });
    }
    for (key, reclaimed_tokens) in &recovered.pruned_tool_outputs {
        let (sequence, block_index) = key
            .split_once(':')
            .ok_or_else(|| AgentLoopError::InvalidConfiguration("fixture block source".into()))?;
        let source = rw_types::ContextBlockId {
            sequence: SequenceId(
                *sources
                    .get(&sequence.parse::<u64>().map_err(|_| {
                        AgentLoopError::InvalidConfiguration("fixture sequence".into())
                    })?)
                    .ok_or_else(|| {
                        AgentLoopError::InvalidConfiguration(
                            "fixture prune source is absent".into(),
                        )
                    })?,
            ),
            block_index: block_index
                .parse()
                .map_err(|_| AgentLoopError::InvalidConfiguration("fixture block index".into()))?,
        };
        pending.push(PendingEvent::ToolOutputPruned {
            source,
            reclaimed_tokens: *reclaimed_tokens,
        });
    }
    append_queue(&mut pending, recovered);
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

fn append_conversation(
    pending: &mut Vec<PendingEvent>,
    conversation: &[rw_types::Turn],
) -> Result<BTreeMap<u64, u64>, AgentLoopError> {
    let original_start = pending.len();
    let mut sources = BTreeMap::new();
    for (ordinal, turn) in conversation.iter().enumerate() {
        if turn.role == Role::User {
            let [Block::Text { text }] = turn.blocks.as_slice() else {
                return Err(AgentLoopError::InvalidConfiguration(
                    "user fixture requires explicit accepted attachments".into(),
                ));
            };
            if turn.meta != TurnMeta::default() {
                return Err(AgentLoopError::InvalidConfiguration(
                    "accepted fixture input cannot carry provider metadata".into(),
                ));
            }
            let accepted_source = SequenceId(pending.len() as u64);
            pending.push(PendingEvent::UserMessageAccepted {
                turn: 0,
                content: text.clone(),
                attachments: vec![],
            });
            sources.insert((original_start + ordinal) as u64, pending.len() as u64);
            pending.push(PendingEvent::ConversationInputCommitted {
                agent_turn: 0,
                accepted_source,
                selection: rw_types::conversation_input::InputSelection::Accepted {},
            });
        } else {
            sources.insert((original_start + ordinal) as u64, pending.len() as u64);
            pending.push(PendingEvent::ConversationTurnCommitted {
                agent_turn: 0,
                turn: turn.clone(),
            });
        }
    }
    Ok(sources)
}

fn append_queue(pending: &mut Vec<PendingEvent>, recovered: &crate::engine::SessionRecoveredState) {
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
}
