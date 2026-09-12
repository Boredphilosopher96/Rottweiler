//! Applies an already settled command result under the captured actor authority.
use crate::engine::AgentLoopError;
use crate::engine::MessageDisposition;
use crate::engine::apply_mode_change;
use crate::engine::apply_permission_mode_change;
use crate::engine::commands::SessionCommandAction;
use crate::engine::commands::render_cost_snapshot;
use crate::engine::commands::render_permission_approvals;
use crate::engine::commands::render_permission_snapshot;
use crate::engine::commands::render_session_review;
use crate::engine::dispatch::DispatchContext;
use crate::engine::dispatch::compaction::start_manual_compaction;
use crate::engine::dispatch::context_surgery::apply_registered_context_surgery;
use crate::engine::dispatch::initialization::start_workspace_initialization;
use crate::engine::dispatch::replies::query_meta;
use crate::engine::dispatch::replies::send_connection_event;
use crate::engine::dispatch::rewind::rewind_state;
use crate::engine::mode_permission_base;
use crate::engine::pending_event::PendingEvent;
use crate::engine::turn::CommandTurnOverrides;
use crate::engine::turn::StartTurnRuntime;
use crate::engine::turn::build_cost_snapshot;
use crate::engine::turn::emit;
use crate::engine::turn::start_turn_with_overrides;
use rw_types::CommandMeta;
use rw_types::EngineEvent;
use rw_types::SessionMode;
use std::sync::Arc;

