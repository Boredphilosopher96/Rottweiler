use crate::PermissionRequest;
use crate::engine::AgentLoopError;
use crate::engine::AgentTurnStatus;
use crate::engine::RoutedEvent;
use crate::engine::SessionUsage;
use crate::engine::diff_binding;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::ActorState;
use crate::engine::session::PendingApproval;
use crate::engine::session::PendingQuestion;
use crate::engine::session::PreparedModelSwitch;
use crate::engine::session::ProtocolCompletion;
use crate::engine::session::SessionActorConfig;
use crate::engine::turn::journal_events::emit;
use crate::engine::turn::journal_events::emit_batch;
use crate::engine::turn::progress::ProgressSlot;
use crate::engine::turn::title::normalize_generated_session_title;
use crate::engine::turn::title::start_session_title_generation;
use crate::engine::wire_turn_id;
use rw_context::Budgeter;
use rw_tools::AskUserInput;
use rw_types::AccountingAttribution;
use rw_types::ApprovalDecision;
use rw_types::Cost;
use rw_types::EngineEvent;
use rw_types::EventMeta;
use rw_types::Question;
use rw_types::QuestionId;
use rw_types::QuestionOption;
use rw_types::QuestionResponseKind;
use rw_types::SequenceId;
use rw_types::ToolCallId;
use rw_types::Turn;
use rw_types::TurnAccounting;
use rw_types::TurnId;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

