use crate::engine::AgentLoopError;
use crate::engine::MAX_CAPTURED_SHELL_OUTPUT_BYTES;
use crate::engine::SESSION_TITLE_MAX_CHARS;
use crate::engine::diff_binding;
use crate::engine::dispatch::DispatchContext;
use crate::engine::dispatch::accepted::apply_accepted;
use crate::engine::dispatch::message_input::prepare_user_message;
use crate::engine::dispatch::permissions::apply_permission_command;
use crate::engine::dispatch::replies::protocol_rejection;
use crate::engine::dispatch::replies::query_meta;
use crate::engine::dispatch::replies::send_ack;
use crate::engine::dispatch::replies::send_connection_event;
use crate::engine::mode_permission_base;
use crate::engine::model_switch_answer;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::review_hash_is_valid;
use crate::engine::projection::review_path_is_valid;
use crate::engine::replay::SessionReplayLimits;
use crate::engine::session::PrecommittedAnswer;
use crate::engine::session::ProtocolCompletion;
use crate::engine::session::recover_actor_from_journal;
use crate::engine::session::validate_gap;
use crate::engine::turn::assemble_session_context;
use crate::engine::turn::current_approval_diff;
use crate::engine::turn::emit;
use crate::engine::turn::normalize_manual_session_title;
use rw_types::ApprovalDecision;
use rw_types::ClientCommand;
use rw_types::ClientRole;
use rw_types::CommandOutcome;
use rw_types::EngineEvent;
use rw_types::PROTOCOL_VERSION;
use rw_types::RewindTarget;
use rw_types::SessionMode;
use std::path::Path;
use tokio::sync::oneshot;

pub(super) fn requires_driver(command: &ClientCommand) -> bool {
    !matches!(
        command,
        ClientCommand::CreateSession { .. }
            | ClientCommand::AttachSession { .. }
            | ClientCommand::TakeDriver { .. }
            | ClientCommand::GetContext { .. }
            | ClientCommand::GetCost { .. }
            | ClientCommand::GetSessionReview { .. }
            | ClientCommand::DumpPrompt { .. }
            | ClientCommand::ListPermissions { .. }
            | ClientCommand::RenameSession { .. }
            | ClientCommand::AttachDevelopmentPlugin { .. }
            | ClientCommand::DetachDevelopmentPlugin { .. }
    )
}

pub(super) fn is_host_command(command: &ClientCommand) -> bool {
    matches!(
        command,
        ClientCommand::CreateSession { .. }
            | ClientCommand::ResumeSession { .. }
            | ClientCommand::Fork { .. }
            | ClientCommand::ListSessions { .. }
            | ClientCommand::GetSessionControls { .. }
            | ClientCommand::GetUiCatalog { .. }
            | ClientCommand::GetUiPanels { .. }
            | ClientCommand::ListCommands { .. }
            | ClientCommand::ListModes { .. }
            | ClientCommand::ListModels { .. }
            | ClientCommand::SearchWorkspaceFiles { .. }
            | ClientCommand::PreviewWorkspaceFile { .. }
            | ClientCommand::GetWorkspaceStatus { .. }
            | ClientCommand::ShutdownHost { .. }
    )
}