#[allow(clippy::too_many_lines, clippy::similar_names)]
pub(super) async fn apply(
    command_meta: CommandMeta,
    content: String,
    observed_turn: u64,
    result: super::command_job::Execution,
    respond: super::command_job::CommandReply,
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
    let disposition = match result {
        Ok(prepared) => {
            let super::command_job::PreparedCommand { mut output, change } = prepared;
            if let Err(error) = super::command_generation::apply(
                change,
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
            .await
            {
                if matches!(error, AgentLoopError::EffectsUnsettled(_)) {
                    state.unsettled = Some(error.to_string());
                    state.tasks.cancel();
                }
                let _ = respond.send(Err(error));
                return;
            }
            let mut unrestorable_paths = Vec::new();
            let mut submitted_prompt = None;
            let mut deferred_command_completion = false;
            match output.action {
                SessionCommandAction::Navigate { target } => {
                    if let Err(error) =
                        super::navigation::request(state, events, &command_meta, target)
                    {
                        let _ = respond.send(Err(error));
                        return;
                    }
                }
                SessionCommandAction::Interrupt => {
                    if let Some(running) = &state.running
                        && running.id == observed_turn
                    {
                        running.cancellation.cancel();
                    }
                }
                SessionCommandAction::Rewind { to_turn } => {
                    match rewind_state(state, config, events, to_turn).await {
                        Ok(report) => unrestorable_paths = report,
                        Err(_error) => {
                            let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(
                                "workspace root generation could not prepare".to_owned(),
                            )));
                            return;
                        }
                    }
                }
                SessionCommandAction::Review => {
                    match config.checkpoints.session_review(&state.session_id).await {
                        Ok(review) => {
                            send_connection_event(
                                events,
                                &command_meta.client_id,
                                EngineEvent::SessionReviewReady {
                                    meta: query_meta(state, &command_meta),
                                    session_id: state.session_id.clone(),
                                    review: review.clone(),
                                },
                            );
                            output.message = render_session_review(&review);
                        }
                        Err(error) => {
                            let _ = respond.send(Err(error));
                            return;
                        }
                    }
                }
                SessionCommandAction::Context => {
                    super::context_job::start(
                        state,
                        config,
                        super::context_job::Target::Command {
                            meta: command_meta,
                            reply: respond,
                        },
                    );
                    return;
                }
                SessionCommandAction::PinContext { item_id } => {
                    if let Err(error) = apply_registered_context_surgery(
                        state,
                        config,
                        events,
                        item_id.clone(),
                        true,
                    )
                    .await
                    {
                        let _ = respond.send(Err(error));
                        return;
                    }
                    output.message = format!("pinned {}", item_id.0);
                }
                SessionCommandAction::EvictContext { item_id } => {
                    if let Err(error) = apply_registered_context_surgery(
                        state,
                        config,
                        events,
                        item_id.clone(),
                        false,
                    )
                    .await
                    {
                        let _ = respond.send(Err(error));
                        return;
                    }
                    output.message = format!("evicted {}", item_id.0);
                }
                SessionCommandAction::Cost => match build_cost_snapshot(state, config).await {
                    Ok(snapshot) => {
                        send_connection_event(
                            events,
                            &command_meta.client_id,
                            EngineEvent::CostSnapshotReady {
                                meta: query_meta(state, &command_meta),
                                session_id: state.session_id.clone(),
                                snapshot: snapshot.clone(),
                            },
                        );
                        output.message = render_cost_snapshot(&snapshot);
                    }
                    Err(error) => {
                        let _ = respond.send(Err(error));
                        return;
                    }
                },
                SessionCommandAction::Compact { instructions } => {
                    start_manual_compaction(
                        state,
                        config,
                        turn_signals,
                        active_turn,
                        instructions,
                        None,
                        None,
                    )
                    .await;
                }
                SessionCommandAction::SwitchMode { mode } => {
                    let base = config
                        .modes
                        .get(&mode.0)
                        .map(mode_permission_base)
                        .ok_or_else(|| {
                            AgentLoopError::InvalidConfiguration(format!(
                                "unknown mode {:?}",
                                mode.0
                            ))
                        });
                    let base = match base {
                        Ok(base) => base,
                        Err(error) => {
                            let _ = respond.send(Err(error));
                            return;
                        }
                    };
                    if base == SessionMode::Execute && state.plan_gate_active {
                        let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(
                            "plan_approval_required: submit and approve a plan before Execute"
                                .to_owned(),
                        )));
                        return;
                    }
                    if let Err(error) =
                        apply_mode_change(state, events, &config.event_sink, mode, &config.modes)
                            .await
                    {
                        let _ = respond.send(Err(error));
                        return;
                    }
                }
                SessionCommandAction::SetPermissionMode { mode } => {
                    if let Err(error) =
                        apply_permission_mode_change(state, events, config, mode).await
                    {
                        let _ = respond.send(Err(error));
                        return;
                    }
                    output.message = render_permission_snapshot(&config.permissions.snapshot());
                }
                SessionCommandAction::AddPermissionRule { rule } => {
                    if let Err(message) = config.permissions.add_session_rule(rule.clone()) {
                        let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(message)));
                        return;
                    }
                    output.message = format!(
                        "added session permission rule: {:?} {}",
                        rule.action, rule.pattern
                    );
                }
                SessionCommandAction::RemovePermissionRule { pattern } => {
                    output.message = if config.permissions.remove_session_rule(&pattern) {
                        format!("removed session permission rule: {pattern}")
                    } else {
                        format!("no session permission rule matched: {pattern}")
                    };
                }
                SessionCommandAction::ClearSessionPermissions => {
                    let cleared = config.permissions.clear_session_permissions();
                    output.message = format!(
                        "cleared {} session permission rule(s) and {} remembered approval(s)",
                        cleared.rules, cleared.approvals
                    );
                }
                SessionCommandAction::ListPermissionApprovals => {
                    output.message =
                        render_permission_approvals(&config.permissions.approval_snapshot());
                }
                SessionCommandAction::RevokeSessionApprovals { id } => {
                    let removed = config.permissions.revoke_session_approvals(id.as_deref());
                    output.message = format!("revoked {removed} session approval(s)");
                }
                SessionCommandAction::RevokeProjectApprovals { id } => {
                    match config.permissions.revoke_project_approvals(id.as_deref()) {
                        Ok(removed) => {
                            output.message = format!("revoked {removed} project approval(s)");
                        }
                        Err(error) => {
                            let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(
                                format!("project approval revocation failed: {error}"),
                            )));
                            return;
                        }
                    }
                }
                SessionCommandAction::AddWorkspaceRoot { .. } => {
                    let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(
                        "workspace action reached completion without preparation".into(),
                    )));
                    return;
                }
                SessionCommandAction::Trust { operation } => {
                    match config.folder_trust.execute(operation).await {
                        Ok(message) => output.message = message,
                        Err(error) => {
                            let _ = respond.send(Err(error));
                            return;
                        }
                    }
                }
                SessionCommandAction::InitializeWorkspace { depth } => {
                    if state.running.is_some()
                        || state.initialization_running
                        || config.tools.session_activity(&state.session_id).is_some()
                    {
                        let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(
                            "workspace initialization requires an idle session".to_owned(),
                        )));
                        return;
                    }
                    let call_id = format!(
                        "command-init-{}-{}",
                        state.next_turn,
                        state
                            .sequence
                            .map_or(0, |sequence| sequence.saturating_add(1))
                    );
                    state.initialization_running = true;
                    start_workspace_initialization(
                        Arc::clone(config),
                        &state.tasks,
                        depth,
                        state.next_turn,
                        call_id,
                        turn_signals.clone(),
                    );
                    deferred_command_completion = true;
                }
                SessionCommandAction::SubmitPrompt {
                    content,
                    model_alias,
                    allowed_tools,
                    permission_patterns,
                    tool_calls,
                } => {
                    if state.running.is_some() {
                        let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(
                            "custom commands require an idle session".to_owned(),
                        )));
                        return;
                    }
                    submitted_prompt = Some((
                        content,
                        CommandTurnOverrides {
                            model_alias,
                            allowed_tools,
                            permission_patterns,
                            tool_calls,
                        },
                    ));
                }
                SessionCommandAction::None => {}
            }
            if deferred_command_completion {
                let _ = respond.send(Ok(MessageDisposition::Command));
                return;
            }
            let name = content
                .trim_start()
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned();
            let persisted = emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::CommandFinished {
                    name,
                    message: output.message,
                    unrestorable_paths,
                },
            )
            .await
            .map(|_| ());
            match (persisted, submitted_prompt) {
                (Err(error), _) => Err(error),
                (Ok(()), None) => Ok(MessageDisposition::Command),
                (Ok(()), Some((prompt, overrides))) => start_turn_with_overrides(
                    state,
                    StartTurnRuntime {
                        config,
                        tool_context,
                        signals: turn_signals,
                        events,
                        active_turn,
                    },
                    vec![(prompt, Vec::new())],
                    overrides,
                )
                .await
                .map(|()| MessageDisposition::Started),
            }
        }
        Err(error) => {
            let persisted = emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::Error {
                    message: error.to_string(),
                },
            )
            .await
            .map(|_| ());
            Err(persisted.err().unwrap_or(error))
        }
    };
    let _ = respond.send(disposition);
}
