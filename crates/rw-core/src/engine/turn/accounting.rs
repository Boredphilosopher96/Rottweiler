use crate::engine::AgentLoopError;
use crate::engine::durability::SessionEventSink;
use crate::engine::event_clock::BudgetLedgerQuery;
use crate::engine::event_clock::EventClock;
use crate::engine::event_clock::format_unix_rfc3339;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::ActorState;
use crate::engine::session::SessionActorConfig;
use crate::engine::turn::provider_messages::persist_event;
use crate::engine::turn::signals::TurnSignal;
use rw_types::BudgetLevel;
use rw_types::BudgetScope;
use rw_types::BudgetUnit;
use rw_types::Cost;
use rw_types::CostSnapshot;
use rw_types::SubscriptionTokenAccounting;
use rw_types::TurnAccounting;
use rw_types::Usage;
use rw_types::config::BudgetConfig;
use std::sync::Arc;
use tokio::sync::mpsc;

pub(super) fn add_usage(total: &mut Usage, usage: &Usage) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(usage.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(usage.cache_write_tokens);
    total.reasoning_tokens = total
        .reasoning_tokens
        .saturating_add(usage.reasoning_tokens);
}

#[derive(Clone, Copy, Default)]
pub(in crate::engine) struct BudgetUsage {
    pub(super) cost_micros_usd: u64,
    pub(super) ai_credit_micros: u64,
    pub(super) subscription_tokens: u64,
}

pub(super) fn cost_units(cost: &Cost) -> BudgetUsage {
    match cost {
        Cost::Monetary {
            amount_micros,
            currency,
        } if currency.eq_ignore_ascii_case("USD") => BudgetUsage {
            cost_micros_usd: *amount_micros,
            ..BudgetUsage::default()
        },
        Cost::AiCredits { credits_micros, .. } => BudgetUsage {
            ai_credit_micros: *credits_micros,
            ..BudgetUsage::default()
        },
        Cost::SubscriptionQuota { .. } => match cost.subscription_token_accounting() {
            SubscriptionTokenAccounting::Metered(tokens) => BudgetUsage {
                subscription_tokens: tokens,
                ..BudgetUsage::default()
            },
            SubscriptionTokenAccounting::NotApplicable
            | SubscriptionTokenAccounting::Unavailable => BudgetUsage::default(),
        },
        Cost::Monetary { .. } | Cost::Unavailable { .. } => BudgetUsage::default(),
    }
}

pub(super) fn dollar_accounting_complete(cost: &Cost) -> bool {
    matches!(cost, Cost::Monetary { currency, .. } if currency.eq_ignore_ascii_case("USD"))
}

pub(super) async fn persist_incomplete_dollar_caps(
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    budget: &BudgetConfig,
    current: u64,
) -> Result<bool, AgentLoopError> {
    let mut hard_stop = false;
    for (scope, limit) in [
        (BudgetScope::Session, budget.session_cost_cap_micros_usd),
        (BudgetScope::Daily, budget.daily_cost_cap_micros_usd),
    ] {
        let Some(limit) = limit else {
            continue;
        };
        persist_event(
            signals,
            PendingEvent::BudgetStatus {
                turn,
                level: BudgetLevel::HardCap,
                scope,
                unit: BudgetUnit::MicrosUsd,
                current,
                limit,
            },
        )
        .await?;
        hard_stop = true;
    }
    Ok(hard_stop)
}

pub(super) async fn persist_incomplete_budget_caps(
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    budget: &BudgetConfig,
    cost: &Cost,
    current_cost_micros: u64,
    current_credit_micros: u64,
    current_tokens: u64,
) -> Result<bool, AgentLoopError> {
    let mut hard_stop = false;
    if !dollar_accounting_complete(cost) {
        hard_stop |=
            persist_incomplete_dollar_caps(signals, turn, budget, current_cost_micros).await?;
    }
    if matches!(cost, Cost::Unavailable { .. }) {
        for (scope, limit) in [
            (BudgetScope::Session, budget.session_ai_credit_cap_micros),
            (BudgetScope::Daily, budget.daily_ai_credit_cap_micros),
        ] {
            let Some(limit) = limit else {
                continue;
            };
            persist_event(
                signals,
                PendingEvent::BudgetStatus {
                    turn,
                    level: BudgetLevel::HardCap,
                    scope,
                    unit: BudgetUnit::AiCreditMicros,
                    current: current_credit_micros,
                    limit,
                },
            )
            .await?;
            hard_stop = true;
        }
    }
    let token_accounting_unknown = matches!(
        cost.subscription_token_accounting(),
        SubscriptionTokenAccounting::Unavailable
    );
    if token_accounting_unknown {
        for (scope, limit) in [
            (BudgetScope::Session, budget.session_token_cap),
            (BudgetScope::Daily, budget.daily_token_cap),
        ] {
            let Some(limit) = limit else {
                continue;
            };
            persist_event(
                signals,
                PendingEvent::BudgetStatus {
                    turn,
                    level: BudgetLevel::HardCap,
                    scope,
                    unit: BudgetUnit::Tokens,
                    current: current_tokens,
                    limit,
                },
            )
            .await?;
            hard_stop = true;
        }
    }
    Ok(hard_stop)
}

