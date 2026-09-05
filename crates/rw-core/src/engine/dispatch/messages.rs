use crate::engine::AgentLoopError;
use crate::engine::MessageDisposition;
use crate::engine::apply_mode_change;
use crate::engine::apply_permission_mode_change;
use crate::engine::commands::SessionCommandAction;
use crate::engine::commands::SessionCommandContext;
use crate::engine::commands::render_context_snapshot;
use crate::engine::commands::render_cost_snapshot;
use crate::engine::commands::render_permission_approvals;
use crate::engine::commands::render_permission_snapshot;
use crate::engine::commands::render_plan;
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
use crate::engine::session_extension::SessionExtensionSnapshot;
use crate::engine::turn::CommandTurnOverrides;
use crate::engine::turn::StartTurnRuntime;
use crate::engine::turn::assemble_session_context;
use crate::engine::turn::build_cost_snapshot;
use crate::engine::turn::context_snapshot;
use crate::engine::turn::emit;
use crate::engine::turn::start_turn;
use crate::engine::turn::start_turn_with_overrides;
use crate::engine::wire_turn_id;
use rw_tools::ToolContext;
use rw_types::Attachment;
use rw_types::CommandMeta;
use rw_types::EngineEvent;
use rw_types::SessionMode;
use std::sync::Arc;
use tokio::sync::oneshot;

