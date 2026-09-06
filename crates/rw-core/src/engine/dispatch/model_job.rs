//! Model preparation retains its runtime while the actor services callbacks.
use super::DispatchContext;
use crate::engine::session::{
    ActorState, PreparedModelSwitch, ProtocolCompletion, SessionActorConfig,
};
use crate::engine::{AgentLoopError, RoutedEvent, model_switch_answer};
use rw_tools::CancellationToken;
use rw_types::extension_control::{ExtensionControl, ExtensionControlOutcome};
use rw_types::extension_invocation::ExtensionInvocationId;
use rw_types::{
    AttachmentData, ClientCommand, ClientId, CommandOutcome, ModeId, ModelContextTransfer,
};
use std::sync::Arc;
use tokio::sync::{broadcast, oneshot};

const PREPARATION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
type Completion = Option<oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>>;
type ResultValue = Result<(), AgentLoopError>;

pub(in crate::engine) struct PluginSelection {
    pub(super) origin: Option<ExtensionInvocationId>,
    pub(super) control: ExtensionControl,
    pub(super) respond: oneshot::Sender<Result<ExtensionControlOutcome, AgentLoopError>>,
}

pub(in crate::engine) enum SelectionAction {
    Protocol {
        authority: Option<(crate::FamilyControlAuthority, rw_types::SequenceId)>,
        command: Box<ClientCommand>,
        respond: oneshot::Sender<CommandOutcome>,
        completion: Completion,
    },
    Plugin(PluginSelection),
    Commit {
        prepared: PreparedModelSwitch,
        clear_context: bool,
        completion: Completion,
    },
}

pub(in crate::engine) struct PendingPreparation {
    receive: oneshot::Receiver<ResultValue>,
    owner: Arc<SessionActorConfig>,
    driver: Option<ClientId>,
    mode: ModeId,
    selected_model: String,
    selected_provider: Option<String>,
    alias: String,
    cause: Option<rw_types::RequestId>,
    action: SelectionAction,
}

pub(super) fn protocol_alias(command: &ClientCommand, state: &ActorState) -> Option<String> {
    match command {
        ClientCommand::SwitchModel {
            model, provider, ..
        } => {
            let needs_choice = state.has_conversation_context()
                && (state.model_alias != model.0 || state.provider != *provider);
            (!needs_choice).then(|| model.0.clone())
        }
        ClientCommand::AnswerQuestion {
            question_id,
            answers,
            ..
        } => {
            let strategy = model_switch_answer(answers, question_id)?;
            (strategy != ModelContextTransfer::PassSummary)
                .then(|| {
                    state
                        .pending_model_switches
                        .get(&question_id.0)
                        .map(|pending| pending.model.0.clone())
                })
                .flatten()
        }
        ClientCommand::SendMessage { attachments, .. } => attachments
            .iter()
            .any(|attachment| {
                matches!(&attachment.data, AttachmentData::InlineBase64 { .. })
                    && matches!(
                        attachment.media_type.as_str(),
                        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                    )
            })
            .then(|| state.model_alias.clone()),
        _ => None,
    }
}

pub(in crate::engine) fn start(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    events: &broadcast::Sender<RoutedEvent>,
    alias: String,
    action: SelectionAction,
) {
    if state.pending_model_preparation.is_some() {
        reject(
            action,
            invalid("model preparation is already admitted"),
            state,
            events,
        );
        return;
    }
    let (send, receive) = oneshot::channel();
    let owner = Arc::clone(config);
    let prepared_alias = alias.clone();
    let task = state.tasks.spawn(
        Arc::clone(config),
        CancellationToken::default(),
        async move {
            let prepare = owner.model.prepare_model(&prepared_alias);
            tokio::pin!(prepare);
            tokio::select! {
                result = &mut prepare => { let _ = send.send(result); }
                () = tokio::time::sleep(PREPARATION_DEADLINE) => {
                    let _ = send.send(Err(AgentLoopError::EffectsUnsettled(
                        "model preparation exceeded its settlement deadline".into(),
                    )));
                    // The proof deadline does not cancel a provider's native effects.
                    // This task and ActorTasks retain the exact generation until return.
                    let _ = prepare.await;
                    }
            }
        },
    );
    match task {
        Ok(task) => {
            drop(task);
            state.pending_model_preparation = Some(PendingPreparation {
                receive,
                owner: Arc::clone(config),
                driver: state.control.driver(),
                mode: state.mode_id.clone(),
                selected_model: state.model_alias.clone(),
                selected_provider: state.provider.clone(),
                alias,
                cause: state.transient_cause.clone(),
                action,
            });
        }
        Err(error) => reject(action, error, state, events),
    }
}

