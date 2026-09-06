use crate::engine::AgentLoopError;
use crate::engine::apply_mode_change;
use crate::engine::dispatch::DispatchContext;
use crate::engine::dispatch::compaction::start_manual_compaction;
use crate::engine::dispatch::context_surgery::apply_context_surgery;
use crate::engine::dispatch::handle_actor_command;
use crate::engine::dispatch::model_switch::commit_prepared_model_switch;
use crate::engine::dispatch::rewind::rewind_state;
use crate::engine::mode_permission_base;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::RecoveredUserShell;
use crate::engine::projection::parse_turn_id;
use crate::engine::projection::plan_review_context_turn;
use crate::engine::projection::shell_context_turn;
use crate::engine::session::ActorCommand;
use crate::engine::session::PrecommittedAnswer;
use crate::engine::session::PreparedModelSwitch;
use crate::engine::session::ProtocolCompletion;
use crate::engine::turn::build_cost_snapshot;
use crate::engine::turn::emit;
use crate::engine::turn::emit_batch;
use rw_types::ClientCommand;
use rw_types::ClientRole;
use rw_types::CommandMeta;
use rw_types::ModeId;
use rw_types::ModelContextTransfer;
use rw_types::PlanArtifact;
use rw_types::PlanDecision;
use rw_types::RewindTarget;
use rw_types::ShellId;
use std::sync::atomic::Ordering;
use tokio::sync::oneshot;