#[allow(clippy::too_many_lines)]
pub(super) async fn dispatch_protocol(
    mut command: ClientCommand,
    respond: oneshot::Sender<CommandOutcome>,
    mut completion: Option<oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>>,
    prepared: bool,
    context: DispatchContext<'_>,
) -> bool {
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
    let meta = command.meta().clone();
    let session = command.session_id().cloned();
    let rejection = if meta.protocol_version != PROTOCOL_VERSION {
        Some(protocol_rejection(
            "unsupported_protocol_version",
            format!(
                "protocol version {} is unsupported; expected {PROTOCOL_VERSION}",
                meta.protocol_version
            ),
        ))
    } else if session.as_ref().is_some_and(|id| id != &config.session_id) {
        Some(protocol_rejection(
            "session_mismatch",
            "command session id does not match this actor",
        ))
    } else if state.closing {
        Some(protocol_rejection("session_closing", "session is closing"))
    } else if is_host_command(&command) {
        Some(protocol_rejection(
            "command_not_available",
            "command requires host dispatch",
        ))
    } else if state.poisoned
        && !matches!(
            (&command, &state.pending_rewind),
            (
                ClientCommand::Rewind {
                    target: RewindTarget::Turn { turn_id },
                    ..
                },
                Some((pending_turn, _))
            ) if turn_id.0 == pending_turn.to_string()
        )
    {
        Some(protocol_rejection(
            "session_requires_recovery",
            "session is fail-closed until checkpoint journal recovery completes",
        ))
    } else if requires_driver(&command) && state.control.driver().as_ref() != Some(&meta.client_id)
    {
        Some(protocol_rejection(
            "driver_required",
            "mutating commands are accepted only from the current driver",
        ))
    } else {
        None
    };
    if let Some(outcome) = rejection {
        send_ack(state, events, &meta, session, outcome.clone());
        let _ = respond.send(outcome);
        return false;
    }

    if state.pending_model_preparation.is_some() && !super::model_job::admit_while_pending(&command)
    {
        let outcome = protocol_rejection(
            "model_preparation_busy",
            "model preparation owns the session selection",
        );
        send_ack(state, events, &meta, session, outcome.clone());
        let _ = respond.send(outcome);
        return false;
    }
    let preparation = (!prepared)
        .then(|| super::model_job::protocol_alias(&command, state))
        .flatten();
    if state.pending_command.is_some() && !super::command_job::admit_while_pending(&command) {
        let outcome = protocol_rejection(
            "command_busy",
            "an admitted command still owns the session policy and generation",
        );
        send_ack(state, events, &meta, session, outcome.clone());
        let _ = respond.send(outcome);
        return false;
    }

    if let ClientCommand::InvokeUiAction { request, .. } = &command
        && let Err(error) = request.validate()
    {
        let outcome = protocol_rejection("invalid_ui_action", error.to_string());
        send_ack(state, events, &meta, session, outcome.clone());
        let _ = respond.send(outcome);
        return false;
    }

    if let Err(error) = super::source_rewind::resolve(&mut command, state, config).await {
        let outcome = protocol_rejection("invalid_rewind_source", error.to_string());
        send_ack(state, events, &meta, session, outcome.clone());
        let _ = respond.send(outcome);
        return false;
    }

    if let Some(outcome) = super::completed_turns::rejection(&command, state, config).await {
        send_ack(state, events, &meta, session, outcome.clone());
        let _ = respond.send(outcome);
        return false;
    }

    if let ClientCommand::UserShellEnded {
        captured_output, ..
    } = &mut command
    {
        *captured_output = captured_output
            .take()
            .map(|output| config.secret_redactor.redact(&output));
    }
    if let ClientCommand::RenameSession { title, .. } = &mut command {
        let Some(normalized) = normalize_manual_session_title(title) else {
            let outcome = protocol_rejection(
                "invalid_session_title",
                format!(
                    "session title must be non-empty, contain no control characters, and contain at most {SESSION_TITLE_MAX_CHARS} characters"
                ),
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        };
        *title = normalized;
    }

    match &command {
        ClientCommand::AttachSession { role, .. } => {
            if *role == ClientRole::Driver
                && state
                    .control
                    .driver()
                    .as_ref()
                    .is_some_and(|driver| driver != &meta.client_id)
            {
                let outcome = protocol_rejection(
                    "driver_lease_held",
                    "another client holds the driver lease; attach as observer or take it explicitly",
                );
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                return false;
            }
        }
        ClientCommand::SendMessage { .. } if state.active_shell.is_some() => {
            let outcome = protocol_rejection(
                "user_shell_active",
                "an agent turn cannot start while the foreground user shell is active",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::AttachDevelopmentPlugin { source, .. }
            if state.running.is_some()
                || state.active_shell.is_some()
                || state.control.driver().is_none()
                || source.is_empty()
                || source.len() > 4096
                || source.chars().any(char::is_control) =>
        {
            let outcome = protocol_rejection(
                "development_attach_requires_idle_session",
                "development plugin attachment requires an idle session and one bounded source path",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::DetachDevelopmentPlugin { .. }
            if state.running.is_some()
                || state.active_shell.is_some()
                || state.control.driver().is_none() =>
        {
            let outcome = protocol_rejection(
                "development_detach_requires_idle_session",
                "development plugin detachment requires an idle session",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::SendMessage { attachments, .. }
            if (state.running.is_some()
                || state.pending_command.is_some()
                || state.pending_model_preparation.is_some())
                && !attachments.is_empty() =>
        {
            let outcome = protocol_rejection(
                "attachment_queue_unsupported",
                "messages with attachments require an idle session so their provider-neutral blocks commit atomically",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::SendMessage {
            content,
            attachments,
            ..
        } if content.trim_start().starts_with('/') && !attachments.is_empty() => {
            let outcome = protocol_rejection(
                "command_attachments_unsupported",
                "slash commands do not accept message attachments",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::SendMessage {
            content,
            attachments,
            ..
        } => {
            if preparation.is_none()
                && let Err(message) = prepare_user_message(
                    content,
                    attachments,
                    &state.model_alias,
                    config.model.as_ref(),
                )
            {
                let outcome = protocol_rejection("invalid_attachment", message);
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                return false;
            }
        }
        ClientCommand::SwitchModel { .. }
        | ClientCommand::SwitchMode { .. }
        | ClientCommand::ApprovePlan { .. }
            if state.running.is_some() || state.active_shell.is_some() =>
        {
            let outcome = protocol_rejection(
                "session_not_idle",
                "model switching requires an idle session with no active user shell",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::SwitchModel { .. } if !state.pending_model_switches.is_empty() => {
            let outcome = protocol_rejection(
                "model_switch_pending",
                "choose how to transfer context for the pending model switch first",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::SwitchModel {
            model, provider, ..
        } => {
            if !config.model.has_model_alias(&model.0) {
                let outcome = protocol_rejection(
                    "unknown_model_alias",
                    format!("model {:?} is unavailable", model.0),
                );
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                return false;
            }
            if let Some(provider) = provider
                && !config.model.has_provider_for_alias(&model.0, provider)
            {
                let outcome = protocol_rejection(
                    "unknown_provider_route",
                    format!(
                        "model alias {:?} has no configured route through provider {:?}",
                        model.0, provider
                    ),
                );
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                return false;
            }
        }
        ClientCommand::SwitchMode { mode, .. } if config.modes.get(&mode.0).is_none() => {
            let outcome = protocol_rejection(
                "unknown_mode",
                format!("mode {:?} is not registered", mode.0),
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::SwitchMode { mode, .. }
            if config.modes.get(&mode.0).is_some_and(|definition| {
                mode_permission_base(definition) == SessionMode::Execute
            }) && state.plan_gate_active =>
        {
            let outcome = protocol_rejection(
                "plan_approval_required",
                "Plan mode can enter Execute only after the submitted plan is approved",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::ApprovePlan { .. } if state.pending_plan.is_none() => {
            let outcome = protocol_rejection(
                "no_pending_plan",
                "there is no submitted plan awaiting review",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::UserShellStarted { command, .. }
            if command.trim().is_empty()
                || state.running.is_some()
                || state.active_shell.is_some()
                || config.tools.session_activity(&state.session_id).is_some() =>
        {
            let outcome = protocol_rejection(
                "shell_start_rejected",
                "a non-empty foreground shell may start only while the session is idle",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::UserShellEnded {
            shell_id,
            captured_output,
            ..
        } if state.active_shell.as_ref().map(|shell| &shell.shell_id) != Some(shell_id)
            || captured_output
                .as_ref()
                .is_some_and(|output| output.len() > MAX_CAPTURED_SHELL_OUTPUT_BYTES) =>
        {
            let outcome = protocol_rejection(
                "shell_end_rejected",
                "shell end must match the active shell id and its captured output must fit the durable limit",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::GetSessionReview { .. }
            if state.running.is_some()
                || state.active_shell.is_some()
                || config.tools.session_activity(&state.session_id).is_some() =>
        {
            let outcome = protocol_rejection(
                "session_not_idle",
                "session review requires an idle session",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::ReviewFile {
            path, current_hash, ..
        } if state.running.is_some()
            || state.active_shell.is_some()
            || config.tools.session_activity(&state.session_id).is_some()
            || !review_path_is_valid(path)
            || !review_hash_is_valid(current_hash) =>
        {
            let outcome = protocol_rejection(
                "invalid_review_file",
                "review decisions require an idle session, a safe relative path, and the displayed current hash",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::ApproveTool {
            tool_call_id,
            invocation_id,
            ..
        } if state
            .pending_approvals
            .get(&tool_call_id.0)
            .is_none_or(|pending| &pending.request.invocation_id != invocation_id) =>
        {
            let outcome = protocol_rejection("unknown_approval", "tool approval is not pending");
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::ApproveTool {
            tool_call_id,
            binding,
            ..
        } if state
            .pending_approvals
            .get(&tool_call_id.0)
            .is_some_and(|pending| pending.binding.as_ref() != binding.as_ref()) =>
        {
            let outcome = protocol_rejection(
                "approval_binding_mismatch",
                "approval binding does not match the displayed proposal; the approval remains pending",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::ApproveTool {
            tool_call_id,
            decision:
                ApprovalDecision::AllowOnce
                | ApprovalDecision::AllowSession
                | ApprovalDecision::AllowProject,
            ..
        } if state
            .pending_approvals
            .get(&tool_call_id.0)
            .and_then(|pending| pending.request.approval_diff.as_ref())
            .is_some_and(|diff| diff.truncated) =>
        {
            let outcome = protocol_rejection(
                "truncated_approval_denied",
                "a truncated diff cannot be approved; deny it and review the complete change through a bounded proposal",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::AnswerQuestion {
            question_id,
            answers,
            ..
        } if (!state.pending_questions.contains_key(&question_id.0)
            && !state.pending_model_switches.contains_key(&question_id.0))
            || !answers
                .iter()
                .any(|answer| answer.question_id == *question_id && !answer.values.is_empty()) =>
        {
            let outcome = protocol_rejection(
                "invalid_question_answer",
                "question is not pending or its answer is empty",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::AnswerQuestion {
            question_id,
            answers,
            ..
        } if state.pending_model_switches.contains_key(&question_id.0)
            && model_switch_answer(answers, question_id).is_none() =>
        {
            let outcome = protocol_rejection(
                "invalid_model_context_transfer",
                "model switching requires exactly one of the displayed context choices",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::AnswerQuestion { question_id, .. }
            if state
                .pending_model_switches
                .get(&question_id.0)
                .is_some_and(|pending| {
                    pending.provider.as_ref().is_some_and(|provider| {
                        !config
                            .model
                            .has_provider_for_alias(&pending.model.0, provider)
                    })
                }) =>
        {
            let outcome = protocol_rejection(
                "unknown_provider_route",
                "the pending model no longer has the selected provider route",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::Compact { .. } if state.running.is_some() => {
            let outcome =
                protocol_rejection("turn_running", "manual compaction requires an idle session");
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        ClientCommand::PinContext { item_id, .. } | ClientCommand::EvictContext { item_id, .. } => {
            if state.running.is_some() {
                let outcome =
                    protocol_rejection("turn_running", "context surgery requires an idle session");
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                return false;
            }
            if !item_id.0.starts_with("conversation:") {
                let outcome = protocol_rejection(
                    "protected_context_item",
                    "only conversation-resident context items support pin or eviction",
                );
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                return false;
            }
            let known = assemble_session_context(
                config,
                &state.conversation,
                &state.queued,
                &state.context_surgery,
                &state.pruned_tool_outputs,
                false,
            )
            .is_ok_and(|assembled| assembled.items.iter().any(|item| item.id.0 == item_id.0));
            if !known {
                let outcome = protocol_rejection(
                    "unknown_context_item",
                    "context item is not present in the current inventory",
                );
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                return false;
            }
        }
        _ => {}
    }

    if let Some(alias) = preparation {
        super::model_job::start(
            state,
            config,
            events,
            alias,
            super::model_job::SelectionAction::Protocol {
                command: Box::new(command),
                respond,
                completion,
            },
        );
        return false;
    }

    if let ClientCommand::ApproveTool {
        tool_call_id,
        decision:
            ApprovalDecision::AllowOnce
            | ApprovalDecision::AllowSession
            | ApprovalDecision::AllowProject,
        ..
    } = &command
    {
        let pending_request = state
            .pending_approvals
            .get(&tool_call_id.0)
            .filter(|pending| pending.binding.is_some())
            .map(|pending| (pending.request.clone(), pending.turn));
        if let Some((request, turn)) = pending_request {
            let refreshed = if let Some(tool) = config.tools.resolve(&request.tool_name) {
                current_approval_diff(&tool, tool_context, &request).await
            } else {
                Err("approved tool is no longer registered".to_owned())
            };
            let current_diff = refreshed.ok().flatten();
            let current_binding = current_diff.as_ref().map(diff_binding);
            let expected_binding = state
                .pending_approvals
                .get(&tool_call_id.0)
                .and_then(|pending| pending.binding.clone());
            if current_binding != expected_binding {
                if let Some(diff) = current_diff {
                    let mut refreshed_request = request;
                    refreshed_request.approval_diff = Some(diff);
                    if let Some(pending) = state.pending_approvals.get_mut(&tool_call_id.0) {
                        pending.binding = current_binding;
                        pending.request = refreshed_request.clone();
                    }
                    if let Err(error) = emit(
                        state,
                        events,
                        &config.event_sink,
                        PendingEvent::PermissionRequested {
                            turn,
                            request: refreshed_request,
                        },
                    )
                    .await
                    .map(|_| ())
                    {
                        if let Some(pending) = state.pending_approvals.remove(&tool_call_id.0) {
                            let _ = pending.respond.send(ApprovalDecision::Deny);
                        }
                        let outcome = protocol_rejection(
                            "approval_refresh_failed",
                            format!("could not persist refreshed approval: {error}"),
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return false;
                    }
                } else {
                    if let Err(error) = super::controls::resolve_approval(
                        state,
                        config,
                        events,
                        tool_call_id,
                        ApprovalDecision::Deny,
                    )
                    .await
                    {
                        state.poisoned = true;
                        let outcome = protocol_rejection(
                            "session_persistence_failure",
                            format!("could not persist stale approval resolution: {error}"),
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return false;
                    }
                    if let Some(pending) = state.pending_approvals.remove(&tool_call_id.0) {
                        let _ = pending.respond.send(ApprovalDecision::Deny);
                    }
                }
                let outcome = protocol_rejection(
                    "approval_stale",
                    "workspace state changed after the displayed diff; no mutation ran and a fresh approval is required",
                );
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                return false;
            }
        }
    }

    if let ClientCommand::RemoveQueuedMessage { position, .. } = &command {
        let Some(index) = state
            .queued_positions
            .iter()
            .position(|queued_position| queued_position.to_string() == *position)
        else {
            let (code, message) = if state.queued.is_empty() {
                (
                    "queued_messages_empty",
                    "there are no queued messages to remove".to_owned(),
                )
            } else {
                (
                    "queued_message_not_found",
                    format!("queued message position {position:?} is no longer present"),
                )
            };
            let outcome = protocol_rejection(code, message);
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            if let Some(complete) = completion.take() {
                let _ = complete.send(Err(AgentLoopError::InvalidConfiguration(
                    "queued message removal failed".to_owned(),
                )));
            }
            return false;
        };
        let queued_position = state.queued_positions[index];
        state.transient_cause = Some(meta.request_id.clone());
        let persisted = emit(
            state,
            events,
            &config.event_sink,
            PendingEvent::QueuedMessageRemoved {
                position: queued_position,
            },
        )
        .await
        .map(|_| ());
        state.transient_cause = None;
        match persisted {
            Ok(()) => {
                state.queued.remove(index);
                state.queued_positions.remove(index);
                let accepted = CommandOutcome::Accepted {};
                send_ack(state, events, &meta, session, accepted.clone());
                let _ = respond.send(accepted);
                if let Some(complete) = completion.take() {
                    let _ = complete.send(Ok(ProtocolCompletion::Unit));
                }
            }
            Err(error) => {
                let outcome = protocol_rejection(
                    "session_persistence_failure",
                    format!("could not persist queued message removal: {error}"),
                );
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                if let Some(complete) = completion.take() {
                    let _ = complete.send(Err(error));
                }
            }
        }
        return false;
    }

    if matches!(&command, ClientCommand::ClearQueuedMessages { .. }) {
        if state.queued.is_empty() {
            let outcome = protocol_rejection(
                "queued_messages_empty",
                "there are no queued messages to clear",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            if let Some(complete) = completion.take() {
                let _ = complete.send(Err(AgentLoopError::InvalidConfiguration(
                    "queued message clear failed".to_owned(),
                )));
            }
            return false;
        }
        state.transient_cause = Some(meta.request_id.clone());
        let persisted = emit(
            state,
            events,
            &config.event_sink,
            PendingEvent::QueuedMessagesCleared,
        )
        .await
        .map(|_| ());
        state.transient_cause = None;
        match persisted {
            Ok(()) => {
                state.queued.clear();
                state.queued_positions.clear();
                let accepted = CommandOutcome::Accepted {};
                send_ack(state, events, &meta, session, accepted.clone());
                let _ = respond.send(accepted);
                if let Some(complete) = completion.take() {
                    let _ = complete.send(Ok(ProtocolCompletion::Unit));
                }
            }
            Err(error) => {
                let outcome = protocol_rejection(
                    "session_persistence_failure",
                    format!("could not persist queued message clear: {error}"),
                );
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                if let Some(complete) = completion.take() {
                    let _ = complete.send(Err(error));
                }
            }
        }
        return false;
    }

    if matches!(
        &command,
        ClientCommand::ListPermissions { .. }
            | ClientCommand::AddSessionPermissionRule { .. }
            | ClientCommand::RemoveSessionPermissionRule { .. }
            | ClientCommand::RevokePermissionApproval { .. }
    ) {
        let mutating = !matches!(&command, ClientCommand::ListPermissions { .. });
        let result = if mutating
            && (state.running.is_some()
                || state.active_shell.is_some()
                || config.tools.session_activity(&state.session_id).is_some())
        {
            Err("permission mutations require an idle session".to_owned())
        } else {
            apply_permission_command(&command, &config.permissions)
        };
        match result {
            Ok(permissions) => {
                let accepted = CommandOutcome::Accepted {};
                send_ack(state, events, &meta, session, accepted.clone());
                send_connection_event(
                    events,
                    &meta.client_id,
                    EngineEvent::PermissionsListed {
                        meta: query_meta(state, &meta),
                        session_id: state.session_id.clone(),
                        permissions,
                    },
                );
                let _ = respond.send(accepted);
                if let Some(complete) = completion.take() {
                    let _ = complete.send(Ok(ProtocolCompletion::Unit));
                }
            }
            Err(message) => {
                let outcome = protocol_rejection("permission_operation_failed", message);
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                if let Some(complete) = completion.take() {
                    let _ = complete.send(Err(AgentLoopError::InvalidConfiguration(
                        "permission operation failed".to_owned(),
                    )));
                }
            }
        }
        return false;
    }

    if let ClientCommand::AttachSession {
        last_seen_sequence, ..
    } = &command
    {
        let view = match config.event_sink.capture_read_view() {
            Ok(view) => view,
            Err(error) => {
                let outcome = protocol_rejection(
                    "gap_replay_failed",
                    format!("could not read durable session tail: {error}"),
                );
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                return false;
            }
        };
        if last_seen_sequence
            .is_some_and(|last_seen| view.last_sequence().is_none_or(|tail| last_seen > tail))
        {
            let outcome = protocol_rejection(
                "sequence_ahead_of_log",
                "last-seen sequence is ahead of the durable session tail",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        }
        match view
            .read_page(*last_seen_sequence, SessionReplayLimits::default())
            .await
        {
            Ok(gap) => {
                if let Err(error) = validate_gap(*last_seen_sequence, &gap, &config.session_id) {
                    let outcome = protocol_rejection(
                        "invalid_gap_replay",
                        format!("durable session gap is invalid: {error}"),
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return false;
                }
            }
            Err(error) => {
                let outcome = protocol_rejection(
                    "gap_replay_failed",
                    format!("could not read durable session gap: {error}"),
                );
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                return false;
            }
        }
    }

    state.transient_cause = Some(meta.request_id.clone());
    let lease_persist = match &command {
        ClientCommand::AttachSession { role, .. }
            if *role == ClientRole::Driver && state.control.driver().is_none() =>
        {
            let driver_event = if state.sequence.is_none() {
                PendingEvent::SessionCreated {
                    driver_client_id: meta.client_id.clone(),
                }
            } else {
                PendingEvent::DriverChanged {
                    driver_client_id: meta.client_id.clone(),
                }
            };
            emit(state, events, &config.event_sink, driver_event)
                .await
                .map(|_| ())
        }
        ClientCommand::TakeDriver { .. }
            if state.control.driver().as_ref() != Some(&meta.client_id) =>
        {
            emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::DriverChanged {
                    driver_client_id: meta.client_id.clone(),
                },
            )
            .await
            .map(|_| ())
        }
        _ => Ok(()),
    };
    if let Err(error) = lease_persist {
        state.transient_cause = None;
        let outcome = protocol_rejection(
            "session_persistence_failure",
            format!("could not persist the driver lease: {error}"),
        );
        send_ack(state, events, &meta, session, outcome.clone());
        let _ = respond.send(outcome);
        if let Some(complete) = completion.take() {
            let _ = complete.send(Err(error));
        }
        return false;
    }
    if let ClientCommand::ApproveTool {
        tool_call_id,
        decision,
        ..
    } = &command
        && let Err(error) =
            super::controls::resolve_approval(state, config, events, tool_call_id, decision.clone())
                .await
    {
        if recover_actor_from_journal(state, config, events, active_turn)
            .await
            .is_err()
        {
            state.poisoned = true;
        }
        let outcome = protocol_rejection(
            "session_persistence_failure",
            format!("could not persist approval resolution: {error}"),
        );
        send_ack(state, events, &meta, session, outcome.clone());
        let _ = respond.send(outcome);
        if let Some(complete) = completion.take() {
            let _ = complete.send(Err(error));
        }
        return false;
    }
    let mut precommitted_answer = None;
    if let ClientCommand::AnswerQuestion {
        question_id,
        answers,
        ..
    } = &command
    {
        let answer = answers
            .iter()
            .find(|answer| answer.question_id == *question_id)
            .map(|answer| answer.values.join("\n"))
            .unwrap_or_default();
        let pending = if let Some(pending) = state.pending_questions.remove(&question_id.0) {
            PrecommittedAnswer::Turn(pending, answer)
        } else if let Some(pending) = state.pending_model_switches.remove(&question_id.0) {
            let Some(strategy) = model_switch_answer(answers, question_id) else {
                state
                    .pending_model_switches
                    .insert(question_id.0.clone(), pending);
                state.transient_cause = None;
                let outcome = protocol_rejection(
                    "invalid_model_context_transfer",
                    "model context choice stopped being valid before commit",
                );
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                return false;
            };
            PrecommittedAnswer::Model(pending, strategy)
        } else {
            state.transient_cause = None;
            let outcome = protocol_rejection(
                "invalid_question_answer",
                "question stopped pending before its answer could be committed",
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            return false;
        };
        let turn = match &pending {
            PrecommittedAnswer::Turn(pending, _) => pending.turn,
            PrecommittedAnswer::Model(pending, _) => pending.turn,
        };
        if let Err(error) = emit(
            state,
            events,
            &config.event_sink,
            PendingEvent::QuestionAnswered {
                turn,
                question_id: question_id.clone(),
                answers: answers.clone(),
            },
        )
        .await
        .map(|_| ())
        {
            if let PrecommittedAnswer::Turn(pending, _) = pending {
                drop(pending.respond);
            }
            state.transient_cause = None;
            if recover_actor_from_journal(state, config, events, active_turn)
                .await
                .is_err()
            {
                // The durable log itself could not be read or repaired;
                // unlike an append failure, continuing from mutable
                // memory would risk acknowledging nonexistent state.
                state.poisoned = true;
            }
            let outcome = protocol_rejection(
                "session_persistence_failure",
                format!("could not persist the question answer: {error}"),
            );
            send_ack(state, events, &meta, session, outcome.clone());
            let _ = respond.send(outcome);
            if let Some(complete) = completion.take() {
                let _ = complete.send(Err(error));
            }
            return false;
        }
        precommitted_answer = Some(pending);
    }
    if matches!(
        command,
        ClientCommand::GetSessionReview { .. } | ClientCommand::ReviewFile { .. }
    ) {
        let result = match &command {
            ClientCommand::GetSessionReview { .. } => config
                .checkpoints
                .session_review(&state.session_id)
                .await
                .map(|review| EngineEvent::SessionReviewReady {
                    meta: query_meta(state, &meta),
                    session_id: state.session_id.clone(),
                    review,
                }),
            ClientCommand::ReviewFile {
                path,
                decision,
                current_hash,
                ..
            } => config
                .checkpoints
                .resolve_review_file(&state.session_id, Path::new(path), *decision, current_hash)
                .await
                .map(|review| EngineEvent::SessionReviewUpdated {
                    meta: query_meta(state, &meta),
                    session_id: state.session_id.clone(),
                    path: path.clone(),
                    decision: *decision,
                    review,
                }),
            _ => unreachable!("review command guard narrows the command"),
        };
        state.transient_cause = None;
        match result {
            Ok(event) => {
                let accepted = CommandOutcome::Accepted {};
                send_ack(state, events, &meta, session, accepted.clone());
                send_connection_event(events, &meta.client_id, event);
                let _ = respond.send(accepted);
                if let Some(complete) = completion.take() {
                    let _ = complete.send(Ok(ProtocolCompletion::Unit));
                }
            }
            Err(error) => {
                let outcome = protocol_rejection(
                    "session_review_failed",
                    "session review could not be completed; refresh and retry",
                );
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                if let Some(complete) = completion.take() {
                    let _ = complete.send(Err(error));
                }
            }
        }
        return false;
    }
    if matches!(
        command,
        ClientCommand::AttachDevelopmentPlugin { .. }
            | ClientCommand::DetachDevelopmentPlugin { .. }
    ) {
        let source = match &command {
            ClientCommand::AttachDevelopmentPlugin { source, .. } => {
                Some(std::path::PathBuf::from(source))
            }
            ClientCommand::DetachDevelopmentPlugin { .. } => None,
            _ => unreachable!("development command guard"),
        };
        let accepted = CommandOutcome::Accepted {};
        send_ack(state, events, &meta, session, accepted.clone());
        let _ = respond.send(accepted);
        super::command_job::start_development(
            meta,
            source,
            super::command_job::CommandReply::Control(completion.take()),
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
        );
        return false;
    }
    let accepted = CommandOutcome::Accepted {};
    send_ack(state, events, &meta, session, accepted.clone());
    let _ = respond.send(accepted);
    apply_accepted(
        command,
        meta,
        completion,
        precommitted_answer,
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
    true
}
