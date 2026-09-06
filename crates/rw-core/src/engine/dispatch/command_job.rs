//! One owned command keeps callbacks serviceable and retains its exact generation.
use super::DispatchContext;
use crate::engine::commands::SessionCommandOutput;
use crate::engine::session::{ProtocolCompletion, SessionActorConfig};
use crate::engine::{AgentLoopError, MessageDisposition};
use crate::ui::BoundUiCommand;
use rw_ext::CommandRegistryError;
use rw_tools::CancellationToken;
use rw_types::{ClientId, CommandMeta, ModeId};
use std::sync::Arc;
use tokio::sync::oneshot;

pub(in crate::engine) type Execution = Result<PreparedCommand, AgentLoopError>;

pub(in crate::engine) struct PreparedCommand {
    pub(super) output: SessionCommandOutput,
    pub(super) change: super::command_generation::PreparedChange,
}

pub(in crate::engine) enum CommandReply {
    Direct(oneshot::Sender<Result<MessageDisposition, AgentLoopError>>),
    Protocol(Option<oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>>),
    Control(Option<oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>>),
}
impl CommandReply {
    pub(super) fn send(self, result: Result<MessageDisposition, AgentLoopError>) -> Result<(), ()> {
        match self {
            Self::Direct(sender) => sender.send(result).map_err(|_| ()),
            Self::Protocol(Some(sender)) => sender
                .send(result.map(ProtocolCompletion::Message))
                .map_err(|_| ()),
            Self::Control(Some(sender)) => sender
                .send(result.map(|_| ProtocolCompletion::Unit))
                .map_err(|_| ()),
            Self::Protocol(None) | Self::Control(None) => Ok(()),
        }
    }
}

pub(in crate::engine) struct PendingCommand {
    origin: rw_types::extension_invocation::ExtensionInvocationId,
    host_tools: Arc<[String]>,
    receive: oneshot::Receiver<Execution>,
    owner: Arc<SessionActorConfig>,
    mode: ModeId,
    driver: Option<ClientId>,
    meta: CommandMeta,
    name: String,
    navigation: Option<rw_types::extension_control::SessionNavigationTarget>,
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
                Err(command_error(&error)),
                reply,
                context,
            )
            .await;
            return;
        }
    };
    let origin = match invocation_id() {
        Ok(origin) => origin,
        Err(error) => {
            let _ = reply.send(Err(error));
            return;
        }
    };
    let bound = bound.with_origin(origin.clone());
    let name = bound.name().to_owned();
    let host_tools = bound.host_tools();
    let mut snapshot = super::command_snapshot::capture(context.state, context.config);
    let owner = Arc::clone(context.config);
    let next_turn = context.state.next_turn;
    let (prepare_started, preparation) = oneshot::channel();
    let operation = async move {
        let result = bound
            .execute(&mut snapshot)
            .await
            .map_err(|error| command_error(&error));
        let result = match result {
            Ok(output) => {
                let _ = prepare_started.send(());
                super::command_generation::prepare_output(output, &owner, next_turn).await
            }
            Err(error) => Err(error),
        };
        (result, Some(bound))
    };
    admit(
        meta,
        observed_turn,
        reply,
        name,
        origin,
        host_tools,
        preparation,
        operation,
        context.state,
        context.config,
    );
}

fn invocation_id() -> Result<rw_types::extension_invocation::ExtensionInvocationId, AgentLoopError>
{
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| {
        AgentLoopError::InvalidConfiguration("command invocation identity unavailable".into())
    })?;
    Ok(rw_types::extension_invocation::ExtensionInvocationId::from_bytes(bytes))
}

pub(super) fn start_development(
    meta: CommandMeta,
    source: Option<std::path::PathBuf>,
    reply: CommandReply,
    state: &mut crate::engine::session::ActorState,
    config: &Arc<SessionActorConfig>,
) {
    let origin = match invocation_id() {
        Ok(origin) => origin,
        Err(error) => {
            let _ = reply.send(Err(error));
            return;
        }
    };
    let owner = Arc::clone(config);
    let (prepare_started, preparation) = oneshot::channel();
    let operation = async move {
        let _ = prepare_started.send(());
        (
            super::command_generation::prepare_development(source.as_deref(), &owner).await,
            None,
        )
    };
    admit(
        meta,
        state.next_turn,
        reply,
        "plugin-development".into(),
        origin,
        Arc::from([]),
        preparation,
        operation,
        state,
        config,
    );
}

