//! One owned command keeps callbacks serviceable and retains its exact generation.
use super::DispatchContext;
use crate::engine::commands::SessionCommandOutput;
use crate::engine::session::{ProtocolCompletion, SessionActorConfig};
use crate::engine::{AgentLoopError, MessageDisposition};
use crate::ui::BoundUiCommand;
use rw_ext::{CommandExecutionError, CommandRegistryError};
use rw_tools::CancellationToken;
use rw_types::{ClientId, CommandMeta, ModeId};
use std::sync::Arc;
use tokio::sync::oneshot;

type Execution = Result<SessionCommandOutput, CommandRegistryError>;

pub(in crate::engine) enum CommandReply {
    Direct(oneshot::Sender<Result<MessageDisposition, AgentLoopError>>),
    Protocol(Option<oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>>),
}
impl CommandReply {
    pub(super) fn send(self, result: Result<MessageDisposition, AgentLoopError>) -> Result<(), ()> {
        match self {
            Self::Direct(sender) => sender.send(result).map_err(|_| ()),
            Self::Protocol(Some(sender)) => sender
                .send(result.map(ProtocolCompletion::Message))
                .map_err(|_| ()),
            Self::Protocol(None) => Ok(()),
        }
    }
}

pub(in crate::engine) struct PendingCommand {
    receive: oneshot::Receiver<Execution>,
    owner: Arc<SessionActorConfig>,
    mode: ModeId,
    driver: Option<ClientId>,
    meta: CommandMeta,
    name: String,
    observed_turn: u64,
    reply: CommandReply,
}

pub(super) async fn start(
    meta: CommandMeta,
    bound: Result<BoundUiCommand, CommandRegistryError>,
    observed_turn: u64,
    reply: CommandReply,
    context: DispatchContext<'_>,
) {
    if context.state.pending_command.is_some() {
        let _ = reply.send(Err(AgentLoopError::InvalidConfiguration(
            "another command is still executing".into(),
        )));
        return;
    }
    let bound = match bound {
        Ok(bound) => bound,
        Err(error) => {
            super::command_result::apply(
                meta,
                String::new(),
                observed_turn,
                Err(error),
                reply,
                context,
            )
            .await;
            return;
        }
    };
    let name = bound.name().to_owned();
    let mut snapshot = super::command_snapshot::capture(context.state, context.config);
    let (send, receive) = oneshot::channel();
    let task = context.state.tasks.spawn(
        Arc::clone(context.config),
        CancellationToken::default(),
        async move {
            // Dropping the user/HTTP waiter never cancels this admitted operation.
            // RPC execution has its own immutable deadline and settlement barrier.
            let result = bound.execute(&mut snapshot).await;
            let unproven = unproven(&result);
            let _ = send.send(result);
            if unproven {
                // Retain the actual handler too: UI commands need not be registered
                // in the generation's public slash-command catalog.
                std::future::pending::<()>().await;
            }
            drop(bound);
        },
    );
    match task {
        Ok(task) => {
            // ActorTasks owns completion independently of this join handle.
            drop(task);
            context.state.pending_command = Some(PendingCommand {
                receive,
                owner: Arc::clone(context.config),
                mode: context.state.mode_id.clone(),
                driver: context.state.control.driver(),
                meta,
                name,
                observed_turn,
                reply,
            });
        }
        Err(error) => {
            let _ = reply.send(Err(error));
        }
    }
}

pub(in crate::engine) async fn wait(pending: &mut Option<PendingCommand>) -> Execution {
    match pending {
        Some(pending) => (&mut pending.receive).await.unwrap_or_else(|_| {
            Err(CommandRegistryError::Execution {
                name: pending.name.clone(),
                source: CommandExecutionError::new(
                    "effects_unsettled",
                    "command owner exited without completion proof",
                ),
            })
        }),
        None => std::future::pending().await,
    }
}

pub(in crate::engine) async fn finish(result: Execution, context: DispatchContext<'_>) {
    let Some(pending) = context.state.pending_command.take() else {
        return;
    };
    if unproven(&result) {
        context.state.unsettled = Some("command effects did not settle".into());
        context.state.tasks.cancel();
        let _ = pending.reply.send(Err(AgentLoopError::EffectsUnsettled(
            "command effects did not settle".into(),
        )));
        return;
    }
    if context.state.closing
        || context.state.poisoned
        || context.state.unsettled.is_some()
        || !Arc::ptr_eq(&pending.owner, context.config)
        || pending.mode != context.state.mode_id
        || pending.driver != context.state.control.driver()
    {
        let _ = pending.reply.send(Err(AgentLoopError::InvalidConfiguration(
            "command completion authority is no longer current".into(),
        )));
        return;
    }
    let previous_cause = context
        .state
        .transient_cause
        .replace(pending.meta.request_id.clone());
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
    super::command_result::apply(
        pending.meta,
        format!("/{}", pending.name),
        pending.observed_turn,
        result,
        pending.reply,
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
    state.transient_cause = previous_cause;
}

fn unproven(result: &Execution) -> bool {
    matches!(result, Err(CommandRegistryError::Execution {source,..}) if matches!(source.code(), "panic" | "effects_unsettled"))
}

/// Only conversational queueing, lease takeover, existing turn answers and
/// read-only requests can cross a pending command's policy boundary.
pub(super) fn admit_while_pending(command: &rw_types::ClientCommand) -> bool {
    use rw_types::ClientCommand;
    match command {
        ClientCommand::SendMessage { content, .. } => !content.trim_start().starts_with('/'),
        ClientCommand::AttachSession { .. }
        | ClientCommand::TakeDriver { .. }
        | ClientCommand::Interrupt { .. }
        | ClientCommand::ApproveTool { .. }
        | ClientCommand::AnswerQuestion { .. }
        | ClientCommand::GetContext { .. }
        | ClientCommand::GetCost { .. }
        | ClientCommand::GetSessionReview { .. }
        | ClientCommand::DumpPrompt { .. }
        | ClientCommand::ListPermissions { .. } => true,
        _ => false,
    }
}