pub(in crate::engine) async fn wait(pending: &mut Option<PendingPreparation>) -> ResultValue {
    match pending {
        Some(pending) => (&mut pending.receive).await.unwrap_or_else(|_| {
            Err(AgentLoopError::EffectsUnsettled(
                "model preparation exited without completion proof".into(),
            ))
        }),
        None => std::future::pending().await,
    }
}

pub(in crate::engine) async fn finish(mut result: ResultValue, context: DispatchContext<'_>) {
    let Some(pending) = context.state.pending_model_preparation.take() else {
        return;
    };
    if matches!(result, Err(AgentLoopError::EffectsUnsettled(_))) {
        context.state.unsettled = Some("model preparation effects did not settle".into());
        context.state.tasks.cancel();
    }
    if result.is_ok()
        && (context.state.closing
            || context.state.poisoned
            || context.state.unsettled.is_some()
            || !Arc::ptr_eq(&pending.owner, context.config)
            || pending.driver != context.state.control.driver()
            || pending.mode != context.state.mode_id
            || pending.selected_model != context.state.model_alias
            || pending.selected_provider != context.state.provider)
    {
        result = Err(invalid(
            "model preparation completion authority is no longer current",
        ));
    }
    if let Err(error) = result {
        if !matches!(error, AgentLoopError::EffectsUnsettled(_)) {
            pending.owner.model.discard_prepared_model(&pending.alias);
        }
        reject(pending.action, error, context.state, context.events);
        return;
    }
    match pending.action {
        SelectionAction::Protocol {
            authority,
            command,
            respond,
            completion,
        } => {
            // Re-run all command authority and input checks after preparation.
            if !super::admission::dispatch_protocol(
                *command, respond, completion, true, authority, context,
            )
            .await
            {
                pending.owner.model.discard_prepared_model(&pending.alias);
            }
        }
        SelectionAction::Plugin(selection) => {
            let previous = std::mem::replace(&mut context.state.transient_cause, pending.cause);
            let result = super::plugin_control::control(
                context.state,
                context.config,
                context.events,
                selection.origin.as_ref(),
                selection.control,
                true,
            )
            .await
            .and_then(|outcome| {
                outcome.ok_or_else(|| invalid("prepared model selection was deferred"))
            });
            if result.is_err() {
                pending.owner.model.discard_prepared_model(&pending.alias);
            }
            let _ = selection.respond.send(result);
            context.state.transient_cause = previous;
        }
        SelectionAction::Commit {
            prepared,
            clear_context,
            completion,
        } => {
            let previous = std::mem::replace(&mut context.state.transient_cause, pending.cause);
            let result = super::model_switch::commit_prepared_model_switch(
                context.state,
                context.config,
                context.events,
                prepared,
                clear_context,
            )
            .await;
            if let Some(complete) = completion {
                let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
            }
            context.state.transient_cause = previous;
        }
    }
}

pub(super) async fn dispatch_plugin(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    events: &broadcast::Sender<RoutedEvent>,
    selection: PluginSelection,
) {
    match super::plugin_control::control(
        state,
        config,
        events,
        selection.origin.as_ref(),
        selection.control.clone(),
        false,
    )
    .await
    {
        Ok(None) => {
            let ExtensionControl::SelectModel { model, .. } = &selection.control else {
                let _ = selection
                    .respond
                    .send(Err(invalid("only model selection may defer preparation")));
                return;
            };
            start(
                state,
                config,
                events,
                model.0.clone(),
                SelectionAction::Plugin(selection),
            );
        }
        result => {
            let _ = selection.respond.send(
                result.and_then(|value| value.ok_or_else(|| invalid("missing control result"))),
            );
        }
    }
}

fn reject(
    action: SelectionAction,
    error: AgentLoopError,
    state: &ActorState,
    events: &broadcast::Sender<RoutedEvent>,
) {
    match action {
        SelectionAction::Protocol {
            authority: _,
            command,
            respond,
            completion,
        } => {
            let outcome =
                super::replies::protocol_rejection("model_unavailable", error.to_string());
            super::replies::send_ack(
                state,
                events,
                command.meta(),
                command.session_id().cloned(),
                outcome.clone(),
            );
            let _ = respond.send(outcome);
            if let Some(complete) = completion {
                let _ = complete.send(Err(error));
            }
        }
        SelectionAction::Plugin(selection) => {
            let _ = selection.respond.send(Err(error));
        }
        SelectionAction::Commit { completion, .. } => {
            if let Some(complete) = completion {
                let _ = complete.send(Err(error));
            }
        }
    }
}
fn invalid(message: &str) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(message.into())
}

pub(super) fn admit_while_pending(command: &ClientCommand) -> bool {
    super::command_job::admit_while_pending(command)
        && !matches!(command, ClientCommand::AnswerQuestion { .. })
}
