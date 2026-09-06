//! A rolling summary consumes every canonical page before replacing its generation.
use super::{
    accounting::{BudgetUsage, SessionAccountingFallback, cost_units},
    compaction::execute_compaction,
    history_context,
    provider_messages::{persist_conversation_turn, persist_event},
    signals::TurnSignal,
};
use crate::engine::{
    AgentLoopError,
    pending_event::PendingEvent,
    projection::ContextSurgeryAction,
    recovery::{ConversationPage, HistoryMaterializationLimits, HistoryRead, SessionHistoryView},
    session::SessionActorConfig,
};
use rw_tools::CancellationToken;
use rw_types::{CompactionReason, ContextItemId, Turn, allocation::PrepareAllocation};
use std::sync::Arc;
use tokio::sync::mpsc;

const PAGE_BYTES: u64 = 1024 * 1024;
const PAGE_HEAP: u64 = 8 * 1024 * 1024;
const CARRY_BYTES: usize = 8 * 1024 * 1024;
const MAX_PINS: usize = 128;

fn invalid(message: &str) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(message.into())
}

/// Accepted input is retained after the summary, never folded into a partial cut.
/// One failed chunk leaves the original conversation generation visible.
#[allow(clippy::too_many_arguments)]
pub(in crate::engine) async fn compact(
    history: Arc<dyn SessionHistoryView>,
    suffix: Vec<Turn>,
    config: &SessionActorConfig,
    cancellation: &CancellationToken,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    reason: CompactionReason,
    instructions: Option<String>,
    accounting: SessionAccountingFallback,
) -> Result<(HistoryRead<ConversationPage>, BudgetUsage), AgentLoopError> {
    let working = history.reserve_working_set()?;
    persist_event(
        signals,
        PendingEvent::CompactionStarted {
            reason: reason.clone(),
        },
    )
    .await?;
    let result = summarize(
        &history,
        config,
        cancellation,
        signals,
        turn,
        reason,
        instructions,
        accounting,
    )
    .await;
    let result: Result<BudgetUsage, AgentLoopError> = async {
        let mut summary = result?;
        summary.carry.extend(suffix);
        check_carry(&summary.carry, &summary.pins)?;
        for value in &summary.carry {
            persist_conversation_turn(signals, turn, value).await?;
        }
        for action in summary.pins {
            persist_event(
                signals,
                PendingEvent::ContextItemPinned {
                    item_id: action.item_id,
                    effective_after_agent_turn: action.effective_after_agent_turn,
                },
            )
            .await?;
        }
        let new_tokens = summary.carry.iter().fold(0u64, |total, value| {
            total.saturating_add(rw_context::LocalTokenEstimator::turn(value))
        });
        persist_event(
            signals,
            PendingEvent::CompactionFinished {
                summary_turn: turn,
                reclaimed_tokens: history
                    .conversation()
                    .estimated_tokens
                    .saturating_sub(new_tokens),
                usage: None,
                cost: None,
            },
        )
        .await?;
        Ok(BudgetUsage {
            cost_micros_usd: summary.cost_micros,
            ai_credit_micros: summary.credit_micros,
            subscription_tokens: summary.tokens,
        })
    }
    .await;
    if result.is_err() {
        persist_event(
            signals,
            PendingEvent::CompactionFailed { summary_turn: turn },
        )
        .await?;
    }
    let cost = result?;
    drop(working);
    let captured = config.history.capture_history().await?;
    Ok((history_context::read_view(&captured).await?, cost))
}

#[derive(Default)]
struct Summary {
    carry: Vec<Turn>,
    pins: Vec<ContextSurgeryAction>,
    cost_micros: u64,
    credit_micros: u64,
    tokens: u64,
}

#[allow(clippy::too_many_arguments)]
async fn summarize(
    history: &Arc<dyn SessionHistoryView>,
    config: &SessionActorConfig,
    cancellation: &CancellationToken,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    reason: CompactionReason,
    instructions: Option<String>,
    accounting: SessionAccountingFallback,
) -> Result<Summary, AgentLoopError> {
    let mut summary = Summary::default();
    let mut next = 0;
    let end = history.conversation().turns;
    while next < end {
        if cancellation.is_cancelled() {
            return Err(invalid("history compaction cancelled"));
        }
        let page = history
            .conversation_page(
                next..end,
                HistoryMaterializationLimits {
                    max_turns: 128,
                    max_serialized_bytes: PAGE_BYTES,
                    max_decoded_bytes: PAGE_HEAP,
                },
            )
            .await?;
        if page.range.start != next || page.range.end <= next {
            return Err(invalid("history compaction cursor did not advance"));
        }
        next = page.range.end;
        let (page, source) = page.into_parts();
        let base = summary.carry.len();
        for (offset, action) in page.context_actions.into_iter().enumerate() {
            if let Some(mut action) = action.filter(|action| action.pinned) {
                action.item_id = ContextItemId(format!("conversation:{}", base + offset));
                summary.pins.push(action);
            }
        }
        summary.carry.extend(page.turns);
        check_carry(&summary.carry, &summary.pins)?;
        let execution = execute_compaction(
            &summary.carry,
            &summary.pins,
            reason.clone(),
            instructions.clone(),
            config,
            accounting,
            cancellation,
            signals,
            turn,
            summary.cost_micros,
            summary.credit_micros,
            summary.tokens,
            true,
            true,
        )
        .await?;
        persist_event(
            signals,
            PendingEvent::CompactionAttemptFinished {
                summary_turn: turn,
                usage: execution.usage,
                cost: execution.cost.clone(),
            },
        )
        .await?;
        let units = cost_units(&execution.cost);
        summary.cost_micros = summary
            .cost_micros
            .saturating_add(units.cost_micros_usd)
            .saturating_add(execution.failed_attempt_cost_micros);
        summary.credit_micros = summary
            .credit_micros
            .saturating_add(units.ai_credit_micros)
            .saturating_add(execution.failed_attempt_credit_micros);
        summary.tokens = summary
            .tokens
            .saturating_add(units.subscription_tokens)
            .saturating_add(execution.failed_attempt_tokens);
        let now = config.event_clock.unix_time_millis();
        let ledger = config
            .event_sink
            .budget_totals(crate::engine::event_clock::BudgetLedgerQuery {
                now_unix_ms: now,
                utc_day_start_unix_ms: now.saturating_sub(now % 86_400_000),
                trailing_minute_start_unix_ms: now.saturating_sub(60_000),
            })
            .await?;
        if ledger.authoritative {
            summary.cost_micros = 0;
            summary.credit_micros = 0;
            summary.tokens = 0;
        }
        if execution.hard_stop {
            return Err(invalid("budget cap prevents complete history compaction"));
        }
        summary.carry = execution.conversation;
        summary.pins = execution
            .remapped_pins
            .into_iter()
            .map(|item_id| ContextSurgeryAction {
                item_id,
                pinned: true,
                effective_after_agent_turn: turn,
            })
            .collect();
        check_carry(&summary.carry, &summary.pins)?;
        drop(source);
    }
    Ok(summary)
}

fn check_carry(turns: &Vec<Turn>, pins: &[ContextSurgeryAction]) -> Result<(), AgentLoopError> {
    if pins.len() > MAX_PINS
        || turns
            .prepared_bytes()
            .is_none_or(|bytes| bytes > CARRY_BYTES)
    {
        return Err(invalid(
            "retained summary and pins exceed compaction working admission",
        ));
    }
    Ok(())
}
