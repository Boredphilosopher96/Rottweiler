use crate::engine::AgentLoopError;
use crate::engine::AgentTurnStatus;
use crate::engine::PreparedUserMessage;
use crate::engine::RoutedEvent;
use crate::engine::SessionUsage;
use crate::engine::commands::CommandToolCall;
use crate::engine::dispatch::prepare_user_message;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::ActorState;
use crate::engine::session::SessionActorConfig;
use crate::engine::turn::accounting::session_accounting_fallback;
use crate::engine::turn::journal_events::emit_batch;
use crate::engine::turn::run::RunningTurn;
use crate::engine::turn::run::run_turn;
use crate::engine::turn::signals::TurnOutcome;
use crate::engine::turn::signals::TurnSignal;
use crate::engine::turn::tool_requests::ActorQuestionAsker;
use crate::engine::unavailable_cost;
use futures_util::FutureExt;
use rw_ext::HookEvent;
use rw_tools::CancellationToken;
use rw_tools::QuestionAsker;
use rw_tools::ToolContext;
use rw_types::Attachment;
use rw_types::Turn;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

#[derive(Default)]
pub(in crate::engine) struct CommandTurnOverrides {
    pub(in crate::engine) model_alias: Option<String>,
    pub(in crate::engine) allowed_tools: Option<Vec<String>>,
    pub(in crate::engine) permission_patterns: Vec<String>,
    pub(in crate::engine) tool_calls: Vec<CommandToolCall>,
}

#[derive(Clone, Copy)]
pub(in crate::engine) struct StartTurnRuntime<'a> {
    pub(in crate::engine) config: &'a Arc<SessionActorConfig>,
    pub(in crate::engine) tool_context: &'a ToolContext,
    pub(in crate::engine) signals: &'a mpsc::UnboundedSender<TurnSignal>,
    pub(in crate::engine) events: &'a broadcast::Sender<RoutedEvent>,
    pub(in crate::engine) active_turn: &'a Arc<AtomicU64>,
}

pub(super) struct AcceptedUserMessage {
    pub(super) source: rw_types::SequenceId,
    pub(super) original_content: String,
    pub(super) message: PreparedUserMessage,
}

pub(super) struct PreparedTurnStart {
    pub(super) config: Arc<SessionActorConfig>,
    pub(super) messages: Vec<PreparedUserMessage>,
    pub(super) tool_calls: Vec<CommandToolCall>,
}

pub(in crate::engine) async fn start_turn(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    tool_context: &ToolContext,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    events: &broadcast::Sender<RoutedEvent>,
    messages: Vec<(String, Vec<Attachment>)>,
    active_turn: &Arc<AtomicU64>,
) -> Result<(), AgentLoopError> {
    start_turn_with_overrides(
        state,
        StartTurnRuntime {
            config,
            tool_context,
            signals,
            events,
            active_turn,
        },
        messages,
        CommandTurnOverrides::default(),
    )
    .await
}

