use crate::engine::AgentLoopError;
use crate::engine::MessageDisposition;
use crate::engine::dispatch::DispatchContext;
use crate::engine::pending_event::PendingEvent;
use crate::engine::turn::emit;
use crate::engine::turn::start_turn;
use rw_types::Attachment;
use rw_types::CommandMeta;
use tokio::sync::oneshot;

#[allow(clippy::too_many_lines, clippy::similar_names)]
pub(super) async fn dispatch_message(
    command_meta: CommandMeta,
    content: String,
    attachments: Vec<Attachment>,
    observed_turn: u64,
    respond: oneshot::Sender<Result<MessageDisposition, AgentLoopError>>,
    context: DispatchContext<'_>,
) {
    let DispatchContext {
        state,
        config,
        tool_context,
        turn_signals,
        events,
        active_turn,
        command_descriptors,
        mode_registry,
    } = context;
    if content.trim_start().starts_with('/') && state.pending_model_preparation.is_some() {
        let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(
            "model preparation owns the session selection".into(),
        )));
        return;
    }
    if content.trim_start().starts_with('/') {
        let bound = config.commands.bind_line(&content);
        super::command_job::start(
            command_meta,
            bound,
            observed_turn,
            super::command_job::CommandReply::Direct(respond),
            DispatchContext {
                state,
                config,
                tool_context,
                turn_signals,
                events,
                active_turn,
                command_descriptors,
                mode_registry,
            },
        )
        .await;
    } else if state.initialization_running {
        let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(
            "workspace initialization is still running".to_owned(),
        )));
    } else if state.running.is_some()
        || state.pending_command.is_some()
        || state.pending_model_preparation.is_some()
    {
        let content = config.secret_redactor.redact(&content);
        let Some(position) = state
            .queued_positions
            .back()
            .copied()
            .unwrap_or(0)
            .checked_add(1)
        else {
            let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(
                "queued message position space is exhausted".to_owned(),
            )));
            return;
        };
        state.queued.push_back(content.clone());
        state.queued_positions.push_back(position);
        let persisted = emit(
            state,
            events,
            &config.event_sink,
            PendingEvent::MessageQueued {
                position,
                content,
                attachments: Vec::new(),
            },
        )
        .await
        .map(|_| ());
        if let Err(error) = persisted {
            state.queued.pop_back();
            state.queued_positions.pop_back();
            let _ = respond.send(Err(error));
        } else {
            let _ = respond.send(Ok(MessageDisposition::Queued));
        }
    } else {
        let result = start_turn(
            state,
            config,
            tool_context,
            turn_signals,
            events,
            vec![(content, attachments)],
            active_turn,
        )
        .await;
        let _ = respond.send(result.map(|()| MessageDisposition::Started));
    }
}