#[allow(clippy::too_many_lines, clippy::similar_names)]
pub(super) async fn dispatch_message(
    command_meta: CommandMeta,
    content: String,
    attachments: Vec<Attachment>,
    observed_turn: u64,
    respond: oneshot::Sender<Result<MessageDisposition, AgentLoopError>>,
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
    if content.trim_start().starts_with('/') {
        let mut context = SessionCommandContext {
            session_id: config.session_id.clone(),
            running: state.running.is_some() || state.initialization_running,
            queued_messages: state.queued.len(),
            mode: state.mode,
            mode_id: state.mode_id.clone(),
            modes: Arc::clone(&config.modes),
            permission_summary: render_permission_snapshot(&config.permissions.snapshot()),
            plan_summary: state
                .pending_plan
                .as_ref()
                .or(state.approved_plan.as_ref())
                .map_or_else(|| "no plan has been submitted".to_owned(), render_plan),
            command_summary: config
                .commands
                .descriptors()
                .map(|descriptor| {
                    descriptor.argument_hint().map_or_else(
                        || format!("/{} — {}", descriptor.name(), descriptor.description()),
                        |hint| {
                            format!(
                                "/{} {} — {}",
                                descriptor.name(),
                                hint,
                                descriptor.description()
                            )
                        },
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let result = config.commands.dispatch_line(&mut context, &content).await;
        let disposition = match result {
            Ok(mut output) => {
                let mut unrestorable_paths = Vec::new();
                let mut submitted_prompt = None;
                let mut deferred_command_completion = false;
                match output.action {
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
                        let snapshot = assemble_session_context(
                            config,
                            &state.conversation,
                            &state.queued,
                            &state.context_surgery,
                            &state.pruned_tool_outputs,
                            false,
                        )
                        .map(|assembled| {
                            context_snapshot(
                                &assembled,
                                &state.conversation,
                                &state.pruned_tool_outputs,
                                config.model.context_metadata(&config.model_alias),
                                &config.model.compaction_config(),
                                state
                                    .running
                                    .as_ref()
                                    .map(|running| wire_turn_id(running.id)),
                            )
                        });
                        match snapshot {
                            Ok(snapshot) => {
                                send_connection_event(
                                    events,
                                    &command_meta.client_id,
                                    EngineEvent::ContextSnapshotReady {
                                        meta: query_meta(state, &command_meta),
                                        session_id: state.session_id.clone(),
                                        snapshot: snapshot.clone(),
                                    },
                                );
                                output.message = render_context_snapshot(&snapshot);
                            }
                            Err(error) => {
                                let _ = respond.send(Err(error));
                                return;
                            }
                        }
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
                        );
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
                        if let Err(error) = apply_mode_change(
                            state,
                            events,
                            &config.event_sink,
                            mode,
                            &config.modes,
                        )
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
                            let _ =
                                respond.send(Err(AgentLoopError::InvalidConfiguration(message)));
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
                    SessionCommandAction::AddWorkspaceRoot { path } => {
                        let current_roots = std::iter::once(config.workspace_root.clone())
                            .chain(config.additional_workspace_roots.iter().cloned())
                            .collect::<Vec<_>>();
                        let generation = match config
                            .workspace_roots
                            .append_root(
                                &path,
                                &current_roots,
                                config.workspace_generation,
                                state.next_turn,
                                Arc::clone(&config.permissions),
                            )
                            .await
                        {
                            Ok(generation) => generation,
                            Err(error) => {
                                let _ = respond.send(Err(error));
                                return;
                            }
                        };
                        let valid_append = generation.generation
                            == config.workspace_generation.saturating_add(1)
                            && generation.effective_from_turn == state.next_turn
                            && generation.roots.len() == current_roots.len() + 1
                            && generation
                                .roots
                                .iter()
                                .take(current_roots.len())
                                .eq(current_roots.iter())
                            && generation.roots.iter().all(|root| {
                                std::fs::canonicalize(root)
                                    .is_ok_and(|canonical| canonical == *root)
                            });
                        if !valid_append {
                            let _ = config
                                .workspace_roots
                                .abort_generation(generation.generation)
                                .await;
                            let _ = respond.send(Err(
                            AgentLoopError::InvalidConfiguration(
                                "workspace root controller returned a non-canonical or non-append generation"
                                    .to_owned(),
                            ),
                        ));
                            return;
                        }
                        let replacement_context =
                            match ToolContext::from_workspace_roots(&generation.roots) {
                                Ok(context) => context
                                    .with_session_id(config.session_id.clone())
                                    .with_mcp_tool_policy(config.tools.mcp_tool_policy().clone()),
                                Err(_error) => {
                                    let _ = config
                                        .workspace_roots
                                        .abort_generation(generation.generation)
                                        .await;
                                    let _ = respond.send(Err(AgentLoopError::ToolContext(
                                        "workspace tool context could not prepare".to_owned(),
                                    )));
                                    return;
                                }
                            };
                        let descriptors = generation
                            .roots
                            .iter()
                            .enumerate()
                            .map(|(index, _root)| rw_types::WorkspaceRootDescriptor {
                                index: u32::try_from(index).unwrap_or(u32::MAX),
                                path: format!("@root/{index}"),
                                machine_local: false,
                            })
                            .collect::<Vec<_>>();
                        if let Err(_error) = config
                            .workspace_roots
                            .prepare_commit_generation(generation.generation)
                            .await
                        {
                            let _ = config
                                .workspace_roots
                                .abort_generation(generation.generation)
                                .await;
                            let _ = respond.send(Err(AgentLoopError::Persistence(
                                "workspace root generation could not commit".to_owned(),
                            )));
                            return;
                        }
                        if let Err(_error) = emit(
                            state,
                            events,
                            &config.event_sink,
                            PendingEvent::WorkspaceRootsChanged {
                                generation: generation.generation,
                                effective_from_turn: generation.effective_from_turn,
                                roots: descriptors,
                            },
                        )
                        .await
                        .map(|_| ())
                        {
                            let _ = config
                                .workspace_roots
                                .abort_generation(generation.generation)
                                .await;
                            let _ = respond.send(Err(AgentLoopError::Persistence(
                                "workspace root change event could not persist".to_owned(),
                            )));
                            return;
                        }
                        config
                            .workspace_roots
                            .finalize_generation(generation.generation);
                        let base_config =
                            config.with_workspace_generation(&generation, &state.mode_id);
                        let rebase = config
                            .extension_development
                            .rebase(SessionExtensionSnapshot {
                                ui: Arc::clone(&config.ui),
                                revision: base_config.workspace_generation,
                                workspace_roots: Arc::from(generation.roots.clone()),
                                tools: Arc::clone(&base_config.tools),
                                hooks: Arc::clone(&base_config.hooks),
                                commands: Arc::clone(&base_config.commands),
                            })
                            .await;
                        let (rebased, development_detached) = match rebase {
                            Ok(result) => result,
                            Err(error) => {
                                state.unsettled = Some(error.to_string());
                                state.tasks.cancel();
                                let _ = respond.send(Err(error));
                                return;
                            }
                        };
                        let next_config = Arc::new(base_config.with_extension_snapshot(&rebased));
                        *command_descriptors
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::from(
                            next_config
                                .commands
                                .descriptors()
                                .cloned()
                                .collect::<Vec<_>>(),
                        );
                        *mode_registry
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Arc::clone(&next_config.modes);
                        *config = next_config;
                        *tool_context = replacement_context;
                        output.message =
                            format!("added workspace root @root/{}", generation.roots.len() - 1);
                        if development_detached {
                            output.message.push_str(
                                "; detached the development plugin after a registry collision",
                            );
                        }
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
                Err(persisted
                    .err()
                    .unwrap_or_else(|| AgentLoopError::Extension(error.to_string())))
            }
        };
        let _ = respond.send(disposition);
    } else if state.initialization_running {
        let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(
            "workspace initialization is still running".to_owned(),
        )));
    } else if state.running.is_some() {
        let content = config.secret_redactor.redact(&content);
        let Some(position) = state
            .queued_positions
            .back()
            .copied()
            .unwrap_or(0)
            .checked_add(1)
        else {
            let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(
                "queued message position space is exhausted".to_owned(),
            )));
            return;
        };
        state.queued.push_back(content.clone());
        state.queued_positions.push_back(position);
        let persisted = emit(
            state,
            events,
            &config.event_sink,
            PendingEvent::MessageQueued {
                position,
                content,
                attachments: Vec::new(),
            },
        )
        .await
        .map(|_| ());
        if let Err(error) = persisted {
            state.queued.pop_back();
            state.queued_positions.pop_back();
            let _ = respond.send(Err(error));
        } else {
            let _ = respond.send(Ok(MessageDisposition::Queued));
        }
    } else {
        let result = start_turn(
            state,
            config,
            tool_context,
            turn_signals,
            events,
            vec![(content, attachments)],
            active_turn,
        )
        .await;
        let _ = respond.send(result.map(|()| MessageDisposition::Started));
    }
}
