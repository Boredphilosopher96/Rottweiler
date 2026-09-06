//! Read-only context work retains its source and runtime until publication.
use super::replies::{query_meta, send_connection_event};
use crate::engine::{
    AgentLoopError, MessageDisposition, RoutedEvent,
    pending_event::PendingEvent,
    recovery::HistoryRead,
    session::{ActorState, ProtocolCompletion, SessionActorConfig},
    turn::{context_snapshot, emit, history_context, prompt_dump},
};
use rw_types::{CommandMeta, EngineEvent, ModeId, TurnId, config::ThinkingLevel};
use std::{collections::VecDeque, sync::Arc};
use tokio::sync::{broadcast, oneshot};

type Completion = oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>;
type PluginReply =
    oneshot::Sender<Result<rw_types::extension_control::ExtensionContextPage, AgentLoopError>>;

pub(in crate::engine) enum Target {
    Context {
        completion: Option<Completion>,
    },
    Prompt {
        turn: Option<TurnId>,
        completion: Option<Completion>,
    },
    Plugin {
        request: rw_types::extension_control::ExtensionContextRead,
        reply: PluginReply,
    },
    Command {
        meta: CommandMeta,
        reply: super::command_job::CommandReply,
    },
}
impl Target {
    fn reject(self, error: AgentLoopError) {
        match self {
            Self::Context { completion, .. } | Self::Prompt { completion, .. } => {
                if let Some(reply) = completion {
                    let _ = reply.send(Err(error));
                }
            }
            Self::Plugin { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Command { reply, .. } => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

pub(in crate::engine) enum Output {
    Context(rw_types::ContextSnapshot),
    Prompt(rw_types::PromptDump),
    Plugin(rw_types::extension_control::ExtensionContextPage),
}
pub(in crate::engine) type ReadResult = Result<HistoryRead<Output>, AgentLoopError>;
pub(in crate::engine) struct PendingRead {
    owner: Arc<SessionActorConfig>,
    model: String,
    provider: Option<String>,
    thinking: ThinkingLevel,
    mode: ModeId,
    target: Target,
    receive: oneshot::Receiver<ReadResult>,
}

pub(in crate::engine) fn start(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    target: Target,
) {
    if state.pending_context_read.is_some() || state.closing {
        target.reject(invalid("context read admission is busy"));
        return;
    }
    let owner = Arc::clone(config);
    let tasks = state.tasks.clone();
    let requested_turn = match &target {
        Target::Prompt { turn, .. } => turn.clone(),
        _ => None,
    };
    let dump = matches!(target, Target::Prompt { .. });
    let plugin = match &target {
        Target::Plugin { request, .. } => Some(request.clone()),
        _ => None,
    };
    let (send, receive) = oneshot::channel();
    let spawned = state.tasks.spawn(
        Arc::clone(config),
        rw_tools::CancellationToken::default(),
        async move {
            let result = read(owner, tasks, requested_turn, dump, plugin).await;
            let _ = send.send(result);
        },
    );
    match spawned {
        Ok(_) => {
            state.pending_context_read = Some(PendingRead {
                owner: Arc::clone(config),
                model: state.model_alias.clone(),
                provider: state.provider.clone(),
                thinking: state.thinking,
                mode: state.mode_id.clone(),
                target,
                receive,
            });
        }
        Err(error) => target.reject(error),
    }
}

async fn read(
    config: Arc<SessionActorConfig>,
    tasks: crate::engine::task_ownership::ActorTasks,
    requested_turn: Option<TurnId>,
    dump: bool,
    plugin: Option<rw_types::extension_control::ExtensionContextRead>,
) -> ReadResult {
    if let Some(turn) = requested_turn {
        return historical_prompt(config, tasks, turn).await;
    }
    let view = config.history.capture_history().await?;
    let bootstrap = view.bootstrap().await?;
    let active_turn = bootstrap
        .head
        .control
        .active
        .as_ref()
        .map(|active| crate::engine::wire_turn_id(active.turn));
    let current = bootstrap
        .map_async(|bootstrap| async {
            let queued: VecDeque<_> = bootstrap
                .controls
                .queued_messages
                .into_iter()
                .map(|(_, message)| message.content)
                .collect();
            history_context::assemble_view(Arc::clone(&config), &tasks, view, queued).await
        })
        .await
        .try_map(|result| result)?
        .flatten();
    tasks
        .spawn_blocking(
            Arc::clone(&config),
            rw_tools::CancellationToken::default(),
            rw_resources::ResourceClass::Cpu,
            move || {
                // Both materializations remain owned through transformation and output delivery.
                current.try_map(|current| {
                    if let Some(request) = plugin {
                        super::plugin_control::read_context(&current, &request).map(Output::Plugin)
                    } else if dump {
                        Ok(Output::Prompt(prompt_dump(
                            current.assembled,
                            &config.model_alias,
                            None,
                            current.through,
                        )))
                    } else {
                        Ok(Output::Context(context_snapshot(
                            &current.assembled,
                            crate::engine::turn::context::ContextSnapshotSource {
                                conversation: &current.conversation,
                                sources: &current.sources,
                                pruned: &current.pruned_tool_outputs,
                            },
                            config.model.context_metadata(&config.model_alias),
                            &config.model.compaction_config(),
                            active_turn,
                            current.through,
                        )))
                    }
                })
            },
        )
        .await?
        .await
        .map_err(|error| invalid(&format!("context result worker failed: {error}")))?
}

async fn historical_prompt(
    config: Arc<SessionActorConfig>,
    tasks: crate::engine::task_ownership::ActorTasks,
    turn: TurnId,
) -> ReadResult {
    let turn_number = crate::engine::projection::parse_turn_id(&turn)
        .map_err(|error| invalid(&error.to_string()))?;
    let view = config
        .history
        .capture_history()
        .await?
        .prompt_at_turn(turn_number)
        .await?;
    let bootstrap = view.bootstrap().await?;
    let verification = Arc::clone(&view);
    let current = bootstrap
        .map_async(|bootstrap| async {
            let queued = bootstrap
                .controls
                .queued_messages
                .into_iter()
                .map(|(_, message)| message.content)
                .collect();
            history_context::assemble_view(Arc::clone(&config), &tasks, view, queued).await
        })
        .await
        .try_map(|result| result)?
        .flatten();
    tasks
        .spawn_blocking(
            Arc::clone(&config),
            rw_tools::CancellationToken::default(),
            rw_resources::ResourceClass::Cpu,
            move || {
                current.try_map(|current| {
                    let dump = prompt_dump(
                        current.assembled,
                        &config.model_alias,
                        Some(turn),
                        current.through,
                    );
                    verification.verify_prompt(turn_number, &dump)?;
                    Ok(Output::Prompt(dump))
                })
            },
        )
        .await?
        .await
        .map_err(|error| invalid(&format!("historical prompt worker failed: {error}")))?
}

pub(in crate::engine) async fn wait(pending: &mut Option<PendingRead>) -> ReadResult {
    match pending {
        Some(pending) => (&mut pending.receive)
            .await
            .unwrap_or_else(|_| Err(invalid("context owner exited without a result"))),
        None => std::future::pending().await,
    }
}

pub(in crate::engine) async fn finish(
    mut result: ReadResult,
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    events: &broadcast::Sender<RoutedEvent>,
) {
    let Some(pending) = state.pending_context_read.take() else {
        return;
    };
    if state.closing
        || state.poisoned
        || state.unsettled.is_some()
        || !Arc::ptr_eq(&pending.owner, config)
        || pending.model != state.model_alias
        || pending.provider != state.provider
        || pending.thinking != state.thinking
        || pending.mode != state.mode_id
    {
        result = Err(invalid("context runtime generation changed; read again"));
    }
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            pending.target.reject(error);
            return;
        }
    };
    match pending.target {
        Target::Plugin { reply, .. } => {
            let result = match &*output {
                Output::Plugin(page) => Ok(page.clone()),
                _ => Err(invalid("context request result mismatch")),
            };
            let _ = reply.send(result);
        }
        Target::Context { completion, .. } => {
            if let Some(reply) = completion {
                let result = output.try_map(|output| match output {
                    Output::Context(snapshot) => Ok(snapshot),
                    _ => Err(invalid("context request result mismatch")),
                });
                let _ = reply.send(result.map(ProtocolCompletion::Context));
            }
        }
        Target::Prompt { completion, .. } => {
            if let Some(reply) = completion {
                let result = output.try_map(|output| match output {
                    Output::Prompt(dump) => Ok(dump),
                    _ => Err(invalid("prompt request result mismatch")),
                });
                let _ = reply.send(result.map(ProtocolCompletion::Prompt));
            }
        }
        Target::Command { meta, reply } => {
            if state.control.driver().as_ref() != Some(&meta.client_id) {
                let _ = reply.send(Err(invalid("context command driver changed")));
                return;
            }
            let Output::Context(snapshot) = &*output else {
                let _ = reply.send(Err(invalid("context command result mismatch")));
                return;
            };
            send_connection_event(
                events,
                &meta.client_id,
                EngineEvent::ContextSnapshotReady {
                    meta: query_meta(state, &meta),
                    session_id: state.session_id.clone(),
                    snapshot: snapshot.clone(),
                },
            );
            let previous = state.transient_cause.replace(meta.request_id);
            let result = emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::CommandFinished {
                    name: "context".into(),
                    message: crate::engine::commands::render_context_snapshot(snapshot),
                    unrestorable_paths: Vec::new(),
                },
            )
            .await
            .map(|_| MessageDisposition::Command);
            state.transient_cause = previous;
            let _ = reply.send(result);
        }
    }
}

fn invalid(message: &str) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(message.into())
}