pub(super) fn combine_cost(total: Option<Cost>, next: Cost) -> Cost {
    let Some(total) = total else {
        return next;
    };
    match (total, next) {
        (
            Cost::Monetary {
                amount_micros: left,
                currency: left_currency,
            },
            Cost::Monetary {
                amount_micros: right,
                currency: right_currency,
            },
        ) if left_currency == right_currency => Cost::Monetary {
            amount_micros: left.saturating_add(right),
            currency: left_currency,
        },
        (
            Cost::AiCredits {
                credits_micros: left,
                nominal_amount_micros: left_nominal,
                currency: left_currency,
            },
            Cost::AiCredits {
                credits_micros: right,
                nominal_amount_micros: right_nominal,
                currency: right_currency,
            },
        ) if left_currency == right_currency => Cost::AiCredits {
            credits_micros: left.saturating_add(right),
            nominal_amount_micros: left_nominal
                .and_then(|value| value.parse::<u64>().ok())
                .zip(right_nominal.and_then(|value| value.parse::<u64>().ok()))
                .map(|(left, right)| left.saturating_add(right).to_string()),
            currency: left_currency,
        },
        (
            Cost::SubscriptionQuota {
                used: left,
                unit: left_unit,
            },
            Cost::SubscriptionQuota {
                used: right,
                unit: right_unit,
            },
        ) if left_unit == right_unit => Cost::SubscriptionQuota {
            used: left
                .and_then(|value| value.parse::<u64>().ok())
                .zip(right.and_then(|value| value.parse::<u64>().ok()))
                .map(|(left, right)| left.saturating_add(right).to_string()),
            unit: left_unit,
        },
        (Cost::Unavailable { reason }, _) | (_, Cost::Unavailable { reason }) => {
            Cost::Unavailable { reason }
        }
        _ => Cost::Unavailable {
            reason: "mixed accounting units cannot be aggregated".to_owned(),
        },
    }
}

pub(in crate::engine) struct BudgetCheck {
    pub(in crate::engine) events: Vec<PendingEvent>,
    pub(in crate::engine) hard_stop: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::engine) struct SessionAccountingFallback {
    pub(super) cost_micros_usd: u64,
    pub(super) ai_credit_micros: u64,
    pub(super) subscription_tokens: u64,
    pub(super) unmetered_subscription_quota_entries: u64,
    pub(super) subscription_quota_entries: u64,
    pub(super) cost_unavailable_entries: u64,
    pub(super) non_usd_monetary_entries: u64,
}

pub(in crate::engine) fn session_accounting_fallback(
    accounting: &[TurnAccounting],
) -> SessionAccountingFallback {
    let mut fallback = SessionAccountingFallback::default();
    for turn in accounting {
        match &turn.cost {
            Cost::Monetary {
                amount_micros,
                currency,
            } if currency.eq_ignore_ascii_case("USD") => {
                fallback.cost_micros_usd = fallback.cost_micros_usd.saturating_add(*amount_micros);
            }
            Cost::AiCredits { credits_micros, .. } => {
                fallback.ai_credit_micros =
                    fallback.ai_credit_micros.saturating_add(*credits_micros);
            }
            Cost::SubscriptionQuota { .. } => {
                fallback.subscription_quota_entries =
                    fallback.subscription_quota_entries.saturating_add(1);
                match turn.cost.subscription_token_accounting() {
                    SubscriptionTokenAccounting::Metered(tokens) => {
                        fallback.subscription_tokens =
                            fallback.subscription_tokens.saturating_add(tokens);
                    }
                    SubscriptionTokenAccounting::Unavailable => {
                        fallback.unmetered_subscription_quota_entries = fallback
                            .unmetered_subscription_quota_entries
                            .saturating_add(1);
                    }
                    SubscriptionTokenAccounting::NotApplicable => {}
                }
            }
            Cost::Unavailable { .. } => {
                fallback.cost_unavailable_entries =
                    fallback.cost_unavailable_entries.saturating_add(1);
            }
            Cost::Monetary { .. } => {
                fallback.non_usd_monetary_entries =
                    fallback.non_usd_monetary_entries.saturating_add(1);
            }
        }
    }
    fallback
}

