mod accepted;
mod admission;
mod command_generation;
pub(super) mod command_job;
mod command_result;
mod command_snapshot;
mod compaction;
mod completed_turns;
mod context_surgery;
mod controls;
mod initialization;
mod message_input;
mod messages;
pub(super) mod model_job;
mod model_switch;
mod navigation;
mod permissions;
mod plugin_control;
mod plugin_messages;
mod replies;
mod rewind;
mod source_rewind;
mod ui_actions;
use crate::engine::AgentLoopError;
use crate::engine::MAX_CAPTURED_SHELL_OUTPUT_BYTES;
use crate::engine::MAX_PLUGIN_NOTIFICATION_MESSAGE_BYTES;
use crate::engine::MAX_PLUGIN_NOTIFICATION_TITLE_BYTES;
use crate::engine::MAX_PLUGIN_STATUS_BYTES;
use crate::engine::RoutedEvent;
use crate::engine::SessionSnapshot;
use crate::engine::dispatch::admission::dispatch_protocol;
use crate::engine::dispatch::messages::dispatch_message;
use crate::engine::dispatch::plugin_messages::handle_plugin_message;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::shell_context_turn;
use crate::engine::session::ActorCommand;
use crate::engine::session::ActorState;
use crate::engine::session::SessionActorConfig;
use crate::engine::session::validate_plugin_id;
use crate::engine::session::validate_plugin_text;
use crate::engine::turn::StartTurnRuntime;
use crate::engine::turn::TurnSignal;
use crate::engine::turn::emit;
pub(super) use message_input::prepare_user_message;
use rw_ext::CommandDescriptor;
use rw_ext::ModeRegistry;
use rw_tools::ToolContext;
use rw_types::EngineEvent;
use rw_types::SequenceId;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