#[allow(clippy::too_many_lines)]
pub(in crate::engine) async fn handle_turn_signal(
    signal: TurnSignal,
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    turn_signals: &mpsc::UnboundedSender<TurnSignal>,
    events: &broadcast::Sender<RoutedEvent>,
    active_turn: &Arc<AtomicU64>,
) -> Result<(), AgentLoopError> {
    match signal {
        TurnSignal::Todo(request) => super::todos::handle(request, state, config, events).await?,
        TurnSignal::Event(event) | TurnSignal::ToolOutput { event, .. } => {
            let Some(running_turn) = state.running.as_ref().map(|running| running.id) else {
                return Ok(());
            };
            if event
                .active_turn()
                .is_some_and(|event_turn| event_turn != running_turn)
            {
                return Ok(());
            }
            let submitted_plan = match &event {
                PendingEvent::PlanSubmitted { artifact } => {
                    rw_types::session_controls::validate_plan(artifact)
                        .map_err(|error| AgentLoopError::InvalidConfiguration(error.into()))?;
                    Some(artifact.clone())
                }
                _ => None,
            };
            emit(state, events, &config.event_sink, event).await?;
            if let Some(artifact) = submitted_plan {
                state.pending_plan = Some(artifact);
            }
        }
        TurnSignal::DurableEvent { kind, respond } => {
            let compaction_accounting = match &kind {
                PendingEvent::CompactionAttemptFinished {
                    summary_turn,
                    usage,
                    cost,
                }
                | PendingEvent::CompactionFinished {
                    summary_turn,
                    usage: Some(usage),
                    cost: Some(cost),
                    ..
                } => Some(TurnAccounting {
                    turn_id: wire_turn_id(*summary_turn),
                    attribution: AccountingAttribution::Compaction,
                    usage: (*usage).into(),
                    cost: cost.clone(),
                }),
                _ => None,
            };
            let result = emit(state, events, &config.event_sink, kind).await;
            if result.is_ok()
                && let Some(accounting) = compaction_accounting
            {
                state.accounting.record(&accounting);
            }
            let _ = respond.send(result.clone());
            result?;
        }
        TurnSignal::ToolProgress(slot) => {
            if state.running.as_ref().map(|running| running.id) != Some(slot.turn) {
                return Ok(());
            }
            if let Some(progress) = slot.take() {
                let _ = events.send(RoutedEvent {
                    target: state.control.driver().clone(),
                    event: EngineEvent::ToolProgress {
                        session_id: state.session_id.clone(),
                        turn_id: wire_turn_id(slot.turn),
                        tool_call_id: ToolCallId(slot.id.clone()),
                        invocation_id: slot.invocation_id.clone(),
                        progress,
                    },
                });
            }
        }
        TurnSignal::SubagentProgress(slot) => {
            let Some(admitted) = slot.take() else {
                return Ok(());
            };
            let progress = admitted.event;
            let event = EngineEvent::SubagentProgress {
                parent_session_id: state.session_id.clone(),
                subagent_id: progress.subagent_id,
                child_session_id: progress.child_session_id,
                child_sequence: progress.child_sequence.map(SequenceId),
                event: progress.event,
            };
            let _ = events.send(RoutedEvent {
                target: state.control.driver().clone(),
                event,
            });
        }
        TurnSignal::CompactionProgress(progress) => {
            if state.running.as_ref().map(|running| running.id) != Some(progress.summary_turn) {
                return Ok(());
            }
            let update = match &progress.kind {
                CompactionProgressKind::AttemptStarted => {
                    crate::engine::session::CompactionPreview::Started
                }
                CompactionProgressKind::Text(text) => {
                    crate::engine::session::CompactionPreview::Text(text)
                }
                CompactionProgressKind::Thinking(text) => {
                    crate::engine::session::CompactionPreview::Thinking(text)
                }
            };
            let Some((started, revision)) =
                state
                    .live
                    .compaction_progress(progress.summary_turn, progress.attempt, update)?
            else {
                return Ok(());
            };
            let event = match progress.kind {
                CompactionProgressKind::AttemptStarted => EngineEvent::CompactionAttemptStarted {
                    started,
                    revision,
                    session_id: state.session_id.clone(),
                    summary_turn_id: wire_turn_id(progress.summary_turn),
                    attempt: progress.attempt,
                },
                CompactionProgressKind::Text(text) => EngineEvent::CompactionTextDelta {
                    started,
                    revision,
                    session_id: state.session_id.clone(),
                    summary_turn_id: wire_turn_id(progress.summary_turn),
                    attempt: progress.attempt,
                    text,
                },
                CompactionProgressKind::Thinking(text) => EngineEvent::CompactionThinkingDelta {
                    started,
                    revision,
                    session_id: state.session_id.clone(),
                    summary_turn_id: wire_turn_id(progress.summary_turn),
                    attempt: progress.attempt,
                    text,
                },
            };
            let _ = events.send(RoutedEvent {
                target: state.control.driver().clone(),
                event,
            });
        }
        TurnSignal::Approval { request, respond } => {
            let Some(turn) = state.running.as_ref().map(|running| running.id) else {
                let _ = respond.send(ApprovalDecision::Deny);
                return Ok(());
            };
            let binding = request.approval_diff.as_ref().map(diff_binding);
            if let Some(previous) = state.pending_approvals.insert(
                request.id.clone(),
                PendingApproval {
                    respond,
                    binding,
                    request: request.clone(),
                    turn,
                },
            ) {
                let _ = previous.respond.send(ApprovalDecision::Deny);
            }
            emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::PermissionRequested { turn, request },
            )
            .await?;
        }
        TurnSignal::Question {
            request,
            respond,
            admission,
        } => {
            let Some(turn) = state.running.as_ref().map(|running| running.id) else {
                let _ = respond.send(Err(rw_tools::ToolError::Cancelled));
                return Ok(());
            };
            let question_id = QuestionId(format!("question-{turn}-{}", state.next_question));
            state.next_question = state.next_question.saturating_add(1);
            let response_kind = if request.options.is_empty() {
                QuestionResponseKind::Text
            } else {
                QuestionResponseKind::SelectOne
            };
            let question = Question {
                id: question_id.clone(),
                prompt: request.question,
                response_kind,
                options: request
                    .options
                    .into_iter()
                    .map(|value| QuestionOption {
                        label: value.clone(),
                        value,
                        description: None,
                        model_context_transfer: None,
                    })
                    .collect(),
                model_switch: None,
            };
            let questions = vec![question];
            let validation = rw_types::question_admission::validate_questions(&questions);
            if let Err(error) = validation {
                let _ = respond.send(Err(rw_tools::ToolError::InvalidInput(error.into())));
                return Ok(());
            }
            if state.pending_questions.len() + state.pending_model_switches.len()
                >= rw_types::question_admission::MAX_PENDING_QUESTION_REQUESTS
            {
                let _ = respond.send(Err(rw_tools::ToolError::InvalidInput(
                    "pending question admission is full".into(),
                )));
                return Ok(());
            }
            state.pending_questions.insert(
                question_id.0.clone(),
                PendingQuestion {
                    questions: questions.clone(),
                    turn,
                    respond,
                    _admission: admission,
                },
            );
            emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::QuestionAsked {
                    turn,
                    question_id,
                    questions,
                },
            )
            .await?;
        }
        TurnSignal::InitializationComplete { name, result } => {
            state.initialization_running = false;
            let message = match result {
                Ok(message) => message,
                Err(error) => {
                    let message = config.secret_redactor.redact(&error.to_string());
                    emit(
                        state,
                        events,
                        &config.event_sink,
                        PendingEvent::Error {
                            message: message.clone(),
                        },
                    )
                    .await?;
                    format!("workspace initialization failed: {message}")
                }
            };
            emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::CommandFinished {
                    name: name.to_owned(),
                    message,
                    unrestorable_paths: Vec::new(),
                },
            )
            .await?;
        }
        TurnSignal::SessionTitleGenerated { title, usage, cost } => {
            if state.session_title.is_none() {
                let title = config.secret_redactor.redact(&title);
                if let Some(title) = normalize_generated_session_title(&title) {
                    emit(
                        state,
                        events,
                        &config.event_sink,
                        PendingEvent::SessionTitleUpdated {
                            title: title.clone(),
                            usage,
                            cost: cost.clone(),
                        },
                    )
                    .await?;
                    state.session_title = Some(title);
                    if let (Some(usage), Some(cost)) = (usage, cost) {
                        state.accounting.record(&TurnAccounting {
                            turn_id: TurnId("title".to_owned()),
                            attribution: AccountingAttribution::Title,
                            usage: usage.into(),
                            cost,
                        });
                    }
                } else {
                    state.title_generation_started = false;
                }
            }
        }
        TurnSignal::EffectsUnsettled { message } => {
            state.tasks.cancel();
            state.poisoned = true;
            state.unsettled = Some(message.clone());
            emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::Error {
                    message: config
                        .secret_redactor
                        .redact(&format!("effect settlement is unproven: {message}")),
                },
            )
            .await?;
        }
        TurnSignal::PluginToolComplete { turn, result } => {
            super::plugin_tool::finish(turn, result, state, config, events, active_turn).await?;
        }
        TurnSignal::Complete(outcome) => {
            if state.running.as_ref().map(|running| running.id) != Some(outcome.turn) {
                return Ok(());
            }
            let completed_successfully = outcome.status == AgentTurnStatus::Completed;
            state.control.finish(outcome.turn);
            state.running = None;
            active_turn.store(0, Ordering::Release);
            if state.unsettled.is_some() {
                return Ok(());
            }
            state.pending_approvals.clear();
            for (_, pending) in std::mem::take(&mut state.pending_questions) {
                let _ = pending.respond.send(Err(rw_tools::ToolError::Cancelled));
            }
            state.replace_conversation(outcome.conversation);
            state.budgeter = outcome.budgeter;
            state.accounting.record(&TurnAccounting {
                turn_id: wire_turn_id(outcome.turn),
                attribution: AccountingAttribution::Main,
                usage: outcome.usage.into(),
                cost: outcome.cost.clone(),
            });
            state.completed_turns = state.completed_turns.saturating_add(1);
            let mut terminal_events = Vec::with_capacity(3);
            if let Some(text) = outcome.deferred_terminal_delta {
                terminal_events.push(PendingEvent::TextDelta {
                    turn: outcome.turn,
                    text,
                });
            }
            if let Some(assistant_turn) = outcome.deferred_terminal_turn {
                terminal_events.push(PendingEvent::ConversationTurnCommitted {
                    agent_turn: outcome.turn,
                    turn: assistant_turn,
                });
            }
            terminal_events.push(PendingEvent::TurnFinished {
                turn: outcome.turn,
                status: outcome.status,
                usage: outcome.usage,
                cost: outcome.cost,
            });
            emit_batch(state, events, &config.event_sink, terminal_events).await?;
            if completed_successfully && !state.closing {
                start_session_title_generation(state, config, turn_signals);
            }
        }
        TurnSignal::ManualCompactionComplete {
            turn,
            conversation,
            mut result,
            model_switch,
            completion,
        } => {
            if let Some(message) = &state.unsettled {
                result = Err(AgentLoopError::EffectsUnsettled(message.clone()));
            }
            if state.running.as_ref().map(|running| running.id) == Some(turn) {
                state.control.finish(turn);
                state.running = None;
                active_turn.store(0, Ordering::Release);
                if result.is_ok() {
                    state.replace_conversation(conversation);
                    if let Some(model_switch) = model_switch {
                        crate::engine::dispatch::model_job::start(
                            state,
                            config,
                            events,
                            model_switch.model.0.clone(),
                            crate::engine::dispatch::model_job::SelectionAction::Commit {
                                prepared: model_switch,
                                clear_context: false,
                                completion,
                            },
                        );
                        return Ok(());
                    }
                }
            }
            if let Some(completion) = completion {
                let _ = completion.send(result.map(|()| ProtocolCompletion::Unit));
            }
        }
    }
    Ok(())
}

