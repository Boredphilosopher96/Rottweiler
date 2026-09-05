use crate::engine::AgentTurnStatus;
use crate::engine::PreparedUserMessage;
use crate::engine::SessionUsage;
use crate::engine::TEXT_DELTA_COALESCE_WINDOW;
use crate::engine::commands::CommandToolCall;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::ContextSurgeryAction;
use crate::engine::session::SessionActorConfig;
use crate::engine::task_ownership;
use crate::engine::turn::accounting::BudgetUsage;
use crate::engine::turn::accounting::SessionAccountingFallback;
use crate::engine::turn::accounting::combine_cost;
use crate::engine::turn::accounting::cost_units;
use crate::engine::turn::accounting::evaluate_budget;
use crate::engine::turn::accounting::persist_incomplete_budget_caps;
use crate::engine::turn::command_tools::CommandToolRuntime;
use crate::engine::turn::command_tools::apply_command_tool_calls;
use crate::engine::turn::compaction::compact_during_turn;
use crate::engine::turn::context::assemble_session_context;
use crate::engine::turn::context::context_snapshot;
use crate::engine::turn::context::prune_before_provider_request;
use crate::engine::turn::context::resolved_overflow_policy;
use crate::engine::turn::hooks::dispatch_hook;
use crate::engine::turn::hooks::hook_rejection;
use crate::engine::turn::hooks::mark_unsettled;
use crate::engine::turn::hooks::report_hook_failures;
use crate::engine::turn::provider_calls;
use crate::engine::turn::provider_messages::append_text;
use crate::engine::turn::provider_messages::append_thinking;
use crate::engine::turn::provider_messages::flush_pending_text_delta;
use crate::engine::turn::provider_messages::persist_conversation_turn;
use crate::engine::turn::provider_messages::persist_event;
use crate::engine::turn::provider_messages::send_event;
use crate::engine::turn::redaction::redacted_json;
use crate::engine::turn::signals::TurnOutcome;
use crate::engine::turn::signals::TurnSignal;
use crate::engine::turn::tool_requests::ChannelApprover;
use crate::engine::turn::tool_requests::PendingToolCall;
use crate::engine::turn::tool_scheduling::DoomLoopGuard;
use crate::engine::turn::tool_scheduling::execute_tool_calls;
use crate::engine::unavailable_cost;
use crate::engine::wire_turn_id;
use futures_util::StreamExt;
use rw_context::Budgeter;
use rw_ext::HookEvent;
use rw_providers::CacheHint;
use rw_providers::FinishReason;
use rw_providers::ProviderEvent;
use rw_providers::ProviderRequest;
use rw_providers::TokenUsage;
use rw_providers::ToolChoice;
use rw_tools::CancellationToken;
use rw_tools::ToolContext;
use rw_types::AccountingAttribution;
use rw_types::Block;
use rw_types::CompactionReason;
use rw_types::RequestId;
use rw_types::Role;
use rw_types::SessionMode;
use rw_types::ToolCallId;
use rw_types::Turn;
use rw_types::TurnMeta;
use rw_types::hook_contract::HookInput;
use rw_types::hook_contract::HookPromptInput;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::mpsc;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[tracing::instrument(target = "rw_performance", level = "trace", name = "turn.run", skip_all, fields(session_id = config.session_id.0.as_str(), turn))]
pub(super) async fn run_turn(
    turn: u64,
    tasks: task_ownership::ActorTasks,
    mut messages: Vec<PreparedUserMessage>,
    command_tool_calls: Vec<CommandToolCall>,
    mut conversation: Vec<Turn>,
    config: Arc<SessionActorConfig>,
    tool_context: ToolContext,
    cancellation: CancellationToken,
    signals: mpsc::UnboundedSender<TurnSignal>,
    mut context_surgery: Vec<ContextSurgeryAction>,
    mut pruned_tool_outputs: BTreeMap<String, u64>,
    mut budgeter: Budgeter,
    local_session_accounting: SessionAccountingFallback,
    mode: SessionMode,
) -> TurnOutcome {
    let approver = ChannelApprover {
        signals: signals.clone(),
        cancellation: cancellation.clone(),
    };
    if apply_command_tool_calls(
        turn,
        &mut messages,
        command_tool_calls,
        CommandToolRuntime {
            tasks: &tasks,
            config: &config,
            context: &tool_context,
            cancellation: &cancellation,
            approver: &approver,
            signals: &signals,
            mode,
        },
    )
    .await
    .is_err()
    {
        return TurnOutcome {
            turn,
            conversation: crate::engine::session::ConversationSummary::from_turns(&conversation),
            status: AgentTurnStatus::Failed,
            usage: SessionUsage::default(),
            cost: unavailable_cost(),
            deferred_terminal_delta: None,
            deferred_terminal_turn: None,
            budgeter,
        };
    }
    for message in messages {
        let Ok(hook) = dispatch_hook(
            &config.hooks,
            HookInput::UserPromptSubmit(HookPromptInput {
                content: message.content.clone(),
            }),
            &cancellation,
            &signals,
        )
        .await
        else {
            return TurnOutcome {
                turn,
                conversation: crate::engine::session::ConversationSummary::from_turns(
                    &conversation,
                ),
                status: AgentTurnStatus::Interrupted,
                usage: SessionUsage::default(),
                cost: unavailable_cost(),
                deferred_terminal_delta: None,
                deferred_terminal_turn: None,
                budgeter,
            };
        };
        report_hook_failures(
            HookEvent::UserPromptSubmit,
            hook.failures(),
            &signals,
            config.secret_redactor.as_ref(),
        );
        if !hook.completed() {
            return TurnOutcome {
                turn,
                conversation: crate::engine::session::ConversationSummary::from_turns(
                    &conversation,
                ),
                status: AgentTurnStatus::Failed,
                usage: SessionUsage::default(),
                cost: unavailable_cost(),
                deferred_terminal_delta: None,
                deferred_terminal_turn: None,
                budgeter,
            };
        }
        let HookInput::UserPromptSubmit(input) = hook.input() else {
            unreachable!("dispatcher preserves hook phase")
        };
        let content = config.secret_redactor.redact(&input.content);
        let user_turn = message.turn(content);
        conversation.push(user_turn.clone());
        if persist_event(
            &signals,
            PendingEvent::ConversationTurnCommitted {
                agent_turn: turn,
                turn: user_turn,
            },
        )
        .await
        .is_err()
        {
            return TurnOutcome {
                turn,
                conversation: crate::engine::session::ConversationSummary::from_turns(
                    &conversation,
                ),
                status: AgentTurnStatus::Failed,
                usage: SessionUsage::default(),
                cost: unavailable_cost(),
                deferred_terminal_delta: None,
                deferred_terminal_turn: None,
                budgeter,
            };
        }
    }

    let mut usage = SessionUsage::default();
    let mut doom = DoomLoopGuard::new(config.identical_tool_failure_limit);
    let mut status = AgentTurnStatus::MaxTurns;
    let mut deferred_terminal_delta = None;
    let mut deferred_terminal_turn = None;
    let mut current_turn_cost_micros = 0_u64;
    let mut current_turn_credit_micros = 0_u64;
    let mut current_turn_tokens = 0_u64;
    let budget_config = config.model.budget_config();
    let mut turn_cost = None;
    let mut citation_admission = rw_types::citation_admission::CitationAdmission::default();

    'iterations: for iteration in 0..config.max_turns {
        if cancellation.is_cancelled() {
            status = AgentTurnStatus::Interrupted;
            break;
        }
        let budget = match evaluate_budget(
            turn,
            config.event_clock.as_ref(),
            &config.event_sink,
            &budget_config,
            local_session_accounting,
            BudgetUsage {
                cost_micros_usd: current_turn_cost_micros,
                ai_credit_micros: current_turn_credit_micros,
                subscription_tokens: current_turn_tokens,
            },
        )
        .await
        {
            Ok(check) => check,
            Err(error) => {
                send_event(
                    &signals,
                    PendingEvent::Error {
                        message: error.to_string(),
                    },
                );
                status = AgentTurnStatus::Failed;
                break;
            }
        };
        for event in budget.events {
            if persist_event(&signals, event).await.is_err() {
                status = AgentTurnStatus::Failed;
                break 'iterations;
            }
        }
        if budget.hard_stop {
            status = AgentTurnStatus::BudgetExceeded;
            break;
        }
        if prune_before_provider_request(
            &conversation,
            &context_surgery,
            &mut pruned_tool_outputs,
            &signals,
        )
        .await
        .is_err()
        {
            status = AgentTurnStatus::Failed;
            break;
        }
        let mut assembled = match assemble_session_context(
            &config,
            &conversation,
            &VecDeque::new(),
            &context_surgery,
            &pruned_tool_outputs,
            false,
        ) {
            Ok(assembled) => assembled,
            Err(error) => {
                send_event(
                    &signals,
                    PendingEvent::Error {
                        message: error.to_string(),
                    },
                );
                status = AgentTurnStatus::Failed;
                break;
            }
        };
        let metadata = config.model.context_metadata(&config.model_alias);
        let compaction = config.model.compaction_config();
        let mut input_estimate = budgeter.estimate(&assembled.turns, &assembled.tools);
        if let Ok(Some(policy)) = resolved_overflow_policy(metadata, &compaction) {
            let overflow = policy.calculate(input_estimate.reconciled_tokens);
            if overflow.should_compact {
                match compact_during_turn(
                    turn,
                    &mut conversation,
                    &mut context_surgery,
                    CompactionReason::Automatic,
                    &config,
                    &cancellation,
                    &signals,
                    local_session_accounting,
                    current_turn_cost_micros,
                    current_turn_credit_micros,
                    current_turn_tokens,
                    None,
                )
                .await
                {
                    Ok((cost_micros, credit_micros, tokens, hard_stop)) => {
                        current_turn_cost_micros =
                            current_turn_cost_micros.saturating_add(cost_micros);
                        current_turn_credit_micros =
                            current_turn_credit_micros.saturating_add(credit_micros);
                        current_turn_tokens = current_turn_tokens.saturating_add(tokens);
                        if hard_stop {
                            status = AgentTurnStatus::BudgetExceeded;
                            break;
                        }
                    }
                    Err(error) => {
                        send_event(
                            &signals,
                            PendingEvent::Error {
                                message: error.to_string(),
                            },
                        );
                        status = AgentTurnStatus::Failed;
                        break;
                    }
                }
                assembled = match assemble_session_context(
                    &config,
                    &conversation,
                    &VecDeque::new(),
                    &context_surgery,
                    &pruned_tool_outputs,
                    false,
                ) {
                    Ok(assembled) => assembled,
                    Err(error) => {
                        send_event(
                            &signals,
                            PendingEvent::Error {
                                message: error.to_string(),
                            },
                        );
                        status = AgentTurnStatus::Failed;
                        break;
                    }
                };
                input_estimate = budgeter.estimate(&assembled.turns, &assembled.tools);
            }
        }
        let mut snapshot = context_snapshot(
            &assembled,
            &conversation,
            &pruned_tool_outputs,
            metadata,
            &compaction,
            Some(wire_turn_id(turn)),
        );
        snapshot.used_tokens = input_estimate.reconciled_tokens;
        let context_metrics = (
            snapshot.used_tokens,
            snapshot.usable_tokens,
            snapshot.reserved_tokens,
            snapshot.context_window_known,
            snapshot.context_window_reason.clone(),
            snapshot.stable_prefix_hash.clone(),
        );
        send_event(
            &signals,
            PendingEvent::ContextUsage {
                turn,
                used_tokens: snapshot.used_tokens,
                usable_tokens: snapshot.usable_tokens,
                reserved_tokens: snapshot.reserved_tokens,
                context_window_known: snapshot.context_window_known,
                context_window_reason: snapshot.context_window_reason,
                stable_prefix_hash: snapshot.stable_prefix_hash,
                cache_hit_basis_points: 0,
                estimated_input_tokens: input_estimate.local_tokens,
                provider_input_tokens: 0,
                correction_millionths: input_estimate.correction_millionths,
            },
        );
        let cache_hint = (assembled.stable_prefix_turn_count > 0 || !assembled.tools.is_empty())
            .then(|| CacheHint {
                stable_prefix_turns: u32::try_from(assembled.stable_prefix_turn_count)
                    .unwrap_or(u32::MAX),
                tools_in_prefix: !assembled.tools.is_empty(),
            });
        let request = ProviderRequest {
            model: config.model_alias.clone(),
            turns: assembled.turns,
            tools: assembled.tools,
            tool_choice: ToolChoice::Auto {},
            max_output_tokens: config.max_output_tokens,
            temperature: None,
            thinking: config.thinking,
            cache_hint,
        };
        if let Err(error) = config.model.prepare_model(&config.model_alias).await {
            send_event(
                &signals,
                PendingEvent::Error {
                    message: error.to_string(),
                },
            );
            status = AgentTurnStatus::Failed;
            break;
        }
        let provider_started = tracing::enabled!(target: "rw_performance", tracing::Level::TRACE)
            .then(std::time::Instant::now);
        let mut first_provider_event = true;
        let invocation = match provider_calls::invocation(
            &config,
            &signals,
            turn,
            AccountingAttribution::Main,
            &request,
        ) {
            Ok(invocation) => invocation,
            Err(error) => {
                send_event(
                    &signals,
                    PendingEvent::Error {
                        message: error.to_string(),
                    },
                );
                status = AgentTurnStatus::Failed;
                break;
            }
        };
        let mut stream = match config.model.stream_for_provider(
            &config.model_alias,
            config.recovered.provider.as_deref(),
            request,
            invocation,
        ) {
            Ok(stream) => stream,
            Err(error) => {
                send_event(
                    &signals,
                    PendingEvent::Error {
                        message: error.to_string(),
                    },
                );
                status = AgentTurnStatus::Failed;
                break;
            }
        };
        let mut assistant = Turn {
            role: Role::Assistant,
            blocks: Vec::new(),
            meta: TurnMeta::default(),
        };
        let mut selected_route = None;
        let mut calls = Vec::<PendingToolCall>::new();
        let mut tool_admission = super::tool_admission::PendingToolBudget::default();
        let mut tool_admission_failed = false;
        let mut finish_reason = None;
        let mut iteration_usage = SessionUsage::default();
        let mut stream_failed = false;
        let mut provider_overflow_recovered = false;
        let mut pending_text_delta = None;
        let mut pending_text_delta_deadline = None;
        loop {
            let next = if let Some(deadline) = pending_text_delta_deadline {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => {
                        flush_pending_text_delta(
                            &mut pending_text_delta,
                            &mut pending_text_delta_deadline,
                            &signals,
                            turn,
                        );
                        status = AgentTurnStatus::Interrupted;
                        break;
                    }
                    () = tokio::time::sleep_until(deadline) => {
                        flush_pending_text_delta(
                            &mut pending_text_delta,
                            &mut pending_text_delta_deadline,
                            &signals,
                            turn,
                        );
                        continue;
                    }
                    event = stream.next() => event,
                }
            } else {
                tokio::select! {
                    () = cancellation.cancelled() => {
                        status = AgentTurnStatus::Interrupted;
                        break;
                    }
                    event = stream.next() => event,
                }
            };
            let Some(event) = next else {
                flush_pending_text_delta(
                    &mut pending_text_delta,
                    &mut pending_text_delta_deadline,
                    &signals,
                    turn,
                );
                break;
            };
            if first_provider_event {
                if let Some(started) = provider_started {
                    tracing::trace!(target: "rw_performance", stage = "provider.first_event",
                        elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
                }
                first_provider_event = false;
            }
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    flush_pending_text_delta(
                        &mut pending_text_delta,
                        &mut pending_text_delta_deadline,
                        &signals,
                        turn,
                    );
                    if error.kind == rw_providers::ProviderErrorKind::ContextOverflow
                        && assistant.blocks.is_empty()
                        && calls.is_empty()
                    {
                        match compact_during_turn(
                            turn,
                            &mut conversation,
                            &mut context_surgery,
                            CompactionReason::ProviderOverflow,
                            &config,
                            &cancellation,
                            &signals,
                            local_session_accounting,
                            current_turn_cost_micros,
                            current_turn_credit_micros,
                            current_turn_tokens,
                            None,
                        )
                        .await
                        {
                            Ok((cost_micros, credit_micros, tokens, hard_stop)) => {
                                current_turn_cost_micros =
                                    current_turn_cost_micros.saturating_add(cost_micros);
                                current_turn_credit_micros =
                                    current_turn_credit_micros.saturating_add(credit_micros);
                                current_turn_tokens = current_turn_tokens.saturating_add(tokens);
                                if hard_stop {
                                    status = AgentTurnStatus::BudgetExceeded;
                                    stream_failed = true;
                                    break;
                                }
                                provider_overflow_recovered = true;
                                break;
                            }
                            Err(compaction_error) => {
                                send_event(
                                    &signals,
                                    PendingEvent::Error {
                                        message: compaction_error.to_string(),
                                    },
                                );
                                status = AgentTurnStatus::Failed;
                                stream_failed = true;
                                break;
                            }
                        }
                    }
                    send_event(
                        &signals,
                        PendingEvent::Error {
                            message: error.to_string(),
                        },
                    );
                    status = if error.kind == rw_providers::ProviderErrorKind::Cancelled {
                        AgentTurnStatus::Interrupted
                    } else {
                        AgentTurnStatus::Failed
                    };
                    stream_failed = true;
                    break;
                }
            };
            if !matches!(
                &event,
                ProviderEvent::TextDelta { .. } | ProviderEvent::Finished { .. }
            ) {
                flush_pending_text_delta(
                    &mut pending_text_delta,
                    &mut pending_text_delta_deadline,
                    &signals,
                    turn,
                );
            }
            match event {
                ProviderEvent::RouteSelected { route } => selected_route = Some(route),
                ProviderEvent::MessageStart { model } => assistant.meta.model = Some(model),
                ProviderEvent::TextDelta { text } => {
                    let text = config.secret_redactor.redact(&text);
                    append_text(&mut assistant.blocks, &text);
                    pending_text_delta
                        .get_or_insert_with(String::new)
                        .push_str(&text);
                    pending_text_delta_deadline.get_or_insert_with(|| {
                        tokio::time::Instant::now() + TEXT_DELTA_COALESCE_WINDOW
                    });
                }
                ProviderEvent::ThinkingDelta { content, signature } => {
                    let content = config.secret_redactor.redact(&content);
                    append_thinking(&mut assistant.blocks, &content, signature.clone());
                    if !content.is_empty() || signature.is_some() {
                        send_event(
                            &signals,
                            PendingEvent::ThinkingDelta {
                                turn,
                                content,
                                signature,
                            },
                        );
                    }
                }
                ProviderEvent::ToolCallStart { id, name } => {
                    if calls.iter().any(|call| call.id == id) {
                        send_event(
                            &signals,
                            PendingEvent::Error {
                                message: format!("provider repeated tool call id `{id}`"),
                            },
                        );
                        status = AgentTurnStatus::Failed;
                        stream_failed = true;
                        tool_admission_failed = true;
                        break;
                    }
                    if let Err(message) = tool_admission.start(&id, &name) {
                        send_event(&signals, PendingEvent::Error { message });
                        status = AgentTurnStatus::Failed;
                        stream_failed = true;
                        tool_admission_failed = true;
                        break;
                    }
                    calls.push(PendingToolCall {
                        invocation_id: rw_types::ToolInvocationId(format!(
                            "turn-{turn}:iteration-{iteration}:call-{}",
                            calls.len()
                        )),
                        id,
                        name,
                        arguments: None,
                        index: calls.len(),
                    });
                }
                ProviderEvent::ToolCallArgumentsDelta { id, json_fragment } => {
                    if !calls
                        .iter()
                        .any(|call| call.id == id && call.arguments.is_none())
                    {
                        send_event(
                            &signals,
                            PendingEvent::Error {
                                message: "tool arguments require an open tool call".into(),
                            },
                        );
                        status = AgentTurnStatus::Failed;
                        stream_failed = true;
                        tool_admission_failed = true;
                        break;
                    }
                    if let Err(message) = tool_admission.delta(&json_fragment) {
                        send_event(&signals, PendingEvent::Error { message });
                        status = AgentTurnStatus::Failed;
                        stream_failed = true;
                        tool_admission_failed = true;
                        break;
                    }
                }
                ProviderEvent::ToolCallEnd { id, arguments } => {
                    if let Some(call) = calls.iter_mut().find(|call| call.id == id) {
                        if call.arguments.is_some() {
                            send_event(
                                &signals,
                                PendingEvent::Error {
                                    message: format!("provider ended tool call `{id}` twice"),
                                },
                            );
                            status = AgentTurnStatus::Failed;
                            stream_failed = true;
                            tool_admission_failed = true;
                            break;
                        }
                        if let Err(message) = tool_admission.arguments(&arguments) {
                            send_event(&signals, PendingEvent::Error { message });
                            status = AgentTurnStatus::Failed;
                            stream_failed = true;
                            tool_admission_failed = true;
                            break;
                        }
                        call.arguments = Some(arguments.clone());
                        assistant.blocks.push(Block::ToolCall {
                            id: ToolCallId(id),
                            name: call.name.clone(),
                            args: redacted_json(arguments, config.secret_redactor.as_ref()),
                        });
                    } else {
                        send_event(
                            &signals,
                            PendingEvent::Error {
                                message: "provider ended an unknown tool call".to_owned(),
                            },
                        );
                        status = AgentTurnStatus::Failed;
                        stream_failed = true;
                        tool_admission_failed = true;
                        break;
                    }
                }
                ProviderEvent::Citation { uri, title, .. } => {
                    // Reject provider-owned oversized input before redaction copies it.
                    let mut candidate = citation_admission;
                    if let Err(message) = candidate.admit(&uri, title.as_ref(), None) {
                        send_event(
                            &signals,
                            PendingEvent::Error {
                                message: message.to_owned(),
                            },
                        );
                        status = AgentTurnStatus::Failed;
                        stream_failed = true;
                        break;
                    }
                    let uri = config.secret_redactor.redact(&uri);
                    let title = title.map(|title| config.secret_redactor.redact(&title));
                    if let Err(message) = citation_admission.admit(&uri, title.as_ref(), None) {
                        send_event(
                            &signals,
                            PendingEvent::Error {
                                message: message.to_owned(),
                            },
                        );
                        status = AgentTurnStatus::Failed;
                        stream_failed = true;
                        break;
                    }
                    assistant.blocks.push(Block::Citation {
                        uri: uri.clone(),
                        title: title.clone(),
                        excerpt: None,
                    });
                    send_event(&signals, PendingEvent::CitationDelta { turn, uri, title });
                }
                ProviderEvent::Usage { usage: latest } => iteration_usage.update(latest),
                ProviderEvent::Finished { reason } => {
                    if reason == FinishReason::ToolCalls || !calls.is_empty() {
                        flush_pending_text_delta(
                            &mut pending_text_delta,
                            &mut pending_text_delta_deadline,
                            &signals,
                            turn,
                        );
                    }
                    finish_reason = Some(reason);
                    break;
                }
            }
        }
        if let Some(started) = provider_started {
            tracing::trace!(target: "rw_performance", stage = "provider.stream",
                elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
        }
        drop(stream);
        if let Err(error) = config.model.settle_effects().await {
            mark_unsettled(&signals, &cancellation, error.to_string());
            status = AgentTurnStatus::Failed;
            break;
        }
        let normalized_iteration_usage: TokenUsage = iteration_usage.into();
        let reconciliation =
            budgeter.reconcile(input_estimate.local_tokens, normalized_iteration_usage);
        let provider_input_tokens = normalized_iteration_usage
            .input_tokens
            .saturating_add(normalized_iteration_usage.cache_read_tokens)
            .saturating_add(normalized_iteration_usage.cache_write_tokens);
        let cache_hit_basis_points = if provider_input_tokens == 0 {
            0
        } else {
            u16::try_from(
                u128::from(normalized_iteration_usage.cache_read_tokens).saturating_mul(10_000)
                    / u128::from(provider_input_tokens),
            )
            .unwrap_or(10_000)
        };
        send_event(
            &signals,
            PendingEvent::ContextUsage {
                turn,
                used_tokens: context_metrics.0,
                usable_tokens: context_metrics.1,
                reserved_tokens: context_metrics.2,
                context_window_known: context_metrics.3,
                context_window_reason: context_metrics.4.clone(),
                stable_prefix_hash: context_metrics.5.clone(),
                cache_hit_basis_points,
                estimated_input_tokens: input_estimate.local_tokens,
                provider_input_tokens,
                correction_millionths: reconciliation.correction_millionths,
            },
        );
        usage.add(iteration_usage);
        let iteration_cost = config.model.cost_for_route(
            &config.model_alias,
            selected_route.as_deref(),
            assistant.meta.model.as_deref(),
            normalized_iteration_usage,
        );
        if let Some(qualified) = config.model.qualified_model_for_route(
            &config.model_alias,
            selected_route.as_deref(),
            assistant.meta.model.as_deref(),
        ) {
            assistant.meta.model = Some(qualified);
        }
        turn_cost = Some(combine_cost(turn_cost.take(), iteration_cost.clone()));
        let iteration_usage = cost_units(&iteration_cost);
        current_turn_cost_micros =
            current_turn_cost_micros.saturating_add(iteration_usage.cost_micros_usd);
        current_turn_credit_micros =
            current_turn_credit_micros.saturating_add(iteration_usage.ai_credit_micros);
        current_turn_tokens =
            current_turn_tokens.saturating_add(iteration_usage.subscription_tokens);
        let mut budget_stop = false;
        match evaluate_budget(
            turn,
            config.event_clock.as_ref(),
            &config.event_sink,
            &budget_config,
            local_session_accounting,
            BudgetUsage {
                cost_micros_usd: current_turn_cost_micros,
                ai_credit_micros: current_turn_credit_micros,
                subscription_tokens: current_turn_tokens,
            },
        )
        .await
        {
            Ok(check) => {
                for event in check.events {
                    if persist_event(&signals, event).await.is_err() {
                        status = AgentTurnStatus::Failed;
                        stream_failed = true;
                        break;
                    }
                }
                budget_stop = check.hard_stop;
                if budget_stop {
                    status = AgentTurnStatus::BudgetExceeded;
                }
            }
            Err(error) => {
                send_event(
                    &signals,
                    PendingEvent::Error {
                        message: error.to_string(),
                    },
                );
                status = AgentTurnStatus::Failed;
                stream_failed = true;
            }
        }
        match persist_incomplete_budget_caps(
            &signals,
            turn,
            &budget_config,
            &iteration_cost,
            current_turn_cost_micros,
            current_turn_credit_micros,
            current_turn_tokens,
        )
        .await
        {
            Err(_) => {
                stream_failed = true;
                status = AgentTurnStatus::Failed;
            }
            Ok(true) => {
                budget_stop = true;
                status = AgentTurnStatus::BudgetExceeded;
            }
            Ok(false) => {}
        }
        if budget_stop {
            flush_pending_text_delta(
                &mut pending_text_delta,
                &mut pending_text_delta_deadline,
                &signals,
                turn,
            );
        }
        if provider_overflow_recovered {
            continue 'iterations;
        }
        let admitted_calls = if !stream_failed
            && !budget_stop
            && !calls.is_empty()
            && finish_reason == Some(FinishReason::ToolCalls)
        {
            match super::tool_admission::AdmittedToolBatch::new(
                std::mem::take(&mut calls),
                config.secret_redactor.as_ref(),
            ) {
                Ok(batch) => Some(batch),
                Err(message) => {
                    send_event(&signals, PendingEvent::Error { message });
                    status = AgentTurnStatus::Failed;
                    stream_failed = true;
                    tool_admission_failed = true;
                    None
                }
            }
        } else {
            None
        };
        if tool_admission_failed {
            // The batch never reached tool admission; retain text but no orphan tool calls.
            assistant
                .blocks
                .retain(|block| !matches!(block, Block::ToolCall { .. }));
            calls.clear();
        }
        let assistant_turn = if assistant.blocks.is_empty() {
            None
        } else {
            conversation.push(assistant.clone());
            Some(assistant)
        };
        if stream_failed || status == AgentTurnStatus::Interrupted || budget_stop {
            if let Some(assistant) = &assistant_turn
                && persist_conversation_turn(&signals, turn, assistant)
                    .await
                    .is_err()
            {
                status = AgentTurnStatus::Failed;
            }
            break;
        }
        let Some(reason) = finish_reason else {
            if let Some(assistant) = &assistant_turn
                && persist_conversation_turn(&signals, turn, assistant)
                    .await
                    .is_err()
            {
                status = AgentTurnStatus::Failed;
                break;
            }
            if status != AgentTurnStatus::Interrupted {
                send_event(
                    &signals,
                    PendingEvent::Error {
                        message: "provider stream ended without a finish reason".to_owned(),
                    },
                );
                status = AgentTurnStatus::Failed;
            }
            break;
        };
        if reason != FinishReason::ToolCalls {
            if !calls.is_empty() {
                if let Some(assistant) = &assistant_turn {
                    let _ = persist_conversation_turn(&signals, turn, assistant).await;
                }
                send_event(
                    &signals,
                    PendingEvent::Error {
                        message: "provider emitted tool calls with a non-tool finish reason"
                            .to_owned(),
                    },
                );
                status = AgentTurnStatus::Failed;
                break;
            }
            status = AgentTurnStatus::Completed;
            deferred_terminal_delta = pending_text_delta.take();
            deferred_terminal_turn = assistant_turn;
            break;
        }
        if let Some(assistant) = &assistant_turn
            && persist_conversation_turn(&signals, turn, assistant)
                .await
                .is_err()
        {
            status = AgentTurnStatus::Failed;
            break;
        }
        if admitted_calls.as_ref().is_none_or(|batch| {
            batch.calls.is_empty() || batch.calls.iter().any(|(call, _)| call.arguments.is_none())
        }) {
            send_event(
                &signals,
                PendingEvent::Error {
                    message: "provider reported incomplete tool calls".to_owned(),
                },
            );
            status = AgentTurnStatus::Failed;
            break;
        }
        let Some(calls) = admitted_calls else {
            status = AgentTurnStatus::Failed;
            break;
        };
        let executions = execute_tool_calls(
            turn,
            &tasks,
            calls,
            &config,
            &tool_context,
            &cancellation,
            &approver,
            &signals,
            mode,
        )
        .await;
        if executions.iter().any(|execution| execution.unsettled) {
            status = AgentTurnStatus::Failed;
            break;
        }
        let interrupted = cancellation.is_cancelled();
        let mut tool_blocks = Vec::new();
        let mut doom_triggered = false;
        for execution in executions {
            tool_blocks.push(Block::ToolResult {
                id: ToolCallId(execution.call.id.clone()),
                output: execution.output.clone(),
                is_error: execution.is_error,
            });
            doom_triggered |= !interrupted && doom.observe(&execution.call, &execution);
        }
        let tool_turn = Turn {
            role: Role::Tool,
            blocks: tool_blocks,
            meta: TurnMeta::default(),
        };
        conversation.push(tool_turn.clone());
        if persist_event(
            &signals,
            PendingEvent::ConversationTurnCommitted {
                agent_turn: turn,
                turn: tool_turn,
            },
        )
        .await
        .is_err()
        {
            status = AgentTurnStatus::Failed;
            break;
        }
        if doom_triggered {
            send_event(
                &signals,
                PendingEvent::GuardTriggered {
                    turn,
                    guard: "identical_tool_failure".to_owned(),
                    message: "identical failing tool invocation repeated too many times in recent history"
                        .to_owned(),
                },
            );
            status = AgentTurnStatus::DoomLoop;
            break 'iterations;
        }
        if interrupted {
            status = AgentTurnStatus::Interrupted;
            break;
        }
    }

    if status == AgentTurnStatus::MaxTurns {
        send_event(
            &signals,
            PendingEvent::GuardTriggered {
                turn,
                guard: "max_turns".to_owned(),
                message: format!(
                    "maximum of {} provider iterations reached",
                    config.max_turns
                ),
            },
        );
    }

    let hook = super::completion_hooks::CompletionHooks {
        config: &config,
        cancellation: &cancellation,
        signals: &signals,
        approver: &approver,
        mode,
    }
    .dispatch(turn, status)
    .await;
    match hook {
        Ok(hook) => {
            report_hook_failures(
                HookEvent::TurnEnd,
                hook.failures(),
                &signals,
                config.secret_redactor.as_ref(),
            );
            if !hook.completed() && status == AgentTurnStatus::Completed {
                if let Some(message) =
                    hook_rejection(hook.status(), config.secret_redactor.as_ref())
                {
                    if let Some(assistant) = deferred_terminal_turn.take() {
                        if let Some(text) = deferred_terminal_delta.take() {
                            let _ = persist_event(&signals, PendingEvent::TextDelta { turn, text })
                                .await;
                        }
                        let _ = persist_conversation_turn(&signals, turn, &assistant).await;
                    }
                    let diagnostic = Turn {
                        role: Role::System,
                        blocks: vec![Block::Text { text: message }],
                        meta: TurnMeta {
                            synthetic: true,
                            ..TurnMeta::default()
                        },
                    };
                    if persist_conversation_turn(&signals, turn, &diagnostic)
                        .await
                        .is_ok()
                    {
                        conversation.push(diagnostic);
                    }
                }
                status = AgentTurnStatus::Failed;
            }
        }
        Err(error) if status == AgentTurnStatus::Completed => {
            send_event(
                &signals,
                PendingEvent::Error {
                    message: config.secret_redactor.redact(&error.to_string()),
                },
            );
            status = if cancellation.is_cancelled()
                && !matches!(error, crate::engine::AgentLoopError::EffectsUnsettled(_))
            {
                AgentTurnStatus::Interrupted
            } else {
                AgentTurnStatus::Failed
            };
        }
        Err(_) => {}
    }
    if status != AgentTurnStatus::Completed
        && let Some(assistant) = deferred_terminal_turn.take()
    {
        if let Some(text) = deferred_terminal_delta.take() {
            let _ = persist_event(&signals, PendingEvent::TextDelta { turn, text }).await;
        }
        let _ = persist_conversation_turn(&signals, turn, &assistant).await;
    }
    let cost = turn_cost.unwrap_or_else(unavailable_cost);
    TurnOutcome {
        turn,
        conversation: crate::engine::session::ConversationSummary::from_turns(&conversation),
        status,
        usage,
        cost,
        deferred_terminal_delta,
        deferred_terminal_turn,
        budgeter,
    }
}

pub(in crate::engine) struct RunningTurn {
    pub(in crate::engine) id: u64,
    pub(in crate::engine) cancellation: CancellationToken,
    pub(in crate::engine) caused_by: Option<RequestId>,
}
