use crate::engine::AgentLoopError;
use crate::engine::SessionUsage;
use crate::engine::event_clock::BudgetLedgerQuery;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::ContextSurgeryAction;
use crate::engine::redaction::StreamingSecretRedactor;
use crate::engine::session::SessionActorConfig;
use crate::engine::turn::accounting::BudgetUsage;
use crate::engine::turn::accounting::SessionAccountingFallback;
use crate::engine::turn::accounting::cost_units;
use crate::engine::turn::accounting::evaluate_budget;
use crate::engine::turn::accounting::persist_incomplete_budget_caps;
use crate::engine::turn::hooks::dispatch_hook;
use crate::engine::turn::hooks::mark_unsettled;
use crate::engine::turn::hooks::report_hook_failures;
use crate::engine::turn::provider_calls;
use crate::engine::turn::provider_messages::persist_conversation_turn;
use crate::engine::turn::provider_messages::persist_event;
use crate::engine::turn::provider_messages::send_compaction_progress;
use crate::engine::turn::signals::CompactionProgressKind;
use crate::engine::turn::signals::TurnSignal;
use futures_util::StreamExt;
use rw_context::CompactionInput;
use rw_context::CompactionReason as ContextCompactionReason;
use rw_context::Compactor;
use rw_context::ConversationPin;
use rw_context::LocalTokenEstimator;
use rw_context::PreCompactHook;
use rw_ext::HookEvent;
use rw_providers::ProviderEvent;
use rw_providers::ProviderRequest;
use rw_providers::ToolChoice;
use rw_tools::CancellationToken;
use rw_types::AccountingAttribution;
use rw_types::Block;
use rw_types::CompactionReason;
use rw_types::ContextItemId;
use rw_types::Cost;
use rw_types::Role;
use rw_types::Turn;
use rw_types::TurnMeta;
use rw_types::hook_contract::HookCompactionInput;
use rw_types::hook_contract::HookInput;
use std::collections::BTreeMap;
use tokio::sync::mpsc;

pub(super) struct CompactionExecution {
    pub(super) conversation: Vec<Turn>,
    pub(super) usage: SessionUsage,
    pub(super) cost: Cost,
    pub(super) reclaimed_tokens: u64,
    pub(super) remapped_pins: Vec<ContextItemId>,
    pub(super) hard_stop: bool,
    pub(super) failed_attempt_cost_micros: u64,
    pub(super) failed_attempt_credit_micros: u64,
    pub(super) failed_attempt_tokens: u64,
}

pub(super) fn context_compaction_reason(reason: &CompactionReason) -> ContextCompactionReason {
    match reason {
        CompactionReason::Automatic => ContextCompactionReason::AutomaticOverflow,
        CompactionReason::Manual => ContextCompactionReason::Manual,
        CompactionReason::ProviderOverflow => ContextCompactionReason::ProviderOverflow,
    }
}