pub(super) fn push_cap_event(
    events: &mut Vec<PendingEvent>,
    turn: u64,
    scope: BudgetScope,
    unit: BudgetUnit,
    current: u64,
    limit: Option<u64>,
    warn_at_percent: u8,
) -> bool {
    let Some(limit) = limit else {
        return false;
    };
    if current >= limit {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::HardCap,
            scope,
            unit,
            current,
            limit,
        });
        return true;
    }
    let warning = u128::from(limit)
        .saturating_mul(u128::from(warn_at_percent))
        .div_ceil(100);
    if u128::from(current) >= warning {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::Warning,
            scope,
            unit,
            current,
            limit,
        });
    }
    false
}

#[allow(clippy::too_many_lines)]
pub(in crate::engine) async fn evaluate_budget(
    turn: u64,
    state_clock: &dyn EventClock,
    sink: &Arc<dyn SessionEventSink>,
    budget: &BudgetConfig,
    local_session: SessionAccountingFallback,
    current_turn: BudgetUsage,
) -> Result<BudgetCheck, AgentLoopError> {
    if budget.session_cost_cap_micros_usd.is_none()
        && budget.daily_cost_cap_micros_usd.is_none()
        && budget.session_ai_credit_cap_micros.is_none()
        && budget.daily_ai_credit_cap_micros.is_none()
        && budget.spend_rate_alarm_micros_usd_per_minute.is_none()
        && budget.ai_credit_rate_alarm_micros_per_minute.is_none()
        && budget.session_token_cap.is_none()
        && budget.daily_token_cap.is_none()
        && budget.token_rate_alarm_per_minute.is_none()
    {
        return Ok(BudgetCheck {
            events: Vec::new(),
            hard_stop: false,
        });
    }
    let now = state_clock.unix_time_millis();
    let ledger = sink
        .budget_totals(BudgetLedgerQuery {
            now_unix_ms: now,
            utc_day_start_unix_ms: now.saturating_sub(now % 86_400_000),
            trailing_minute_start_unix_ms: now.saturating_sub(60_000),
        })
        .await?;
    let session_cost = if ledger.authoritative {
        ledger.session_cost_micros_usd
    } else {
        local_session.cost_micros_usd
    }
    .saturating_add(current_turn.cost_micros_usd);
    let session_credits = if ledger.authoritative {
        ledger.session_ai_credit_micros
    } else {
        local_session.ai_credit_micros
    }
    .saturating_add(current_turn.ai_credit_micros);
    let session_tokens = if ledger.authoritative {
        ledger.session_subscription_tokens
    } else {
        local_session.subscription_tokens
    }
    .saturating_add(current_turn.subscription_tokens);
    let daily_cost = ledger
        .daily_cost_micros_usd
        .saturating_add(current_turn.cost_micros_usd);
    let daily_credits = ledger
        .daily_ai_credit_micros
        .saturating_add(current_turn.ai_credit_micros);
    let trailing_cost = ledger
        .trailing_minute_cost_micros_usd
        .saturating_add(current_turn.cost_micros_usd);
    let trailing_credits = ledger
        .trailing_minute_ai_credit_micros
        .saturating_add(current_turn.ai_credit_micros);
    let daily_tokens = ledger
        .daily_subscription_tokens
        .saturating_add(current_turn.subscription_tokens);
    let trailing_tokens = ledger
        .trailing_minute_subscription_tokens
        .saturating_add(current_turn.subscription_tokens);
    let mut events = Vec::new();
    let mut hard_stop = false;
    if !ledger.authoritative {
        for (unit, current, limit) in [
            (
                BudgetUnit::MicrosUsd,
                daily_cost,
                budget.daily_cost_cap_micros_usd,
            ),
            (
                BudgetUnit::AiCreditMicros,
                daily_credits,
                budget.daily_ai_credit_cap_micros,
            ),
        ] {
            if let Some(limit) = limit {
                events.push(PendingEvent::BudgetStatus {
                    turn,
                    level: BudgetLevel::HardCap,
                    scope: BudgetScope::Daily,
                    unit,
                    current,
                    limit,
                });
                hard_stop = true;
            }
        }
    }
    let session_accounting_incomplete = if ledger.authoritative {
        ledger.session_subscription_quota_entries > 0
            || ledger.session_cost_unavailable_entries > 0
            || ledger.session_non_usd_monetary_entries > 0
    } else {
        local_session.subscription_quota_entries > 0
            || local_session.cost_unavailable_entries > 0
            || local_session.non_usd_monetary_entries > 0
    };
    if let Some(limit) = budget.session_cost_cap_micros_usd
        && session_accounting_incomplete
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::HardCap,
            scope: BudgetScope::Session,
            unit: BudgetUnit::MicrosUsd,
            current: session_cost,
            limit,
        });
        hard_stop = true;
    }
    let session_credit_accounting_incomplete = if ledger.authoritative {
        ledger.session_cost_unavailable_entries > 0
    } else {
        local_session.cost_unavailable_entries > 0
    };
    if let Some(limit) = budget.session_ai_credit_cap_micros
        && session_credit_accounting_incomplete
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::HardCap,
            scope: BudgetScope::Session,
            unit: BudgetUnit::AiCreditMicros,
            current: session_credits,
            limit,
        });
        hard_stop = true;
    }
    let daily_accounting_incomplete = ledger.daily_subscription_quota_entries > 0
        || ledger.daily_cost_unavailable_entries > 0
        || ledger.daily_non_usd_monetary_entries > 0;
    if ledger.authoritative
        && let Some(limit) = budget.daily_cost_cap_micros_usd
        && daily_accounting_incomplete
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::HardCap,
            scope: BudgetScope::Daily,
            unit: BudgetUnit::MicrosUsd,
            current: daily_cost,
            limit,
        });
        hard_stop = true;
    }
    let session_token_accounting_incomplete = if ledger.authoritative {
        ledger.session_unmetered_subscription_quota_entries > 0
    } else {
        local_session.unmetered_subscription_quota_entries > 0
    };
    if let Some(limit) = budget.session_token_cap
        && session_token_accounting_incomplete
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::HardCap,
            scope: BudgetScope::Session,
            unit: BudgetUnit::Tokens,
            current: session_tokens,
            limit,
        });
        hard_stop = true;
    }
    if ledger.authoritative
        && let Some(limit) = budget.daily_token_cap
        && ledger.daily_unmetered_subscription_quota_entries > 0
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::HardCap,
            scope: BudgetScope::Daily,
            unit: BudgetUnit::Tokens,
            current: daily_tokens,
            limit,
        });
        hard_stop = true;
    }
    if ledger.authoritative
        && let Some(limit) = budget.daily_ai_credit_cap_micros
        && ledger.daily_cost_unavailable_entries > 0
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::HardCap,
            scope: BudgetScope::Daily,
            unit: BudgetUnit::AiCreditMicros,
            current: daily_credits,
            limit,
        });
        hard_stop = true;
    }
    hard_stop |= push_cap_event(
        &mut events,
        turn,
        BudgetScope::Session,
        BudgetUnit::MicrosUsd,
        session_cost,
        budget.session_cost_cap_micros_usd,
        budget.warn_at_percent,
    );
    if ledger.authoritative {
        hard_stop |= push_cap_event(
            &mut events,
            turn,
            BudgetScope::Daily,
            BudgetUnit::MicrosUsd,
            daily_cost,
            budget.daily_cost_cap_micros_usd,
            budget.warn_at_percent,
        );
    }
    hard_stop |= push_cap_event(
        &mut events,
        turn,
        BudgetScope::Session,
        BudgetUnit::Tokens,
        session_tokens,
        budget.session_token_cap,
        budget.warn_at_percent,
    );
    if ledger.authoritative {
        hard_stop |= push_cap_event(
            &mut events,
            turn,
            BudgetScope::Daily,
            BudgetUnit::Tokens,
            daily_tokens,
            budget.daily_token_cap,
            budget.warn_at_percent,
        );
    }
    hard_stop |= push_cap_event(
        &mut events,
        turn,
        BudgetScope::Session,
        BudgetUnit::AiCreditMicros,
        session_credits,
        budget.session_ai_credit_cap_micros,
        budget.warn_at_percent,
    );
    if ledger.authoritative {
        hard_stop |= push_cap_event(
            &mut events,
            turn,
            BudgetScope::Daily,
            BudgetUnit::AiCreditMicros,
            daily_credits,
            budget.daily_ai_credit_cap_micros,
            budget.warn_at_percent,
        );
    }
    if budget
        .spend_rate_alarm_micros_usd_per_minute
        .is_some_and(|limit| trailing_cost >= limit)
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::SpendRateAlarm,
            scope: BudgetScope::TrailingMinute,
            unit: BudgetUnit::MicrosUsd,
            current: trailing_cost,
            limit: budget
                .spend_rate_alarm_micros_usd_per_minute
                .unwrap_or_default(),
        });
    }
    if budget
        .ai_credit_rate_alarm_micros_per_minute
        .is_some_and(|limit| trailing_credits >= limit)
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::SpendRateAlarm,
            scope: BudgetScope::TrailingMinute,
            unit: BudgetUnit::AiCreditMicros,
            current: trailing_credits,
            limit: budget
                .ai_credit_rate_alarm_micros_per_minute
                .unwrap_or_default(),
        });
    }
    if budget
        .token_rate_alarm_per_minute
        .is_some_and(|limit| trailing_tokens >= limit)
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::SpendRateAlarm,
            scope: BudgetScope::TrailingMinute,
            unit: BudgetUnit::Tokens,
            current: trailing_tokens,
            limit: budget.token_rate_alarm_per_minute.unwrap_or_default(),
        });
    }
    Ok(BudgetCheck { events, hard_stop })
}