pub(super) enum CompactionProgressKind {
    AttemptStarted,
    Text(String),
    Thinking(String),
}

pub(in crate::engine) struct CompactionProgress {
    pub(super) summary_turn: u64,
    pub(super) attempt: u32,
    pub(super) kind: CompactionProgressKind,
}

pub(in crate::engine) enum TurnSignal {
    PluginToolComplete {
        turn: u64,
        result: Result<super::tool_requests::ToolExecution, AgentLoopError>,
    },
    Todo(super::todos::TodoRequest),
    EffectsUnsettled {
        message: String,
    },
    Event(PendingEvent),
    ToolOutput {
        event: PendingEvent,
        _permit: OwnedSemaphorePermit,
    },
    DurableEvent {
        kind: PendingEvent,
        respond: oneshot::Sender<Result<EventMeta, AgentLoopError>>,
    },
    SubagentProgress(Arc<super::child_progress::ChildProgressSlot>),
    ToolProgress(Arc<ProgressSlot>),
    CompactionProgress(CompactionProgress),
    Approval {
        request: PermissionRequest,
        respond: oneshot::Sender<ApprovalDecision>,
    },
    Question {
        request: AskUserInput,
        respond: oneshot::Sender<Result<String, rw_tools::ToolError>>,
        admission: OwnedSemaphorePermit,
    },
    Complete(TurnOutcome),
    ManualCompactionComplete {
        turn: u64,
        conversation: crate::engine::session::ConversationSummary,
        result: Result<(), AgentLoopError>,
        model_switch: Option<PreparedModelSwitch>,
        completion: Option<oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>>,
    },
    InitializationComplete {
        name: &'static str,
        result: Result<String, AgentLoopError>,
    },
    SessionTitleGenerated {
        title: String,
        usage: Option<SessionUsage>,
        cost: Option<Cost>,
    },
}

pub(in crate::engine) struct TurnOutcome {
    pub(super) turn: u64,
    pub(super) conversation: crate::engine::session::ConversationSummary,
    pub(super) status: AgentTurnStatus,
    pub(super) usage: SessionUsage,
    pub(super) cost: Cost,
    pub(super) deferred_terminal_delta: Option<String>,
    pub(super) deferred_terminal_turn: Option<Turn>,
    pub(super) budgeter: Budgeter,
}
