//! A rolling summary consumes every canonical page before replacing its generation.
mod fragments;
use super::{
    accounting::{BudgetUsage, SessionAccountingFallback, cost_units},
    compaction::execute_compaction,
    history_context,
    provider_messages::persist_event,
    signals::TurnSignal,
};
use crate::engine::{
    AgentLoopError,
    pending_event::PendingEvent,
    recovery::{ConversationPage, HistoryMaterializationLimits, HistoryRead, SessionHistoryView},
    session::SessionActorConfig,
};
use rw_tools::CancellationToken;
use rw_types::{CompactionReason, Turn, allocation::PrepareAllocation};
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
    suffix: Vec<super::context_commits::RetainedUser>,
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
        reason.clone(),
        instructions,
        accounting,
    )
    .await;
    let result: Result<BudgetUsage, AgentLoopError> = async {
        let mut summary = result?;
        if suffix.is_empty() && reason == CompactionReason::Automatic && !summary.suppress_continue
        {
            summary.carry.push(rw_context::auto_continue_turn());
        }
        let suffix_start = summary.carry.len();
        let suffix_sources = append_suffix(config, &mut summary.carry, suffix).await?;
        check_carry(&summary.carry, &summary.pins)?;
        let mut committed = Vec::with_capacity(summary.carry.len());
        for (ordinal, value) in summary.carry.iter().enumerate() {
            let source = summary
                .pins
                .iter()
                .find(|pin| pin.ordinal == ordinal)
                .map(|pin| &pin.source)
                .or_else(|| {
                    ordinal
                        .checked_sub(suffix_start)
                        .and_then(|index| suffix_sources.get(index))
                });
            committed.push(super::context_commits::commit(signals, turn, value, source).await?);
        }
        for pin in summary.pins {
            let sequence = committed
                .get(pin.ordinal)
                .ok_or_else(|| invalid("compaction pin position"))?;
            persist_event(
                signals,
                PendingEvent::ContextItemPinned {
                    item_id: rw_types::context_source::conversation_item(*sequence),
                    effective_after_agent_turn: pin.order,
                },
            )
            .await?;
        }
        let now = config.event_clock.unix_time_millis();
        config
            .event_sink
            .budget_totals(crate::engine::event_clock::BudgetLedgerQuery {
                now_unix_ms: now,
                utc_day_start_unix_ms: now.saturating_sub(now % 86_400_000),
                trailing_minute_start_unix_ms: now.saturating_sub(60_000),
            })
            .await?;
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

async fn append_suffix(
    config: &SessionActorConfig,
    carry: &mut Vec<Turn>,
    suffix: Vec<super::context_commits::RetainedUser>,
) -> Result<Vec<crate::engine::recovery::ConversationSource>, AgentLoopError> {
    let selected = config.history.capture_history().await?;
    let mut sources = Vec::with_capacity(suffix.len());
    for value in suffix {
        let source = selected.source_turn(value.source).await?;
        let (_, source) = source
            .as_ref()
            .ok_or_else(|| invalid("compaction suffix source is not effective"))?;
        sources.push(source.clone());
        carry.push(value.turn);
    }
    Ok(sources)
}

#[derive(Default)]
struct Summary {
    carry: Vec<Turn>,
    pins: Vec<CarryPin>,
    suppress_continue: bool,
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
    let mut fragment = None;
    let end = history.conversation().turns;
    while next < end {
        if cancellation.is_cancelled() {
            return Err(invalid("history compaction cancelled"));
        }
        let tokens = page_tokens(config, &summary.carry, instructions.as_deref())?;
        let source = summary
            .append_next(history, &mut next, &mut fragment, end, tokens)
            .await?;
        check_carry(&summary.carry, &summary.pins)?;
        if summary.carry.is_empty() {
            drop(source);
            continue;
        }
        let execution = execute_compaction(
            &summary.carry,
            summary
                .pins
                .iter()
                .map(|pin| rw_context::ConversationPin {
                    item_id: format!("carry:{}", pin.ordinal),
                    order: pin.order,
                    turn: summary.carry[pin.ordinal].clone(),
                })
                .collect(),
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
        summary.accept(execution, config, signals, turn).await?;
        drop(source);
    }
    Ok(summary)
}

fn check_carry(turns: &Vec<Turn>, pins: &[CarryPin]) -> Result<(), AgentLoopError> {
    if pins.len() > MAX_PINS
        || turns
            .prepared_bytes()
            .and_then(|bytes| {
                bytes.checked_add(pins.len().checked_mul(std::mem::size_of::<CarryPin>())?)
            })
            .is_none_or(|bytes| bytes > CARRY_BYTES)
    {
        return Err(invalid(
            "retained summary and pins exceed compaction working admission",
        ));
    }
    Ok(())
}

fn page_tokens(
    config: &SessionActorConfig,
    carry: &[Turn],
    instructions: Option<&str>,
) -> Result<u64, AgentLoopError> {
    let compaction = config.model.compaction_config();
    let alias = compaction
        .model_alias
        .as_deref()
        .unwrap_or(&config.model_alias);
    let metadata = config.model.context_metadata(alias);
    let main = config.model.context_metadata(&config.model_alias);
    // Both compaction route and its configured fallback must admit the request.
    let window = match (metadata.max_context_tokens, main.max_context_tokens) {
        (Some(left), Some(right)) => left.min(right),
        (Some(value), None) | (None, Some(value)) => value,
        (None, None) => 32_768,
    };
    let carry = carry.iter().fold(0u64, |total, turn| {
        total.saturating_add(rw_context::LocalTokenEstimator::turn(turn))
    });
    let prompt = rw_context::LocalTokenEstimator::text(rw_context::DEFAULT_COMPACTION_PROMPT)
        .saturating_add(instructions.map_or(0, rw_context::LocalTokenEstimator::text));
    window
        .checked_sub(
            carry
                .saturating_add(prompt)
                .saturating_add(u64::from(
                    super::compaction::summary_output_tokens(config, alias).max(
                        super::compaction::summary_output_tokens(config, &config.model_alias),
                    ),
                ))
                .saturating_add(32),
        )
        .filter(|tokens| *tokens > 0)
        .ok_or_else(|| invalid("retained summary and pins exhaust compaction context capacity"))
}

impl Summary {
    async fn append_next(
        &mut self,
        history: &Arc<dyn SessionHistoryView>,
        next: &mut u64,
        fragment: &mut Option<fragments::PendingFragments>,
        end: u64,
        tokens: u64,
    ) -> Result<HistoryRead<()>, AgentLoopError> {
        let sources = history.conversation_sources(*next..*next + 1).await?;
        let first = sources
            .first()
            .ok_or_else(|| invalid("history source cursor"))?;
        if fragment.is_some()
            || first.serialized_bytes > PAGE_BYTES
            || first.decoded_bytes > PAGE_HEAP
            || first.estimated_tokens > tokens
        {
            return fragments::append(
                fragment,
                history,
                next,
                first.sequence,
                tokens,
                &mut self.carry,
            )
            .await;
        }
        let page = history
            .conversation_page(
                *next..end,
                HistoryMaterializationLimits {
                    max_turns: 128,
                    max_serialized_bytes: PAGE_BYTES,
                    max_decoded_bytes: PAGE_HEAP,
                    max_estimated_tokens: tokens,
                },
            )
            .await?;
        if page.range.start != *next || page.range.end <= *next {
            return Err(invalid("history compaction cursor did not advance"));
        }
        *next = page.range.end;
        let (page, owner) = page.into_parts();
        for ((mut value, source), action) in page
            .turns
            .into_iter()
            .zip(page.sources)
            .zip(page.context_actions)
        {
            if action.as_ref().is_some_and(|action| !action.pinned) {
                continue;
            }
            apply_pruning(&mut value, source.sequence, &page.pruned_tool_outputs);
            if let Some(action) = action {
                self.pins.push(CarryPin {
                    ordinal: self.carry.len(),
                    order: action.effective_after_agent_turn,
                    source: source.clone(),
                });
            }
            self.carry.push(value);
        }
        Ok(owner)
    }

    async fn accept(
        &mut self,
        execution: super::compaction::CompactionExecution,
        config: &SessionActorConfig,
        signals: &mpsc::UnboundedSender<TurnSignal>,
        turn: u64,
    ) -> Result<(), AgentLoopError> {
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
        self.cost_micros = self
            .cost_micros
            .saturating_add(units.cost_micros_usd)
            .saturating_add(execution.failed_attempt_cost_micros);
        self.credit_micros = self
            .credit_micros
            .saturating_add(units.ai_credit_micros)
            .saturating_add(execution.failed_attempt_credit_micros);
        self.tokens = self
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
            self.cost_micros = 0;
            self.credit_micros = 0;
            self.tokens = 0;
        }
        if execution.hard_stop {
            return Err(invalid("budget cap prevents complete history compaction"));
        }
        self.suppress_continue |= !execution.auto_continue;
        self.carry = execution.conversation;
        self.pins = execution
            .remapped_pins
            .into_iter()
            .map(|(ordinal, item)| {
                let previous = item
                    .0
                    .strip_prefix("carry:")
                    .and_then(|value| value.parse::<usize>().ok())
                    .and_then(|position| self.pins.iter().find(|pin| pin.ordinal == position))
                    .ok_or_else(|| invalid("rolling compaction pin source"))?;
                Ok(CarryPin {
                    ordinal,
                    order: turn,
                    source: previous.source.clone(),
                })
            })
            .collect::<Result<Vec<_>, AgentLoopError>>()?;
        check_carry(&self.carry, &self.pins)?;
        Ok(())
    }
}

struct CarryPin {
    ordinal: usize,
    order: u64,
    source: crate::engine::recovery::ConversationSource,
}

pub(super) fn apply_pruning(
    value: &mut Turn,
    sequence: rw_types::SequenceId,
    pruned: &std::collections::BTreeMap<String, u64>,
) {
    for (index, block) in value.blocks.iter_mut().enumerate() {
        if let rw_types::Block::ToolResult { output, .. } = block
            && pruned.contains_key(&super::context::block_key(sequence, index))
        {
            *output = rw_types::ToolOutput::Text {
                text: rw_context::PRUNED_TOOL_OUTPUT_REPLACEMENT.into(),
            };
        }
    }
}

pub(super) fn requires_streaming(
    view: &Arc<dyn SessionHistoryView>,
    config: &SessionActorConfig,
    instructions: Option<&str>,
) -> Result<bool, AgentLoopError> {
    let cut = view.conversation();
    Ok(cut.serialized_bytes > PAGE_BYTES
        || cut.decoded_bytes > PAGE_HEAP
        || cut.estimated_tokens > page_tokens(config, &[], instructions)?)
}