#[allow(clippy::too_many_lines)]
pub(in crate::engine) async fn build_cost_snapshot(
    state: &ActorState,
    config: &SessionActorConfig,
) -> Result<CostSnapshot, AgentLoopError> {
    let now = state.event_clock.unix_time_millis();
    let day_start = now.saturating_sub(now % 86_400_000);
    let ledger = config
        .event_sink
        .budget_totals(BudgetLedgerQuery {
            now_unix_ms: now,
            utc_day_start_unix_ms: day_start,
            trailing_minute_start_unix_ms: now.saturating_sub(60_000),
        })
        .await?;
    let mut usage = Usage {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
    };
    let mut local_cost = 0_u64;
    let mut local_credits = 0_u64;
    let mut local_subscription = 0_u64;
    let mut local_subscription_tokens = 0_u64;
    let mut local_unmetered_subscription = 0_u64;
    let mut local_unavailable = 0_u64;
    let mut local_non_usd = 0_u64;
    for turn in &state.accounting {
        add_usage(&mut usage, &turn.usage);
        match &turn.cost {
            Cost::Monetary {
                amount_micros,
                currency,
            } if currency.eq_ignore_ascii_case("USD") => {
                local_cost = local_cost.saturating_add(*amount_micros);
            }
            Cost::AiCredits { credits_micros, .. } => {
                local_credits = local_credits.saturating_add(*credits_micros);
            }
            Cost::Monetary { .. } => local_non_usd = local_non_usd.saturating_add(1),
            Cost::SubscriptionQuota { .. } => {
                local_subscription = local_subscription.saturating_add(1);
                match turn.cost.subscription_token_accounting() {
                    SubscriptionTokenAccounting::Metered(tokens) => {
                        local_subscription_tokens =
                            local_subscription_tokens.saturating_add(tokens);
                    }
                    SubscriptionTokenAccounting::Unavailable => {
                        local_unmetered_subscription =
                            local_unmetered_subscription.saturating_add(1);
                    }
                    SubscriptionTokenAccounting::NotApplicable => {}
                }
            }
            Cost::Unavailable { .. } => {
                local_unavailable = local_unavailable.saturating_add(1);
            }
        }
    }
    let session_cost = ledger.session_cost_micros_usd.max(local_cost);
    let session_credits = ledger.session_ai_credit_micros.max(local_credits);
    let session_tokens = ledger
        .session_subscription_tokens
        .max(local_subscription_tokens);
    // UTC-day/trailing windows are storage-authoritative. Session totals are
    // safely recoverable from this session's durable events; day membership is not.
    let daily_cost = ledger.daily_cost_micros_usd;
    let daily_credits = ledger.daily_ai_credit_micros;
    let session_subscription = ledger
        .session_subscription_quota_entries
        .max(local_subscription);
    let session_unavailable = ledger
        .session_cost_unavailable_entries
        .max(local_unavailable);
    let session_non_usd = ledger.session_non_usd_monetary_entries.max(local_non_usd);
    let session_unmetered_subscription = ledger
        .session_unmetered_subscription_quota_entries
        .max(local_unmetered_subscription);
    let budget = config.model.budget_config();
    let hard_cap_reached = budget
        .session_cost_cap_micros_usd
        .is_some_and(|limit| session_cost >= limit)
        || budget
            .daily_cost_cap_micros_usd
            .is_some_and(|limit| daily_cost >= limit)
        || budget
            .session_ai_credit_cap_micros
            .is_some_and(|limit| session_credits >= limit)
        || budget
            .daily_ai_credit_cap_micros
            .is_some_and(|limit| daily_credits >= limit)
        || budget
            .session_token_cap
            .is_some_and(|limit| session_tokens >= limit || session_unmetered_subscription > 0)
        || budget.daily_token_cap.is_some_and(|limit| {
            ledger.daily_subscription_tokens >= limit
                || ledger.daily_unmetered_subscription_quota_entries > 0
        });
    let input_total = usage
        .input_tokens
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_write_tokens);
    let cache_hit_basis_points = if input_total == 0 {
        0
    } else {
        u16::try_from(
            u128::from(usage.cache_read_tokens).saturating_mul(10_000) / u128::from(input_total),
        )
        .unwrap_or(10_000)
    };
    let date = format_unix_rfc3339(now / 1_000, 0);
    Ok(CostSnapshot {
        utc_day: date.get(..10).unwrap_or("1970-01-01").to_owned(),
        turns: state.accounting.clone(),
        session_usage: usage,
        session_cost_micros_usd: session_cost,
        session_ai_credit_micros: session_credits,
        session_subscription_tokens: session_tokens,
        daily_cost_micros_usd: daily_cost,
        daily_ai_credit_micros: daily_credits,
        daily_subscription_tokens: ledger.daily_subscription_tokens,
        trailing_minute_cost_micros_usd: ledger.trailing_minute_cost_micros_usd,
        trailing_minute_ai_credit_micros: ledger.trailing_minute_ai_credit_micros,
        trailing_minute_subscription_tokens: ledger.trailing_minute_subscription_tokens,
        cache_hit_basis_points,
        session_cost_cap_micros_usd: budget.session_cost_cap_micros_usd,
        daily_cost_cap_micros_usd: budget.daily_cost_cap_micros_usd,
        session_ai_credit_cap_micros: budget.session_ai_credit_cap_micros,
        daily_ai_credit_cap_micros: budget.daily_ai_credit_cap_micros,
        session_token_cap: budget.session_token_cap,
        daily_token_cap: budget.daily_token_cap,
        spend_rate_alarm_micros_usd_per_minute: budget.spend_rate_alarm_micros_usd_per_minute,
        ai_credit_rate_alarm_micros_per_minute: budget.ai_credit_rate_alarm_micros_per_minute,
        token_rate_alarm_per_minute: budget.token_rate_alarm_per_minute,
        hard_cap_reached,
        session_monetary_accounting_complete: session_subscription == 0
            && session_unavailable == 0
            && session_non_usd == 0,
        daily_monetary_accounting_complete: ledger.daily_subscription_quota_entries == 0
            && ledger.daily_cost_unavailable_entries == 0
            && ledger.daily_non_usd_monetary_entries == 0,
        session_subscription_quota_entries: session_subscription,
        session_cost_unavailable_entries: session_unavailable,
        session_non_usd_monetary_entries: session_non_usd,
        daily_subscription_quota_entries: ledger.daily_subscription_quota_entries,
        daily_cost_unavailable_entries: ledger.daily_cost_unavailable_entries,
        daily_non_usd_monetary_entries: ledger.daily_non_usd_monetary_entries,
    })
}