#[allow(clippy::too_many_lines)]
pub(super) async fn apply_accepted(
    command: ClientCommand,
    meta: CommandMeta,
    mut completion: Option<oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>>,
    mut precommitted_answer: Option<PrecommittedAnswer>,
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
    match command {
        ClientCommand::ListPermissions { .. }
        | ClientCommand::AddSessionPermissionRule { .. }
        | ClientCommand::RemoveSessionPermissionRule { .. }
        | ClientCommand::RemoveQueuedMessage { .. }
        | ClientCommand::ClearQueuedMessages { .. }
        | ClientCommand::ExportSession { .. }
        | ClientCommand::RevokePermissionApproval { .. }
        | ClientCommand::ListMcpServers { .. }
        | ClientCommand::ListRuntimeServices { .. }
        | ClientCommand::AddMcpHttpServer { .. }
        | ClientCommand::AddMcpStdioServer { .. }
        | ClientCommand::RemoveMcpServer { .. }
        | ClientCommand::ReviewMcpServer { .. }
        | ClientCommand::ApproveMcpServer { .. }
        | ClientCommand::SetMcpServerEnabled { .. } => {
            unreachable!("host query commands return through their typed query branch")
        }
        ClientCommand::RenameSession { title, .. } => {
            state.transient_cause = Some(meta.request_id.clone());
            let result = emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::SessionTitleUpdated {
                    title: title.clone(),
                    usage: None,
                    cost: None,
                },
            )
            .await
            .map(|_| ());
            state.transient_cause = None;
            if result.is_ok() {
                state.session_title = Some(title);
                state.title_generation_started = true;
            }
            if let Some(complete) = completion.take() {
                let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
            }
        }
        ClientCommand::AttachDevelopmentPlugin { .. }
        | ClientCommand::DetachDevelopmentPlugin { .. } => {
            unreachable!("development plugin commands return through their typed branch")
        }
        ClientCommand::AttachSession { role, .. } => {
            state
                .client_roles
                .insert(meta.client_id.0.clone(), role.clone());
        }
        ClientCommand::TakeDriver { .. } => {
            state
                .client_roles
                .insert(meta.client_id.0.clone(), ClientRole::Driver);
        }
        ClientCommand::SwitchMode { mode, .. } => {
            let result =
                apply_mode_change(state, events, &config.event_sink, mode, &config.modes).await;
            if let Some(complete) = completion.take() {
                let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
            }
        }
        ClientCommand::ApprovePlan {
            decision,
            revisions,
            ..
        } => {
            let execute_definition = if decision == PlanDecision::Approve {
                let Some(definition) = config.modes.get("execute") else {
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(Err(AgentLoopError::InvalidConfiguration(
                            "execute mode is not registered".to_owned(),
                        )));
                    }
                    return;
                };
                Some(definition)
            } else {
                None
            };
            let artifact = state.pending_plan.clone().unwrap_or_else(|| PlanArtifact {
                title: String::new(),
                summary_md: String::new(),
                steps: Vec::new(),
                open_questions: Vec::new(),
            });
            let mut durable = vec![PendingEvent::PlanReviewed {
                artifact: artifact.clone(),
                decision,
                revisions: revisions.clone(),
            }];
            let context_turn = plan_review_context_turn(&artifact, decision, revisions.as_deref());
            let plan_source = rw_types::SequenceId(
                state.sequence.map_or(0, |sequence| sequence + 1) + durable.len() as u64,
            );
            let item_id = rw_types::context_source::conversation_item(plan_source);
            if context_turn.is_some() {
                durable.push(PendingEvent::ConversationContextCommitted {
                    agent_turn: state.completed_turns,
                    selection: rw_types::conversation_input::ContextSelection::PlanReview {
                        source: rw_types::SequenceId(plan_source.0 - 1),
                    },
                });
            }
            if let Some(definition) = execute_definition {
                durable.push(PendingEvent::ContextItemPinned {
                    item_id: item_id.clone(),
                    effective_after_agent_turn: state.completed_turns,
                });
                durable.push(PendingEvent::ModeChanged {
                    mode: ModeId("execute".to_owned()),
                    definition_fingerprint: definition.semantic_fingerprint(),
                });
            }
            let result = emit_batch(state, events, &config.event_sink, durable)
                .await
                .map(|_| ());
            if result.is_ok() {
                state.pending_plan = None;
                if let Some(turn) = context_turn {
                    state.append_conversation(turn, plan_source);
                }
                if let Some(definition) = execute_definition {
                    state.approved_plan = Some(artifact);
                    state.plan_gate_active = false;
                    state.mode = mode_permission_base(definition);
                    state.mode_id = ModeId("execute".to_owned());
                }
            }
            if let Some(complete) = completion.take() {
                let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
            }
        }
        ClientCommand::SwitchModel {
            model, provider, ..
        } => {
            let result = super::model_switch::request_model_selection(
                state, config, events, model, provider,
            )
            .await;
            if let Some(complete) = completion.take() {
                let _ = complete.send(result.map(|_| ProtocolCompletion::Unit));
            }
        }
        ClientCommand::UserShellStarted { command, .. } => {
            let shell_id = ShellId(format!(
                "shell-{}",
                state
                    .sequence
                    .map_or(0, |sequence| sequence.saturating_add(1))
            ));
            let shell = RecoveredUserShell {
                shell_id: shell_id.clone(),
                command: command.clone(),
            };
            let result = emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::UserShellStateChanged {
                    shell_id,
                    command,
                    active: true,
                    status: None,
                    captured_output: None,
                },
            )
            .await
            .map(|_| ());
            if result.is_ok() {
                state.active_shell = Some(shell);
            }
            if let Some(complete) = completion.take() {
                let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
            }
        }
        ClientCommand::UserShellEnded {
            shell_id,
            status,
            captured_output,
            ..
        } => {
            let command = state
                .active_shell
                .as_ref()
                .map(|shell| shell.command.clone())
                .unwrap_or_default();
            let context = shell_context_turn(&command, status, captured_output.as_deref());
            let result = emit(
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
            .map(|meta| {
                state.append_conversation(context, meta.sequence_id);
                state.active_shell = None;
            });
            if let Some(complete) = completion.take() {
                let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
            }
        }
        ClientCommand::InvokeUiAction { request, .. } => {
            match super::ui_actions::resolve(state, config, &request).await {
                Err(error) => {
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(Err(error));
                    }
                }
                Ok(bound) => {
                    super::command_job::start(
                        meta,
                        Ok(bound),
                        active_turn.load(Ordering::Acquire),
                        super::command_job::CommandReply::Protocol(completion.take()),
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
            }
        }
        ClientCommand::SendMessage {
            content,
            attachments,
            ..
        } => {
            if content.trim_start().starts_with('/') {
                let bound = config.commands.bind_line(&content);
                super::command_job::start(
                    meta,
                    bound,
                    active_turn.load(Ordering::Acquire),
                    super::command_job::CommandReply::Protocol(completion.take()),
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
                return;
            }
            let (internal_respond, internal_receive) = oneshot::channel();
            Box::pin(handle_actor_command(
                ActorCommand::SendMessage {
                    command_meta: meta,
                    content,
                    attachments,
                    observed_turn: active_turn.load(Ordering::Acquire),
                    respond: internal_respond,
                },
                state,
                config,
                tool_context,
                turn_signals,
                events,
                active_turn,
                command_descriptors,
                mode_registry,
            ))
            .await;
            match internal_receive.await {
                Ok(Ok(disposition)) => {
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(Ok(ProtocolCompletion::Message(disposition)));
                    }
                }
                Ok(Err(error)) => {
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(Err(error.clone()));
                    }
                    // A turn-opening failure is not itself evidence that
                    // durable state is inconsistent. Production sinks
                    // append opening batches atomically, and validation
                    // failures happen before an append. Keep the actor
                    // usable so a transient storage failure (or a
                    // corrected input) can be retried without trapping
                    // the UI in an unrecoverable live-poisoned session.
                    let _ = emit(
                        state,
                        events,
                        &config.event_sink,
                        PendingEvent::Error {
                            message: format!(
                                "accepted message failed before turn execution: {error}"
                            ),
                        },
                    )
                    .await
                    .map(|_| ());
                }
                Err(_) => {
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(Err(AgentLoopError::Closed));
                    }
                }
            }
        }
        ClientCommand::Interrupt { .. } => {
            unreachable!("interrupt admission belongs to session control")
        }
        ClientCommand::ApproveTool {
            tool_call_id,
            decision,
            ..
        } => {
            if let Some(pending) = state.pending_approvals.remove(&tool_call_id.0) {
                let _ = pending.respond.send(decision);
            }
            if let Some(complete) = completion.take() {
                let _ = complete.send(Ok(ProtocolCompletion::Unit));
            }
        }
        ClientCommand::AnswerQuestion { .. } => {
            if let Some(answer) = precommitted_answer.take() {
                match answer {
                    PrecommittedAnswer::Turn(pending, answer) => {
                        let _ = pending.respond.send(Ok(answer));
                        if let Some(complete) = completion.take() {
                            let _ = complete.send(Ok(ProtocolCompletion::Unit));
                        }
                    }
                    PrecommittedAnswer::Model(pending, strategy) => {
                        let prepared = PreparedModelSwitch {
                            thinking: config
                                .model
                                .thinking_for_model(&pending.model.0, state.thinking),
                            model: pending.model,
                            provider: pending.provider,
                        };
                        match strategy {
                            ModelContextTransfer::PassSummary => {
                                let completion = completion.take();
                                start_manual_compaction(
                                state,
                                config,
                                turn_signals,
                                active_turn,
                                Some(
                                    "Summarize the conversation for transfer to the selected model. Preserve user intent, decisions, constraints, and unfinished work."
                                        .to_owned(),
                                ),
                                Some(prepared),
                                completion,
                            ).await;
                            }
                            ModelContextTransfer::PassFullContext
                            | ModelContextTransfer::StartWithoutContext => {
                                let clear_context =
                                    strategy == ModelContextTransfer::StartWithoutContext;
                                let result = commit_prepared_model_switch(
                                    state,
                                    config,
                                    events,
                                    prepared,
                                    clear_context,
                                )
                                .await;
                                if let Some(complete) = completion.take() {
                                    let _ =
                                        complete.send(result.map(|()| ProtocolCompletion::Unit));
                                }
                            }
                        }
                    }
                }
            }
        }
        ClientCommand::Rewind {
            target: RewindTarget::Turn { turn_id },
            ..
        } => {
            let rewind = match parse_turn_id(&turn_id) {
                Ok(to_turn) => rewind_state(state, config, events, to_turn).await,
                Err(error) => Err(AgentLoopError::InvalidConfiguration(error.to_string())),
            };
            let result = match rewind {
                Ok(unrestorable_paths) => {
                    let message = if unrestorable_paths.is_empty() {
                        format!("rewound to turn {}", turn_id.0)
                    } else {
                        format!(
                            "rewound to turn {} with {} unrestorable path(s)",
                            turn_id.0,
                            unrestorable_paths.len()
                        )
                    };
                    emit(
                        state,
                        events,
                        &config.event_sink,
                        PendingEvent::CommandFinished {
                            name: "rewind".to_owned(),
                            message,
                            unrestorable_paths: unrestorable_paths.clone(),
                        },
                    )
                    .await
                    .map(|_| ())
                    .map(|()| unrestorable_paths)
                }
                Err(error) => Err(error),
            };
            if let Some(complete) = completion.take() {
                let _ = complete.send(result.map(ProtocolCompletion::Rewind));
            }
        }
        ClientCommand::PinContext { item_id, .. } => {
            let result =
                apply_context_surgery(state, events, &config.event_sink, item_id, true).await;
            if let Some(complete) = completion.take() {
                let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
            }
        }
        ClientCommand::EvictContext { item_id, .. } => {
            let result =
                apply_context_surgery(state, events, &config.event_sink, item_id, false).await;
            if let Some(complete) = completion.take() {
                let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
            }
        }
        ClientCommand::GetContext { .. } => {
            super::context_job::start(
                state,
                config,
                super::context_job::Target::Context {
                    completion: completion.take(),
                },
            );
        }
        ClientCommand::GetCost { .. } => {
            let result = build_cost_snapshot(state, config).await;
            if let Some(complete) = completion.take() {
                let _ = complete
                    .send(result.map(|snapshot| ProtocolCompletion::Cost(Box::new(snapshot))));
            }
        }
        ClientCommand::DumpPrompt { turn_id, .. } => {
            super::context_job::start(
                state,
                config,
                super::context_job::Target::Prompt {
                    turn: turn_id,
                    completion: completion.take(),
                },
            );
        }
        ClientCommand::Compact { instructions, .. } => {
            let completion = completion.take();
            start_manual_compaction(
                state,
                config,
                turn_signals,
                active_turn,
                instructions,
                None,
                completion,
            )
            .await;
        }
        ClientCommand::ReadSessionChildren { .. }
        | ClientCommand::ReadTranscriptTail { .. }
        | ClientCommand::ReadTranscript { .. }
        | ClientCommand::ReadTranscriptContent { .. }
        | ClientCommand::GetTodos { .. }
        | ClientCommand::CreateSession { .. }
        | ClientCommand::ResumeSession { .. }
        | ClientCommand::Fork { .. }
        | ClientCommand::GetSessionReview { .. }
        | ClientCommand::ReviewFile { .. }
        | ClientCommand::ListSessions { .. }
        | ClientCommand::SearchSessions { .. }
        | ClientCommand::GetSessionState { .. }
        | ClientCommand::GetSessionControls { .. }
        | ClientCommand::ReadFamilyControls { .. }
        | ClientCommand::ReadChildState { .. }
        | ClientCommand::ReadChildControls { .. }
        | ClientCommand::ResolveChildControl { .. }
        | ClientCommand::GetUiCatalog { .. }
        | ClientCommand::GetUiPanels { .. }
        | ClientCommand::ListCommands { .. }
        | ClientCommand::ListModes { .. }
        | ClientCommand::ListModels { .. }
        | ClientCommand::ListSettings { .. }
        | ClientCommand::SetSetting { .. }
        | ClientCommand::BeginProviderAuth { .. }
        | ClientCommand::ConfigureBuiltinProvider { .. }
        | ClientCommand::CompleteProviderAuth { .. }
        | ClientCommand::CancelProviderAuth { .. }
        | ClientCommand::SearchWorkspaceFiles { .. }
        | ClientCommand::PreviewWorkspaceFile { .. }
        | ClientCommand::GetWorkspaceStatus { .. }
        | ClientCommand::GetWorkspaceDiff { .. }
        | ClientCommand::ListSubagents { .. }
        | ClientCommand::ContinueSubagent { .. }
        | ClientCommand::InterruptSubagent { .. }
        | ClientCommand::CloseSubagent { .. }
        | ClientCommand::ShutdownHost { .. }
        | ClientCommand::Rewind {
            target: RewindTarget::Source { .. },
            ..
        } => {}
    }
    if let Some(complete) = completion.take() {
        let _ = complete.send(Err(AgentLoopError::InvalidConfiguration(
            "command has no local completion result".to_owned(),
        )));
    }
    state.transient_cause = None;
}