#[allow(clippy::too_many_arguments)]
fn admit(
    meta: CommandMeta,
    observed_turn: u64,
    reply: CommandReply,
    name: String,
    origin: rw_types::extension_invocation::ExtensionInvocationId,
    host_tools: Arc<[String]>,
    preparation: oneshot::Receiver<()>,
    operation: impl std::future::Future<Output = (Execution, Option<BoundUiCommand>)> + Send + 'static,
    state: &mut crate::engine::session::ActorState,
    config: &Arc<SessionActorConfig>,
) {
    if state.pending_command.is_some() {
        let _ = reply.send(Err(AgentLoopError::InvalidConfiguration(
            "another command is still executing".into(),
        )));
        return;
    }
    let (send, receive) = oneshot::channel();
    let owner = Arc::clone(config);
    let task = state.tasks.spawn(
        Arc::clone(config),
        CancellationToken::default(),
        async move {
            tokio::pin!(operation);
            let (result, bound) = tokio::select! {
                result = &mut operation => result,
                () = preparation_deadline(preparation) => {
                    let _ = send.send(Err(AgentLoopError::EffectsUnsettled(
                        "command generation preparation exceeded its proof deadline".into(),
                    )));
                    // Keep the real operation, generation and effects owned after
                    // reporting failed proof. The deadline does not cancel them.
                    let (result, bound) = operation.await;
                    if let Ok(prepared) = &result { prepared.change.abort(&owner).await; }
                    if unproven(&result) { std::future::pending::<()>().await; }
                    drop(bound);
                    return;
                }
            };
            let unproven = unproven(&result);
            if let Err(Ok(prepared)) = send.send(result) {
                prepared.change.abort(&owner).await;
            }
            if unproven {
                // A handler absent from the public slash catalog still owns its
                // actual effects; failed proof cannot release that handler.
                std::future::pending::<()>().await;
            }
            drop(bound);
        },
    );
    match task {
        Ok(task) => {
            // ActorTasks owns completion independently of this join handle.
            drop(task);
            state.pending_command = Some(PendingCommand {
                origin,
                host_tools,
                receive,
                owner: Arc::clone(config),
                mode: state.mode_id.clone(),
                driver: state.control.driver(),
                meta,
                name,
                navigation: None,
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
            Err(AgentLoopError::EffectsUnsettled(
                "command owner exited without completion proof".into(),
            ))
        }),
        None => std::future::pending().await,
    }
}

pub(in crate::engine) async fn finish(mut result: Execution, context: DispatchContext<'_>) {
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
        if let Ok(prepared) = &result {
            if prepared.change.requires_publication() {
                context.state.unsettled =
                    Some("prepared generation lost publication authority".into());
                context.state.tasks.cancel();
            }
            prepared.change.abort(&pending.owner).await;
        }
        let _ = pending.reply.send(Err(AgentLoopError::InvalidConfiguration(
            "command completion authority is no longer current".into(),
        )));
        return;
    }
    if let Some(target) = pending.navigation
        && let Ok(prepared) = &mut result
    {
        if prepared.output.action != crate::engine::commands::SessionCommandAction::None {
            prepared.change.abort(&pending.owner).await;
            let _ = pending.reply.send(Err(AgentLoopError::InvalidConfiguration(
                "navigation cannot accompany another command action".into(),
            )));
            return;
        }
        prepared.output.action = crate::engine::commands::SessionCommandAction::Navigate { target };
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

async fn preparation_deadline(started: oneshot::Receiver<()>) {
    if started.await.is_err() {
        std::future::pending::<()>().await;
    }
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
}

fn unproven(result: &Execution) -> bool {
    matches!(result, Err(AgentLoopError::EffectsUnsettled(_)))
}

fn command_error(error: &CommandRegistryError) -> AgentLoopError {
    if matches!(error, CommandRegistryError::Execution {source,..} if matches!(source.code(), "panic" | "effects_unsettled"))
    {
        AgentLoopError::EffectsUnsettled(error.to_string())
    } else {
        AgentLoopError::Extension(error.to_string())
    }
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

impl PendingCommand {
    pub(in crate::engine) fn host_tools(&self) -> Arc<[String]> {
        Arc::clone(&self.host_tools)
    }
    pub(super) fn meta(&self) -> &CommandMeta {
        &self.meta
    }

    pub(in crate::engine) fn allows(
        &self,
        origin: &rw_types::extension_invocation::ExtensionInvocationId,
        config: &Arc<SessionActorConfig>,
        driver: Option<&ClientId>,
    ) -> bool {
        &self.origin == origin && Arc::ptr_eq(&self.owner, config) && self.driver.as_ref() == driver
    }
    pub(super) fn queue_navigation(
        &mut self,
        target: rw_types::extension_control::SessionNavigationTarget,
    ) -> Result<(), AgentLoopError> {
        if self.navigation.is_some() {
            return Err(AgentLoopError::InvalidConfiguration(
                "navigation is already requested by this command".into(),
            ));
        }
        self.navigation = Some(target);
        Ok(())
    }
    pub(super) fn update_mode(&mut self, mode: ModeId) {
        self.mode = mode;
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    #[tokio::test(start_paused = true)]
    async fn preparation_clock_begins_after_the_callback_phase() {
        let (send, receive) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(super::preparation_deadline(receive));
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        assert!(
            !task.is_finished(),
            "callback time is not generation preparation"
        );
        send.send(()).expect("begin preparation");
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(29)).await;
        assert!(!task.is_finished());
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        task.await.expect("preparation deadline");
    }
}