pub(super) async fn persist_failed_compaction_attempt(
    config: &SessionActorConfig,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    alias: &str,
    selected_route: Option<&str>,
    reported_model: Option<&str>,
    usage: SessionUsage,
) -> Result<Option<(Cost, bool)>, AgentLoopError> {
    if usage == SessionUsage::default() {
        return Ok(None);
    }
    let cost = config
        .model
        .cost_for_route(alias, selected_route, reported_model, usage.into());
    persist_event(
        signals,
        PendingEvent::CompactionAttemptFinished {
            summary_turn: turn,
            usage,
            cost: cost.clone(),
        },
    )
    .await?;
    let now = config.event_clock.unix_time_millis();
    let ledger = config
        .event_sink
        .budget_totals(BudgetLedgerQuery {
            now_unix_ms: now,
            utc_day_start_unix_ms: now.saturating_sub(now % 86_400_000),
            trailing_minute_start_unix_ms: now.saturating_sub(60_000),
        })
        .await?;
    Ok(Some((cost, ledger.authoritative)))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn execute_compaction(
    conversation: &[Turn],
    surgery: &[ContextSurgeryAction],
    reason: CompactionReason,
    instructions: Option<String>,
    config: &SessionActorConfig,
    local_session_accounting: SessionAccountingFallback,
    cancellation: &CancellationToken,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    current_turn_cost_micros: u64,
    current_turn_credit_micros: u64,
    current_turn_tokens: u64,
    enforce_budget_via_signals: bool,
    streaming: bool,
) -> Result<CompactionExecution, AgentLoopError> {
    let hook_result = dispatch_hook(
        &config.hooks,
        HookInput::PreCompact(HookCompactionInput {
            reason: reason.clone(),
            conversation_turns: u32::try_from(conversation.len()).map_err(|_| {
                AgentLoopError::Extension("conversation exceeds hook turn-count limit".to_owned())
            })?,
            injected_context: Vec::new(),
            replacement_prompt: None,
            suppress_auto_continue: false,
        }),
        cancellation,
        signals,
    )
    .await?;
    report_hook_failures(
        HookEvent::PreCompact,
        hook_result.failures(),
        signals,
        config.secret_redactor.as_ref(),
    );
    if !hook_result.completed() {
        return Err(AgentLoopError::Extension(
            "pre_compact hook blocked compaction".to_owned(),
        ));
    }
    let HookInput::PreCompact(input) = hook_result.input() else {
        unreachable!("dispatcher preserves hook phase")
    };
    let hook = PreCompactHook {
        injected_context: input
            .injected_context
            .iter()
            .map(|value| config.secret_redactor.redact(value))
            .collect(),
        replacement_prompt: input
            .replacement_prompt
            .as_ref()
            .map(|value| config.secret_redactor.redact(value)),
    };
    let automatic_continue = !streaming && !input.suppress_auto_continue;
    let mut latest = BTreeMap::<String, &ContextSurgeryAction>::new();
    for action in surgery {
        latest.insert(action.item_id.0.clone(), action);
    }
    let pins = latest
        .values()
        .filter(|action| action.pinned)
        .filter_map(|action| {
            let index = action
                .item_id
                .0
                .strip_prefix("conversation:")?
                .parse::<usize>()
                .ok()?;
            let pinned_turn = conversation.get(index)?.clone();
            Some(ConversationPin {
                item_id: action.item_id.0.clone(),
                order: action.effective_after_agent_turn,
                turn: pinned_turn,
            })
        })
        .collect();
    let compaction_config = config.model.compaction_config();
    let plan = Compactor::plan(CompactionInput {
        conversation: conversation.to_vec(),
        pins,
        reason: context_compaction_reason(&reason),
        instructions,
        hook,
        session_model_alias: config.model_alias.clone(),
        compaction_model_alias: compaction_config.model_alias,
        automatic_continue,
    })
    .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
    let mut summary_request_turns = plan.history.clone();
    summary_request_turns.push(Turn {
        role: Role::User,
        blocks: vec![Block::Text {
            text: plan.summary_prompt.clone(),
        }],
        meta: TurnMeta {
            synthetic: true,
            ..TurnMeta::default()
        },
    });
    let aliases = if plan.model_alias == config.model_alias {
        vec![plan.model_alias.clone()]
    } else {
        vec![plan.model_alias.clone(), config.model_alias.clone()]
    };
    let mut last_error = None;
    let mut completed = None;
    let mut failed_attempt_cost_micros = 0_u64;
    let mut failed_attempt_credit_micros = 0_u64;
    let mut failed_attempt_tokens = 0_u64;
    for (attempt_index, alias) in aliases.into_iter().enumerate() {
        let attempt = u32::try_from(attempt_index).unwrap_or(u32::MAX);
        send_compaction_progress(
            signals,
            turn,
            attempt,
            CompactionProgressKind::AttemptStarted,
        );
        if let Err(error) = config.model.prepare_model(&alias).await {
            last_error = Some(error);
            continue;
        }
        if enforce_budget_via_signals {
            let budget = evaluate_budget(
                turn,
                config.event_clock.as_ref(),
                &config.event_sink,
                &config.model.budget_config(),
                local_session_accounting,
                BudgetUsage {
                    cost_micros_usd: current_turn_cost_micros
                        .saturating_add(failed_attempt_cost_micros),
                    ai_credit_micros: current_turn_credit_micros
                        .saturating_add(failed_attempt_credit_micros),
                    subscription_tokens: current_turn_tokens.saturating_add(failed_attempt_tokens),
                },
            )
            .await?;
            for event in budget.events {
                persist_event(signals, event).await?;
            }
            if budget.hard_stop {
                return Err(AgentLoopError::InvalidConfiguration(
                    "budget hard cap prevents compaction model call".to_owned(),
                ));
            }
        }
        let request = ProviderRequest {
            model: alias.clone(),
            turns: summary_request_turns.clone(),
            tools: Vec::new(),
            tool_choice: ToolChoice::None {},
            max_output_tokens: config.max_output_tokens,
            temperature: None,
            thinking: config.thinking,
            cache_hint: None,
        };
        let provider = (alias == config.model_alias)
            .then_some(config.recovered.provider.as_deref())
            .flatten();
        let invocation = provider_calls::invocation(
            config,
            signals,
            turn,
            AccountingAttribution::Compaction,
            &request,
        )?;
        let mut stream = match config
            .model
            .stream_for_provider(&alias, provider, request, invocation)
        {
            Ok(stream) => stream,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        let mut summary = String::new();
        let mut text_redactor = StreamingSecretRedactor::new(config.secret_redactor.as_ref());
        let mut thinking_redactor = StreamingSecretRedactor::new(config.secret_redactor.as_ref());
        let mut usage = SessionUsage::default();
        let mut reported_model = None;
        let mut selected_route = None;
        let mut failed = None;
        let mut cancelled = false;
        loop {
            let event = tokio::select! {
                () = cancellation.cancelled() => {
                    cancelled = true;
                    failed = Some(AgentLoopError::Provider("compaction cancelled".to_owned()));
                    break;
                }
                event = stream.next() => event,
            };
            let Some(event) = event else {
                break;
            };
            match event {
                Ok(ProviderEvent::RouteSelected { route }) => selected_route = Some(route),
                Ok(ProviderEvent::MessageStart { model }) => reported_model = Some(model),
                Ok(ProviderEvent::TextDelta { text }) => {
                    let text = text_redactor.push(&text);
                    if let Err(error) = append_summary(&mut summary, &text) {
                        failed = Some(error);
                        break;
                    }
                    if !text.is_empty() {
                        send_compaction_progress(
                            signals,
                            turn,
                            attempt,
                            CompactionProgressKind::Text(text),
                        );
                    }
                }
                Ok(ProviderEvent::ThinkingDelta { content, .. }) => {
                    let text = thinking_redactor.push(&content);
                    if !text.is_empty() {
                        send_compaction_progress(
                            signals,
                            turn,
                            attempt,
                            CompactionProgressKind::Thinking(text),
                        );
                    }
                }
                Ok(ProviderEvent::Usage { usage: latest }) => usage.update(latest),
                Ok(
                    ProviderEvent::ToolCallStart { .. }
                    | ProviderEvent::ToolCallArgumentsDelta { .. }
                    | ProviderEvent::ToolCallEnd { .. },
                ) => {
                    failed = Some(AgentLoopError::Provider(
                        "compaction model attempted a tool call".to_owned(),
                    ));
                    break;
                }
                Ok(ProviderEvent::Citation { .. } | ProviderEvent::Finished { .. }) => {}
                Err(error) => {
                    failed = Some(AgentLoopError::Provider(error.to_string()));
                    break;
                }
            }
        }
        drop(stream);
        if let Err(error) = config.model.settle_effects().await {
            mark_unsettled(signals, cancellation, error.to_string());
            return Err(error);
        }
        if failed.is_none() {
            let text_tail = text_redactor.finish();
            if let Err(error) = append_summary(&mut summary, &text_tail) {
                failed = Some(error);
            } else if !text_tail.is_empty() {
                send_compaction_progress(
                    signals,
                    turn,
                    attempt,
                    CompactionProgressKind::Text(text_tail),
                );
            }
        }
        if let Some(error) = failed {
            if let Some((cost, false)) = persist_failed_compaction_attempt(
                config,
                signals,
                turn,
                &alias,
                selected_route.as_deref(),
                reported_model.as_deref(),
                usage,
            )
            .await?
            {
                if persist_incomplete_budget_caps(
                    signals,
                    turn,
                    &config.model.budget_config(),
                    &cost,
                    current_turn_cost_micros,
                    current_turn_credit_micros,
                    current_turn_tokens,
                )
                .await?
                {
                    return Err(AgentLoopError::InvalidConfiguration(
                        "budget cap cannot price a failed compaction attempt".to_owned(),
                    ));
                }
                let units = cost_units(&cost);
                failed_attempt_cost_micros =
                    failed_attempt_cost_micros.saturating_add(units.cost_micros_usd);
                failed_attempt_credit_micros =
                    failed_attempt_credit_micros.saturating_add(units.ai_credit_micros);
                failed_attempt_tokens =
                    failed_attempt_tokens.saturating_add(units.subscription_tokens);
            }
            if cancelled {
                return Err(error);
            }
            last_error = Some(error);
            continue;
        }
        let thinking_tail = thinking_redactor.finish();
        if !thinking_tail.is_empty() {
            send_compaction_progress(
                signals,
                turn,
                attempt,
                CompactionProgressKind::Thinking(thinking_tail),
            );
        }
        if summary.trim().is_empty() {
            if let Some((cost, false)) = persist_failed_compaction_attempt(
                config,
                signals,
                turn,
                &alias,
                selected_route.as_deref(),
                reported_model.as_deref(),
                usage,
            )
            .await?
            {
                if persist_incomplete_budget_caps(
                    signals,
                    turn,
                    &config.model.budget_config(),
                    &cost,
                    current_turn_cost_micros,
                    current_turn_credit_micros,
                    current_turn_tokens,
                )
                .await?
                {
                    return Err(AgentLoopError::InvalidConfiguration(
                        "budget cap cannot price a failed compaction attempt".to_owned(),
                    ));
                }
                let units = cost_units(&cost);
                failed_attempt_cost_micros =
                    failed_attempt_cost_micros.saturating_add(units.cost_micros_usd);
                failed_attempt_credit_micros =
                    failed_attempt_credit_micros.saturating_add(units.ai_credit_micros);
                failed_attempt_tokens =
                    failed_attempt_tokens.saturating_add(units.subscription_tokens);
            }
            last_error = Some(AgentLoopError::Provider(
                "compaction model returned an empty summary".to_owned(),
            ));
            continue;
        }
        let cost = config.model.cost_for_route(
            &alias,
            selected_route.as_deref(),
            reported_model.as_deref(),
            usage.into(),
        );
        let compaction_usage = cost_units(&cost);
        let hard_stop = if enforce_budget_via_signals {
            let post_budget = evaluate_budget(
                turn,
                config.event_clock.as_ref(),
                &config.event_sink,
                &config.model.budget_config(),
                local_session_accounting,
                BudgetUsage {
                    cost_micros_usd: current_turn_cost_micros
                        .saturating_add(failed_attempt_cost_micros)
                        .saturating_add(compaction_usage.cost_micros_usd),
                    ai_credit_micros: current_turn_credit_micros
                        .saturating_add(failed_attempt_credit_micros)
                        .saturating_add(compaction_usage.ai_credit_micros),
                    subscription_tokens: current_turn_tokens
                        .saturating_add(failed_attempt_tokens)
                        .saturating_add(compaction_usage.subscription_tokens),
                },
            )
            .await?;
            for event in post_budget.events {
                persist_event(signals, event).await?;
            }
            let incomplete = persist_incomplete_budget_caps(
                signals,
                turn,
                &config.model.budget_config(),
                &cost,
                current_turn_cost_micros.saturating_add(failed_attempt_cost_micros),
                current_turn_credit_micros.saturating_add(failed_attempt_credit_micros),
                current_turn_tokens.saturating_add(failed_attempt_tokens),
            )
            .await?;
            post_budget.hard_stop || incomplete
        } else {
            false
        };
        completed = Some((summary, usage, cost, hard_stop));
        break;
    }
    let Some((summary, usage, cost, hard_stop)) = completed else {
        return Err(last_error.unwrap_or_else(|| {
            AgentLoopError::Provider("compaction model was unavailable".to_owned())
        }));
    };
    let old_tokens = conversation.iter().fold(0_u64, |total, turn| {
        total.saturating_add(LocalTokenEstimator::turn(turn))
    });
    let compacted = plan.post_summary_turns(summary);
    let new_tokens = compacted.iter().fold(0_u64, |total, turn| {
        total.saturating_add(LocalTokenEstimator::turn(turn))
    });
    let remapped_pins = (0..plan.ordered_pins.len())
        .map(|index| ContextItemId(format!("conversation:{}", index.saturating_add(1))))
        .collect();
    Ok(CompactionExecution {
        conversation: compacted,
        usage,
        cost,
        reclaimed_tokens: old_tokens.saturating_sub(new_tokens),
        remapped_pins,
        hard_stop,
        failed_attempt_cost_micros,
        failed_attempt_credit_micros,
        failed_attempt_tokens,
    })
}

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    target = "rw_performance",
    level = "trace",
    name = "context.compact",
    skip_all
)]
pub(in crate::engine) async fn compact_during_turn(
    turn: u64,
    conversation: &mut Vec<Turn>,
    surgery: &mut Vec<ContextSurgeryAction>,
    reason: CompactionReason,
    config: &SessionActorConfig,
    cancellation: &CancellationToken,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    local_session_accounting: SessionAccountingFallback,
    current_turn_cost_micros: u64,
    current_turn_credit_micros: u64,
    current_turn_tokens: u64,
    instructions: Option<String>,
) -> Result<(u64, u64, u64, bool), AgentLoopError> {
    persist_event(
        signals,
        PendingEvent::CompactionStarted {
            reason: reason.clone(),
        },
    )
    .await?;
    let transaction = async {
        let execution = execute_compaction(
            conversation,
            surgery,
            reason,
            instructions,
            config,
            local_session_accounting,
            cancellation,
            signals,
            turn,
            current_turn_cost_micros,
            current_turn_credit_micros,
            current_turn_tokens,
            true,
            false,
        )
        .await?;
        for compacted_turn in &execution.conversation {
            persist_conversation_turn(signals, turn, compacted_turn).await?;
        }
        surgery.clear();
        for item_id in &execution.remapped_pins {
            persist_event(
                signals,
                PendingEvent::ContextItemPinned {
                    item_id: item_id.clone(),
                    effective_after_agent_turn: turn,
                },
            )
            .await?;
            surgery.push(ContextSurgeryAction {
                item_id: item_id.clone(),
                pinned: true,
                effective_after_agent_turn: turn,
            });
        }
        let successful = cost_units(&execution.cost);
        let cost_micros = successful
            .cost_micros_usd
            .saturating_add(execution.failed_attempt_cost_micros);
        let credit_micros = successful
            .ai_credit_micros
            .saturating_add(execution.failed_attempt_credit_micros);
        let tokens = successful
            .subscription_tokens
            .saturating_add(execution.failed_attempt_tokens);
        let now = config.event_clock.unix_time_millis();
        let ledger = config
            .event_sink
            .budget_totals(BudgetLedgerQuery {
                now_unix_ms: now,
                utc_day_start_unix_ms: now.saturating_sub(now % 86_400_000),
                trailing_minute_start_unix_ms: now.saturating_sub(60_000),
            })
            .await?;
        persist_event(
            signals,
            PendingEvent::CompactionFinished {
                summary_turn: turn,
                reclaimed_tokens: execution.reclaimed_tokens,
                usage: Some(execution.usage),
                cost: Some(execution.cost),
            },
        )
        .await?;
        *conversation = execution.conversation;
        Ok((
            if ledger.authoritative { 0 } else { cost_micros },
            if ledger.authoritative {
                0
            } else {
                credit_micros
            },
            if ledger.authoritative { 0 } else { tokens },
            execution.hard_stop,
        ))
    }
    .await;
    match transaction {
        Ok(result) => Ok(result),
        Err(error) => {
            persist_event(
                signals,
                PendingEvent::CompactionFailed { summary_turn: turn },
            )
            .await?;
            Err(error)
        }
    }
}

fn append_summary(summary: &mut String, text: &str) -> Result<(), AgentLoopError> {
    const MAX_SUMMARY_BYTES: usize = 256 * 1024;
    if text.len() > MAX_SUMMARY_BYTES.saturating_sub(summary.len()) {
        return Err(AgentLoopError::Provider(
            "compaction summary exceeds admitted bytes".into(),
        ));
    }
    summary.push_str(text);
    Ok(())
}
