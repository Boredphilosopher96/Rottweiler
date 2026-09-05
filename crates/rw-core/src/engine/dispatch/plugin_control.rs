//! Machine controls enter the actor without creating or impersonating a driver.
use crate::engine::session::{ActorState, SessionActorConfig};
use crate::engine::turn::{assemble_session_context, protocol_context_kind};
use crate::engine::{AgentLoopError, RoutedEvent, apply_mode_change, mode_permission_base};
use rw_types::extension_control::{
    ExtensionContextItem, ExtensionContextPage, ExtensionContextRead, ExtensionControl,
    ExtensionControlOutcome, MAX_CONTEXT_PAGE_ITEMS, validate_name,
};
use rw_types::{ContextItemId, ContextItemState, Role, SequenceId, SessionMode};
use std::sync::Arc;
use tokio::sync::broadcast;

pub(super) fn read_context(
    state: &ActorState,
    config: &SessionActorConfig,
    request: ExtensionContextRead,
) -> Result<ExtensionContextPage, AgentLoopError> {
    let sequence = state.sequence.map(SequenceId);
    if request.after_item_id.is_some() && request.expected_sequence != sequence {
        return Ok(ExtensionContextPage::Restart {});
    }
    let assembled = assemble_session_context(
        config,
        &state.conversation,
        &state.queued,
        &state.context_surgery,
        &state.pruned_tool_outputs,
        false,
    )?;
    let start = if let Some(id) = &request.after_item_id {
        validate_name(&id.0).map_err(invalid)?;
        match assembled.items.iter().position(|item| item.id.0 == id.0) {
            Some(index) => index + 1,
            None => return Ok(ExtensionContextPage::Restart {}),
        }
    } else {
        0
    };
    let end = start
        .saturating_add(MAX_CONTEXT_PAGE_ITEMS)
        .min(assembled.items.len());
    let items = assembled.items[start..end]
        .iter()
        .map(|item| {
            validate_name(&item.id.0).map_err(invalid)?;
            let role = item
                .assembled_turn_index
                .and_then(|index| assembled.turns.get(index))
                .map(|turn| &turn.role);
            Ok(ExtensionContextItem {
                item_id: ContextItemId(item.id.0.clone()),
                kind: protocol_context_kind(item.kind, role),
                source: match &item.provenance {
                    rw_context::ContextProvenance::BuiltIn => {
                        rw_types::extension_control::ExtensionContextSource::BuiltIn
                    }
                    rw_context::ContextProvenance::ProjectFile { .. } => {
                        rw_types::extension_control::ExtensionContextSource::ProjectFile
                    }
                    rw_context::ContextProvenance::Extension { .. } => {
                        rw_types::extension_control::ExtensionContextSource::Extension
                    }
                    rw_context::ContextProvenance::Conversation { .. } => {
                        rw_types::extension_control::ExtensionContextSource::Conversation
                    }
                    rw_context::ContextProvenance::UserPin => {
                        rw_types::extension_control::ExtensionContextSource::UserPin
                    }
                    rw_context::ContextProvenance::ClientQueue => {
                        rw_types::extension_control::ExtensionContextSource::ClientQueue
                    }
                },
                estimated_tokens: item.tokens,
                state: ContextItemState {
                    pinned: item.pinned,
                    evicted: item.evicted,
                    summarized: item.summarized,
                    pruned: item.pruned,
                },
            })
        })
        .collect::<Result<Vec<_>, AgentLoopError>>()?;
    let next_after_item_id = (end < assembled.items.len())
        .then(|| items.last().map(|item| item.item_id.clone()))
        .flatten();
    Ok(ExtensionContextPage::Ready {
        sequence,
        items,
        next_after_item_id,
    })
}

pub(super) async fn control(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    events: &broadcast::Sender<RoutedEvent>,
    control: ExtensionControl,
) -> Result<ExtensionControlOutcome, AgentLoopError> {
    control.validate().map_err(invalid)?;
    if state.running.is_some()
        || state.active_shell.is_some()
        || state.initialization_running
        || state.closing
        || state.poisoned
        || state.unsettled.is_some()
        || !state.pending_model_switches.is_empty()
    {
        return Ok(ExtensionControlOutcome::Busy {});
    }
    match control {
        ExtensionControl::PinContext { item_id } => {
            super::context_surgery::apply_registered_context_surgery(
                state, config, events, item_id, true,
            )
            .await?;
        }
        ExtensionControl::EvictContext { item_id } => {
            super::context_surgery::apply_registered_context_surgery(
                state, config, events, item_id, false,
            )
            .await?;
        }
        ExtensionControl::SelectMode { mode } => {
            let definition = config
                .modes
                .get(&mode.0)
                .ok_or_else(|| invalid("unknown mode"))?;
            if mode_permission_base(definition) == SessionMode::Execute && state.plan_gate_active {
                return Err(invalid(
                    "plan_approval_required: approve a plan before Execute",
                ));
            }
            apply_mode_change(state, events, &config.event_sink, mode, &config.modes).await?;
        }
        ExtensionControl::SelectModel { model, provider } => {
            if !config.model.has_model_alias(&model.0) {
                return Err(invalid("unknown model alias"));
            }
            if provider
                .as_ref()
                .is_some_and(|provider| !config.model.has_provider_for_alias(&model.0, provider))
            {
                return Err(invalid("unknown provider route"));
            }
            let needs_choice = state
                .conversation
                .iter()
                .any(|turn| turn.role != Role::System)
                && (state.model_alias != model.0 || state.provider != provider);
            if !needs_choice {
                config.model.prepare_model(&model.0).await?;
            }
            if let Some(question_id) =
                super::model_switch::request_model_selection(state, config, events, model, provider)
                    .await?
            {
                return Ok(ExtensionControlOutcome::ContextChoiceRequired { question_id });
            }
        }
    }
    Ok(ExtensionControlOutcome::Applied {})
}
fn invalid(message: &str) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(message.into())
}
