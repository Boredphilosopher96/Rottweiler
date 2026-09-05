use crate::engine::AgentLoopError;
use crate::engine::MAX_PLUGIN_MESSAGE_BYTES;
use crate::engine::MessageDisposition;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::ActorState;
use crate::engine::session::validate_plugin_id;
use crate::engine::session::validate_plugin_text;
use crate::engine::turn::StartTurnRuntime;
use crate::engine::turn::emit;
use crate::engine::turn::start_turn;

pub(super) async fn handle_plugin_message(
    plugin_id: String,
    content: String,
    state: &mut ActorState,
    runtime: StartTurnRuntime<'_>,
) -> Result<MessageDisposition, AgentLoopError> {
    validate_plugin_id(&plugin_id)?;
    validate_plugin_text("injected message", &content, MAX_PLUGIN_MESSAGE_BYTES)?;
    if state.poisoned {
        return Err(AgentLoopError::InvalidConfiguration(
            "session requires recovery before plugin message injection".to_owned(),
        ));
    }
    if state.active_shell.is_some() {
        return Err(AgentLoopError::InvalidConfiguration(
            "an agent turn cannot start while the foreground user shell is active".to_owned(),
        ));
    }
    if state.initialization_running {
        return Err(AgentLoopError::InvalidConfiguration(
            "workspace initialization is still running".to_owned(),
        ));
    }
    let content = runtime.config.secret_redactor.redact(&content);
    validate_plugin_text(
        "redacted injected message",
        &content,
        MAX_PLUGIN_MESSAGE_BYTES,
    )?;
    let disposition = if state.running.is_some()
        || state.pending_command.is_some()
        || state.pending_model_preparation.is_some()
    {
        let position = state
            .queued_positions
            .back()
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                AgentLoopError::InvalidConfiguration(
                    "queued message position space is exhausted".to_owned(),
                )
            })?;
        state.queued.push_back(content.clone());
        state.queued_positions.push_back(position);
        if let Err(error) = emit(
            state,
            runtime.events,
            &runtime.config.event_sink,
            PendingEvent::MessageQueued {
                position,
                content: content.clone(),
                attachments: Vec::new(),
            },
        )
        .await
        .map(|_| ())
        {
            state.queued.pop_back();
            state.queued_positions.pop_back();
            return Err(error);
        }
        MessageDisposition::Queued
    } else {
        start_turn(
            state,
            runtime.config,
            runtime.tool_context,
            runtime.signals,
            runtime.events,
            vec![(content.clone(), Vec::new())],
            runtime.active_turn,
        )
        .await?;
        MessageDisposition::Started
    };
    if let Err(error) = emit(
        state,
        runtime.events,
        &runtime.config.event_sink,
        PendingEvent::PluginMessageInjected {
            plugin_id,
            content,
            queued: disposition == MessageDisposition::Queued,
        },
    )
    .await
    .map(|_| ())
    {
        if let Some(running) = &state.running {
            running.cancellation.cancel();
        }
        state.poisoned = true;
        return Err(error);
    }
    Ok(disposition)
}