pub(super) struct DispatchContext<'a> {
    pub(super) state: &'a mut ActorState,
    pub(super) config: &'a mut Arc<SessionActorConfig>,
    pub(super) tool_context: &'a mut ToolContext,
    pub(super) turn_signals: &'a mpsc::UnboundedSender<TurnSignal>,
    pub(super) events: &'a broadcast::Sender<RoutedEvent>,
    pub(super) active_turn: &'a Arc<AtomicU64>,
    pub(super) command_descriptors: &'a Arc<RwLock<Arc<[CommandDescriptor]>>>,
    pub(super) mode_registry: &'a Arc<RwLock<Arc<ModeRegistry>>>,
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_actor_command(
    command: ActorCommand,
    state: &mut ActorState,
    config: &mut Arc<SessionActorConfig>,
    tool_context: &mut ToolContext,
    turn_signals: &mpsc::UnboundedSender<TurnSignal>,
    events: &broadcast::Sender<RoutedEvent>,
    active_turn: &Arc<AtomicU64>,
    command_descriptors: &Arc<RwLock<Arc<[CommandDescriptor]>>>,
    mode_registry: &Arc<RwLock<Arc<ModeRegistry>>>,
) {
    match command {
        ActorCommand::Controls { respond } => {
            let _ = respond.send(controls::snapshot(state));
        }
        ActorCommand::Protocol {
            command,
            respond,
            completion,
        } => {
            dispatch_protocol(
                command,
                respond,
                completion,
                false,
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
        }
        ActorCommand::PluginInjectMessage {
            plugin_id,
            content,
            respond,
        } => {
            let result = handle_plugin_message(
                plugin_id,
                content,
                state,
                StartTurnRuntime {
                    config,
                    tool_context,
                    signals: turn_signals,
                    events,
                    active_turn,
                },
            )
            .await;
            let _ = respond.send(result);
        }
        ActorCommand::PluginContextRead { request, respond } => {
            let _ = respond.send(plugin_control::read_context(state, config, &request));
        }
        ActorCommand::PluginToolCall { request, respond } => {
            crate::engine::turn::plugin_tool::start(
                request,
                respond,
                state,
                StartTurnRuntime {
                    config,
                    tool_context,
                    signals: turn_signals,
                    events,
                    active_turn,
                },
            )
            .await;
        }
        ActorCommand::PluginControl {
            origin,
            control,
            respond,
        } => {
            model_job::dispatch_plugin(
                state,
                config,
                events,
                model_job::PluginSelection {
                    origin,
                    control,
                    respond,
                },
            )
            .await;
        }
        ActorCommand::PluginQuery { respond } => {
            let _ = respond.send(Ok(rw_types::extension_contract::ExtensionSessionSnapshot {
                session_id: state.session_id.clone(),
                title: state.session_title.clone(),
                mode_id: state.mode_id.clone(),
                model_alias: state.model_alias.clone(),
                active_turn: state
                    .running
                    .as_ref()
                    .map(|turn| crate::engine::wire_turn_id(turn.id)),
                queued_messages: state.queued.len(),
                last_sequence: state.sequence.map(SequenceId),
            }));
        }
        ActorCommand::PluginStateRead { plugin_id, respond } => {
            let result = async {
                validate_plugin_id(&plugin_id)?;
                config
                    .event_sink
                    .extension_state(&plugin_id)
                    .await
                    .map(|view| view.snapshot)
            }
            .await;
            let _ = respond.send(result);
        }
        ActorCommand::PluginStateCommit {
            plugin_id,
            transaction,
            respond,
        } => {
            let result =
                super::plugin_state::commit(plugin_id, transaction, state, config, events).await;
            let _ = respond.send(result);
        }
        ActorCommand::PluginSetStatus {
            plugin_id,
            status,
            respond,
        } => {
            let result = async {
                validate_plugin_id(&plugin_id)?;
                validate_plugin_text("plugin status", &status, MAX_PLUGIN_STATUS_BYTES)?;
                if state.poisoned {
                    return Err(AgentLoopError::InvalidConfiguration(
                        "session requires recovery before plugin status updates".to_owned(),
                    ));
                }
                let status = config.secret_redactor.redact(&status);
                validate_plugin_text("redacted plugin status", &status, MAX_PLUGIN_STATUS_BYTES)?;
                emit(
                    state,
                    events,
                    &config.event_sink,
                    PendingEvent::PluginStatusChanged { plugin_id, status },
                )
                .await
                .map(|_| ())
            }
            .await;
            let _ = respond.send(result);
        }
        ActorCommand::PluginNotify {
            plugin_id,
            title,
            message,
            respond,
        } => {
            let result = async {
                validate_plugin_id(&plugin_id)?;
                validate_plugin_text(
                    "notification title",
                    &title,
                    MAX_PLUGIN_NOTIFICATION_TITLE_BYTES,
                )?;
                validate_plugin_text(
                    "notification message",
                    &message,
                    MAX_PLUGIN_NOTIFICATION_MESSAGE_BYTES,
                )?;
                if state.poisoned {
                    return Err(AgentLoopError::InvalidConfiguration(
                        "session requires recovery before plugin notifications".to_owned(),
                    ));
                }
                let title = config.secret_redactor.redact(&title);
                let message = config.secret_redactor.redact(&message);
                validate_plugin_text(
                    "redacted notification title",
                    &title,
                    MAX_PLUGIN_NOTIFICATION_TITLE_BYTES,
                )?;
                validate_plugin_text(
                    "redacted notification message",
                    &message,
                    MAX_PLUGIN_NOTIFICATION_MESSAGE_BYTES,
                )?;
                emit(
                    state,
                    events,
                    &config.event_sink,
                    PendingEvent::UiNotification {
                        plugin_id,
                        title,
                        message,
                    },
                )
                .await
                .map(|_| ())
            }
            .await;
            let _ = respond.send(result);
        }
        ActorCommand::SendMessage {
            command_meta,
            content,
            attachments,
            observed_turn,
            respond,
        } => {
            dispatch_message(
                command_meta,
                content,
                attachments,
                observed_turn,
                respond,
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
        }
        #[cfg(test)]
        ActorCommand::Interrupt {
            target_turn,
            respond,
        } => {
            let interrupted = state.running.as_ref().is_some_and(|running| {
                if running.id != target_turn {
                    return false;
                }
                running.cancellation.cancel();
                true
            });
            let _ = respond.send(interrupted);
        }
        ActorCommand::CompleteUserShell {
            shell_id,
            status,
            captured_output,
            respond,
        } => {
            let captured_output =
                captured_output.map(|output| config.secret_redactor.redact(&output));
            let result = if captured_output
                .as_ref()
                .is_some_and(|output| output.len() > MAX_CAPTURED_SHELL_OUTPUT_BYTES)
            {
                Err(AgentLoopError::InvalidConfiguration(
                    "captured foreground-shell output exceeds the durable limit".to_owned(),
                ))
            } else if state
                .active_shell
                .as_ref()
                .is_none_or(|active| active.shell_id != shell_id)
            {
                Err(AgentLoopError::InvalidConfiguration(
                    "foreground-shell completion does not match the active shell id".to_owned(),
                ))
            } else {
                let command = state
                    .active_shell
                    .as_ref()
                    .map(|active| active.command.clone())
                    .unwrap_or_default();
                let context = shell_context_turn(&command, status, captured_output.as_deref());
                let persisted = emit(
                    state,
                    events,
                    &config.event_sink,
                    PendingEvent::UserShellStateChanged {
                        shell_id,
                        command,
                        active: false,
                        status: Some(status),
                        captured_output,
                    },
                )
                .await
                .map(|_| ());
                if persisted.is_ok() {
                    state.append_conversation(context);
                    state.active_shell = None;
                }
                persisted
            };
            let _ = respond.send(result);
        }
        ActorCommand::RecordSubagentSpawned {
            subagent_id,
            child_session_id,
            task,
            respond,
        } => {
            let result = emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::SubagentSpawned {
                    subagent_id,
                    child_session_id,
                    task,
                },
            )
            .await
            .map(|_| ());
            let _ = respond.send(result);
        }
        ActorCommand::RecordSubagentFinished { result, respond } => {
            let subagent_id = result.subagent_id.clone();
            let result = emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::SubagentFinished {
                    subagent_id,
                    result,
                },
            )
            .await
            .map(|_| ());
            let _ = respond.send(result);
        }
        ActorCommand::PublishSubagentProgressBatch { progress, respond } => {
            for progress in progress {
                let _ = events.send(RoutedEvent {
                    target: None,
                    event: EngineEvent::SubagentProgress {
                        parent_session_id: state.session_id.clone(),
                        subagent_id: progress.subagent_id,
                        child_session_id: progress.child_session_id,
                        child_sequence: progress.child_sequence.map(SequenceId),
                        event: progress.event,
                    },
                });
            }
            let _ = respond.send(Ok(()));
        }
        ActorCommand::UiCatalog { respond } => {
            let _ = respond.send(config.ui.catalog());
        }
        ActorCommand::UiPanels { respond } => {
            let _ = respond.send(config.ui.panels());
        }
        ActorCommand::Snapshot { respond } => {
            let _ = respond.send(SessionSnapshot {
                conversation_turns: state.conversation.len() as u64,
                resolved_model: state.resolved_model.clone(),
                queued_messages: state.queued.iter().cloned().collect(),
                running: state.running.is_some(),
                completed_turns: state.completed_turns,
                model_alias: state.model_alias.clone(),
                provider: state.provider.clone(),
                thinking: state.thinking,
                mode: state.mode,
                mode_id: state.mode_id.clone(),
                permission_mode: config.permissions.snapshot().runtime_mode,
                pending_plan: state.pending_plan.clone(),
                approved_plan: state.approved_plan.clone(),
                plan_gate_active: state.plan_gate_active,
                active_shell: state.active_shell.clone(),
                active_background: config.tools.session_activity(&state.session_id).is_some(),
                workspace_generation: config.workspace_generation,
                workspace_roots: std::iter::once(&config.workspace_root)
                    .chain(&config.additional_workspace_roots)
                    .enumerate()
                    .map(|(index, _root)| rw_types::WorkspaceRootDescriptor {
                        index: u32::try_from(index).unwrap_or(u32::MAX),
                        path: format!("@root/{index}"),
                        machine_local: false,
                    })
                    .collect(),
                driver_client_id: state.control.driver().clone(),
            });
        }
    }
}

#[cfg(test)]
pub(super) use permissions::permission_state;

pub(in crate::engine) use message_input::{recover_user_message, redact_prepared_message};