pub(super) fn prepare_turn_start(
    state: &ActorState,
    config: &Arc<SessionActorConfig>,
    messages: Vec<(String, Vec<Attachment>)>,
    overrides: CommandTurnOverrides,
) -> Result<PreparedTurnStart, AgentLoopError> {
    let CommandTurnOverrides {
        model_alias,
        allowed_tools,
        permission_patterns,
        tool_calls,
    } = overrides;
    let model_alias = model_alias
        .as_deref()
        .unwrap_or(&state.model_alias)
        .to_owned();
    let provider = (model_alias == state.model_alias)
        .then(|| state.provider.clone())
        .flatten();
    let mut turn_config =
        config.with_model_route_and_mode(model_alias.clone(), provider, &state.mode_id);
    turn_config.thinking = state.thinking;
    let mode = config.modes.get(&state.mode_id.0).ok_or_else(|| {
        AgentLoopError::InvalidConfiguration(format!("unknown active mode {:?}", state.mode_id.0))
    })?;
    if !mode.allowed_tools().is_empty() {
        turn_config.tools = Arc::new(
            turn_config
                .tools
                .subset(mode.allowed_tools().iter().map(String::as_str))
                .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?,
        );
    }
    if let Some(allowed_tools) = allowed_tools {
        turn_config.tools = Arc::new(
            turn_config
                .tools
                .subset(allowed_tools.iter().map(String::as_str))
                .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?,
        );
    }
    if !permission_patterns.is_empty() {
        turn_config.permissions = Arc::new(
            config
                .permissions
                .restricted_to_patterns(&permission_patterns)
                .map_err(AgentLoopError::InvalidConfiguration)?,
        );
    }
    let messages = messages
        .into_iter()
        .map(|(content, attachments)| {
            prepare_user_message(&content, &attachments, &model_alias, config.model.as_ref())
                .and_then(|message| message.redact(config.secret_redactor.as_ref()))
                .map_err(AgentLoopError::InvalidConfiguration)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PreparedTurnStart {
        config: Arc::new(turn_config),
        messages,
        tool_calls,
    })
}

pub(super) fn prepare_turn_opening(
    turn: u64,
    messages: &[PreparedUserMessage],
    first_accepted: rw_types::SequenceId,
    synchronous: bool,
    conversation: &mut Vec<Turn>,
) -> Vec<PendingEvent> {
    let capacity = if synchronous {
        messages.len().saturating_mul(2).saturating_add(1)
    } else {
        messages.len().saturating_add(1)
    };
    let mut events = Vec::with_capacity(capacity);
    events.push(PendingEvent::TurnStarted { turn });
    events.extend(
        messages
            .iter()
            .map(|message| PendingEvent::UserMessageAccepted {
                turn,
                content: message.content.clone(),
                attachments: message.stored_attachments.clone(),
            }),
    );
    if synchronous {
        for (index, message) in messages.iter().enumerate() {
            let user_turn = message.turn(message.content.clone());
            events.push(PendingEvent::ConversationInputCommitted {
                agent_turn: turn,
                accepted_source: rw_types::SequenceId(first_accepted.0 + index as u64),
                selection: rw_types::conversation_input::InputSelection::Accepted {},
            });
            conversation.push(user_turn);
        }
    }
    events
}

#[allow(clippy::too_many_lines)]
pub(in crate::engine) async fn start_turn_with_overrides(
    state: &mut ActorState,
    runtime: StartTurnRuntime<'_>,
    messages: Vec<(String, Vec<Attachment>)>,
    overrides: CommandTurnOverrides,
) -> Result<(), AgentLoopError> {
    let PreparedTurnStart {
        config,
        messages,
        tool_calls,
    } = prepare_turn_start(state, runtime.config, messages, overrides)?;
    let history =
        super::history_context::capture(runtime.config, state.sequence.map(rw_types::SequenceId))
            .await?;
    let turn = state.next_turn;
    let native_search = config.model.native_web_searcher(
        &config.model_alias,
        super::provider_calls::binding(
            &config,
            runtime.signals,
            turn,
            rw_types::AccountingAttribution::Main,
            0,
        )?,
    );

    state.next_turn = state.next_turn.saturating_add(1);
    let cancellation = CancellationToken::default();
    state.running = Some(RunningTurn {
        id: turn,
        cancellation: cancellation.clone(),
        caused_by: state.transient_cause.clone(),
    });
    state.control.start(turn, cancellation.clone());
    runtime.active_turn.store(turn, Ordering::Release);
    let prepare_users_synchronously = runtime
        .config
        .hooks
        .registrations(HookEvent::UserPromptSubmit)
        .len()
        == 0
        && tool_calls.is_empty();
    let first_accepted = state
        .sequence
        .map_or(Some(1), |sequence| sequence.checked_add(2))
        .filter(|first| first.checked_add(messages.len() as u64).is_some())
        .map(rw_types::SequenceId)
        .ok_or_else(|| AgentLoopError::Persistence("input source sequence overflow".into()))?;
    let mut opening_conversation = Vec::new();
    let opening_events = prepare_turn_opening(
        turn,
        &messages,
        first_accepted,
        prepare_users_synchronously,
        &mut opening_conversation,
    );
    if let Err(error) = emit_batch(
        state,
        runtime.events,
        &runtime.config.event_sink,
        opening_events,
    )
    .await
    {
        state.control.finish(turn);
        state.running = None;
        runtime.active_turn.store(0, Ordering::Release);
        return Err(error);
    }
    let panic_conversation = state.conversation_summary();
    let run_messages = if prepare_users_synchronously {
        Vec::new()
    } else {
        messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| AcceptedUserMessage {
                source: rw_types::SequenceId(first_accepted.0 + index as u64),
                original_content: message.content.clone(),
                message,
            })
            .collect()
    };
    let protocol_asker: Arc<dyn QuestionAsker> = Arc::new(ActorQuestionAsker::new(
        runtime.signals.clone(),
        cancellation.clone(),
    ));
    let tool_context = runtime
        .tool_context
        .clone()
        .with_cancellation(cancellation.clone())
        .with_question_asker(protocol_asker)
        .with_model_alias(config.model_alias.clone())
        .with_native_searcher(native_search)
        .with_todo_store(Arc::new(super::todos::ActorTodoStore::new(
            turn,
            runtime.signals.clone(),
        )));
    let signals = runtime.signals.clone();
    let state_budgeter = state.budgeter;
    let local_session_accounting = session_accounting_fallback(&state.accounting);
    let state_mode = state.mode;
    let provider_owner = Arc::clone(&config.model);
    let tasks = state.tasks.clone();
    let turn_tasks = tasks.clone();
    tasks.spawn(Arc::clone(&config), cancellation.clone(), async move {
        let fallback = panic_conversation.clone();
        let outcome = AssertUnwindSafe(async {
            let page = history
                .conversation_page(
                    0..history.conversation().turns,
                    crate::engine::recovery::HistoryMaterializationLimits::default(),
                )
                .await?;
            let (page, opening_spend) = if page.has_more {
                drop(page);
                super::history_compaction::compact(
                    history,
                    opening_conversation,
                    &config,
                    &cancellation,
                    &signals,
                    turn,
                    rw_types::CompactionReason::Automatic,
                    None,
                    local_session_accounting,
                )
                .await?
            } else {
                (
                    page.map(|mut page| {
                        page.turns.extend(opening_conversation);
                        page
                    }),
                    super::accounting::BudgetUsage::default(),
                )
            };
            let run_signals = signals.clone();
            let completed = page
                .map_async(|page| async move {
                    run_turn(
                        turn,
                        turn_tasks,
                        run_messages,
                        tool_calls,
                        page.turns,
                        config,
                        tool_context,
                        cancellation,
                        run_signals,
                        page.context_actions.into_iter().flatten().collect(),
                        page.pruned_tool_outputs,
                        state_budgeter,
                        local_session_accounting,
                        state_mode,
                        opening_spend,
                    )
                    .await
                })
                .await;
            // Completion contains scalar metadata; release historical bodies and
            // their admitted source owner before the actor receives the result.
            let mut outcome = None;
            drop(completed.map(|result| {
                outcome = Some(result);
            }));
            outcome.ok_or_else(|| AgentLoopError::Persistence("missing owned turn outcome".into()))
        })
        .catch_unwind()
        .await;
        let outcome = match outcome {
            Ok(Ok(outcome)) => outcome,
            failed => {
                let message = match failed {
                    Ok(Err(error)) => error.to_string(),
                    Err(_) => {
                        let message = "turn owner panicked before effect settlement".to_owned();
                        let _ = signals.send(TurnSignal::EffectsUnsettled {
                            message: message.clone(),
                        });
                        message
                    }
                    Ok(Ok(_)) => unreachable!(),
                };
                let _ = super::provider_messages::persist_event(
                    &signals,
                    PendingEvent::Error { message },
                )
                .await;
                TurnOutcome {
                    turn,
                    conversation: fallback,
                    status: AgentTurnStatus::Failed,
                    usage: SessionUsage::default(),
                    cost: unavailable_cost(),
                    deferred_terminal_delta: None,
                    deferred_terminal_turn: None,
                    budgeter: state_budgeter,
                }
            }
        };
        if let Err(error) = provider_owner.settle_effects().await {
            let _ = signals.send(TurnSignal::EffectsUnsettled {
                message: error.to_string(),
            });
        }
        let _ = signals.send(TurnSignal::Complete(outcome));
    })?;
    Ok(())
}
