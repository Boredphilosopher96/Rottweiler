//! Integer-only spend-rate alarms and hard budget caps.

use serde::{Deserialize, Serialize};
use thiserror::Error;

const MILLIS_PER_MINUTE: u64 = 60_000;
const MILLIS_PER_DAY: u64 = 86_400_000;

/// Workstream responsible for provider usage.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendAttribution {
    MainAgent,
    Compaction,
    Subagent,
}

/// Usage accounting that deliberately cannot collapse unavailable cost to $0.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpendAmount {
    ApiCost { micros_usd: u64 },
    AiCredits { micros: u64 },
    Unavailable { reason: String },
}

/// One normalized usage charge.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpendEntry {
    /// Stable session identity used to separate session and cross-session totals.
    pub session_id: String,
    pub occurred_at_unix_ms: u64,
    pub attribution: SpendAttribution,
    pub amount: SpendAmount,
}

/// Config primitives shared by CLI/config and engine enforcement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpendCaps {
    pub session_cost_cap_micros_usd: Option<u64>,
    pub daily_cost_cap_micros_usd: Option<u64>,
    pub session_ai_credit_cap_micros: Option<u64>,
    pub daily_ai_credit_cap_micros: Option<u64>,
    pub spend_rate_alarm_micros_usd_per_minute: Option<u64>,
    pub ai_credit_rate_alarm_micros_per_minute: Option<u64>,
    pub warn_at_percent: u8,
}

impl Default for SpendCaps {
    fn default() -> Self {
        Self {
            session_cost_cap_micros_usd: None,
            daily_cost_cap_micros_usd: None,
            session_ai_credit_cap_micros: None,
            daily_ai_credit_cap_micros: None,
            spend_rate_alarm_micros_usd_per_minute: None,
            ai_credit_rate_alarm_micros_per_minute: None,
            warn_at_percent: 80,
        }
    }
}

impl SpendCaps {
    /// Validates percentage semantics while allowing a zero hard cap.
    ///
    /// # Errors
    ///
    /// Returns an error unless the warning percentage is 1 through 100.
    pub const fn validate(self) -> Result<Self, SpendConfigError> {
        if self.warn_at_percent == 0 || self.warn_at_percent > 100 {
            Err(SpendConfigError::InvalidWarningPercent(
                self.warn_at_percent,
            ))
        } else {
            Ok(self)
        }
    }
}

/// Invalid spend configuration.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SpendConfigError {
    #[error("warn_at_percent must be in 1..=100, got {0}")]
    InvalidWarningPercent(u8),
}

/// Unit associated with a budget signal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendUnit {
    MicrosUsd,
    AiCreditMicros,
}

/// Typed warning/alarm/hard-cap category.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendSignalKind {
    SessionCapWarning,
    SessionCapReached,
    DailyCapWarning,
    DailyCapReached,
    RateAlarm,
    AccountingUnavailable,
}

/// One user-visible budget signal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpendSignal {
    pub kind: SpendSignalKind,
    pub unit: Option<SpendUnit>,
    pub current: Option<u64>,
    pub limit: Option<u64>,
}

/// Whether cost accounting is authoritative in each evaluation scope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpendCompleteness {
    pub session: bool,
    pub daily: bool,
    pub trailing_minute: bool,
}

/// Spend totals and enforcement result at one point in time.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpendStatus {
    pub session_cost_micros_usd: u64,
    pub daily_cost_micros_usd: u64,
    pub session_ai_credit_micros: u64,
    pub daily_ai_credit_micros: u64,
    pub cost_rate_micros_usd_per_minute: u64,
    pub ai_credit_rate_micros_per_minute: u64,
    pub session_unavailable_entries: u64,
    pub daily_unavailable_entries: u64,
    pub trailing_minute_unavailable_entries: u64,
    /// Scope-specific completeness. False means unknown, never zero dollars.
    pub cost_accounting_complete: SpendCompleteness,
    pub signals: Vec<SpendSignal>,
    pub hard_stop: bool,
}

/// In-memory session ledger. Callers persist entries as engine events.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SpendTracker {
    entries: Vec<SpendEntry>,
}

impl SpendTracker {
    /// Adds a normalized usage charge to this session ledger.
    pub fn record(&mut self, entry: SpendEntry) {
        self.entries.push(entry);
    }

    /// Returns immutable entries for persistence/debug inspection.
    #[must_use]
    pub fn entries(&self) -> &[SpendEntry] {
        &self.entries
    }

    /// Evaluates one selected session plus cross-session UTC-day/minute limits.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid warning percentage.
    #[allow(clippy::too_many_lines)]
    pub fn evaluate(
        &self,
        target_session_id: &str,
        now_unix_ms: u64,
        caps: SpendCaps,
    ) -> Result<SpendStatus, SpendConfigError> {
        let caps = caps.validate()?;
        let day_start = now_unix_ms - (now_unix_ms % MILLIS_PER_DAY);
        let minute_start = now_unix_ms.saturating_sub(MILLIS_PER_MINUTE);
        let mut session_cost = 0_u64;
        let mut daily_cost = 0_u64;
        let mut minute_cost = 0_u64;
        let mut session_credits = 0_u64;
        let mut daily_credits = 0_u64;
        let mut minute_credits = 0_u64;
        let mut session_unavailable = 0_u64;
        let mut daily_unavailable = 0_u64;
        let mut minute_unavailable = 0_u64;

        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.occurred_at_unix_ms <= now_unix_ms)
        {
            let is_today = entry.occurred_at_unix_ms >= day_start;
            let is_minute = entry.occurred_at_unix_ms >= minute_start;
            let is_target_session = entry.session_id == target_session_id;
            match entry.amount {
                SpendAmount::ApiCost { micros_usd } => {
                    if is_target_session {
                        session_cost = session_cost.saturating_add(micros_usd);
                    }
                    if is_today {
                        daily_cost = daily_cost.saturating_add(micros_usd);
                    }
                    if is_minute {
                        minute_cost = minute_cost.saturating_add(micros_usd);
                    }
                }
                SpendAmount::AiCredits { micros } => {
                    if is_target_session {
                        session_credits = session_credits.saturating_add(micros);
                    }
                    if is_today {
                        daily_credits = daily_credits.saturating_add(micros);
                    }
                    if is_minute {
                        minute_credits = minute_credits.saturating_add(micros);
                    }
                }
                SpendAmount::Unavailable { .. } => {
                    if is_target_session {
                        session_unavailable = session_unavailable.saturating_add(1);
                    }
                    if is_today {
                        daily_unavailable = daily_unavailable.saturating_add(1);
                    }
                    if is_minute {
                        minute_unavailable = minute_unavailable.saturating_add(1);
                    }
                }
            }
        }

        let mut signals = Vec::new();
        append_cap_signals(
            &mut signals,
            SpendUnit::MicrosUsd,
            session_cost,
            caps.session_cost_cap_micros_usd,
            caps.warn_at_percent,
            SpendSignalKind::SessionCapWarning,
            SpendSignalKind::SessionCapReached,
        );
        append_cap_signals(
            &mut signals,
            SpendUnit::MicrosUsd,
            daily_cost,
            caps.daily_cost_cap_micros_usd,
            caps.warn_at_percent,
            SpendSignalKind::DailyCapWarning,
            SpendSignalKind::DailyCapReached,
        );
        append_cap_signals(
            &mut signals,
            SpendUnit::AiCreditMicros,
            session_credits,
            caps.session_ai_credit_cap_micros,
            caps.warn_at_percent,
            SpendSignalKind::SessionCapWarning,
            SpendSignalKind::SessionCapReached,
        );
        append_cap_signals(
            &mut signals,
            SpendUnit::AiCreditMicros,
            daily_credits,
            caps.daily_ai_credit_cap_micros,
            caps.warn_at_percent,
            SpendSignalKind::DailyCapWarning,
            SpendSignalKind::DailyCapReached,
        );
        append_rate_signal(
            &mut signals,
            SpendUnit::MicrosUsd,
            minute_cost,
            caps.spend_rate_alarm_micros_usd_per_minute,
        );
        append_rate_signal(
            &mut signals,
            SpendUnit::AiCreditMicros,
            minute_credits,
            caps.ai_credit_rate_alarm_micros_per_minute,
        );
        if session_unavailable > 0 || daily_unavailable > 0 || minute_unavailable > 0 {
            signals.push(SpendSignal {
                kind: SpendSignalKind::AccountingUnavailable,
                unit: None,
                current: Some(
                    session_unavailable
                        .max(daily_unavailable)
                        .max(minute_unavailable),
                ),
                limit: None,
            });
        }
        let hard_stop = signals.iter().any(|signal| {
            matches!(
                signal.kind,
                SpendSignalKind::SessionCapReached | SpendSignalKind::DailyCapReached
            )
        });
        Ok(SpendStatus {
            session_cost_micros_usd: session_cost,
            daily_cost_micros_usd: daily_cost,
            session_ai_credit_micros: session_credits,
            daily_ai_credit_micros: daily_credits,
            cost_rate_micros_usd_per_minute: minute_cost,
            ai_credit_rate_micros_per_minute: minute_credits,
            session_unavailable_entries: session_unavailable,
            daily_unavailable_entries: daily_unavailable,
            trailing_minute_unavailable_entries: minute_unavailable,
            cost_accounting_complete: SpendCompleteness {
                session: session_unavailable == 0,
                daily: daily_unavailable == 0,
                trailing_minute: minute_unavailable == 0,
            },
            signals,
            hard_stop,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn append_cap_signals(
    signals: &mut Vec<SpendSignal>,
    unit: SpendUnit,
    current: u64,
    cap: Option<u64>,
    warn_at_percent: u8,
    warning: SpendSignalKind,
    reached: SpendSignalKind,
) {
    let Some(limit) = cap else {
        return;
    };
    let reached_cap = current >= limit;
    let warning_threshold = u128::from(current).saturating_mul(100)
        >= u128::from(limit).saturating_mul(u128::from(warn_at_percent));
    if reached_cap || warning_threshold {
        signals.push(SpendSignal {
            kind: if reached_cap { reached } else { warning },
            unit: Some(unit),
            current: Some(current),
            limit: Some(limit),
        });
    }
}

fn append_rate_signal(
    signals: &mut Vec<SpendSignal>,
    unit: SpendUnit,
    current: u64,
    alarm: Option<u64>,
) {
    if let Some(limit) = alarm.filter(|limit| current >= *limit) {
        signals.push(SpendSignal {
            kind: SpendSignalKind::RateAlarm,
            unit: Some(unit),
            current: Some(current),
            limit: Some(limit),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SpendAmount, SpendAttribution, SpendCaps, SpendEntry, SpendSignalKind, SpendTracker,
    };

    #[test]
    fn zero_cap_is_an_immediate_hard_stop() {
        let status = SpendTracker::default().evaluate(
            "session-a",
            1,
            SpendCaps {
                session_cost_cap_micros_usd: Some(0),
                ..SpendCaps::default()
            },
        );
        assert!(status.is_ok_and(|value| value.hard_stop));
    }

    #[test]
    fn unavailable_cost_is_never_presented_as_zero() {
        let mut tracker = SpendTracker::default();
        tracker.record(SpendEntry {
            session_id: "session-a".into(),
            occurred_at_unix_ms: 100,
            attribution: SpendAttribution::MainAgent,
            amount: SpendAmount::Unavailable {
                reason: "subscription quota".into(),
            },
        });
        let status = tracker.evaluate("session-a", 100, SpendCaps::default());
        assert!(status.is_ok_and(|value| {
            !value.cost_accounting_complete.session
                && !value.cost_accounting_complete.daily
                && !value.cost_accounting_complete.trailing_minute
                && value.session_unavailable_entries == 1
                && value.daily_unavailable_entries == 1
                && value.trailing_minute_unavailable_entries == 1
                && value
                    .signals
                    .iter()
                    .any(|signal| signal.kind == SpendSignalKind::AccountingUnavailable)
        }));
    }

    #[test]
    fn warning_rate_alarm_and_hard_cap_are_distinct() {
        let mut tracker = SpendTracker::default();
        tracker.record(SpendEntry {
            session_id: "session-a".into(),
            occurred_at_unix_ms: 1_000,
            attribution: SpendAttribution::Compaction,
            amount: SpendAmount::ApiCost { micros_usd: 85 },
        });
        let status = tracker.evaluate(
            "session-a",
            1_000,
            SpendCaps {
                session_cost_cap_micros_usd: Some(100),
                spend_rate_alarm_micros_usd_per_minute: Some(80),
                ..SpendCaps::default()
            },
        );
        assert!(status.is_ok_and(|value| {
            !value.hard_stop
                && value
                    .signals
                    .iter()
                    .any(|signal| signal.kind == SpendSignalKind::SessionCapWarning)
                && value
                    .signals
                    .iter()
                    .any(|signal| signal.kind == SpendSignalKind::RateAlarm)
        }));
    }

    #[test]
    fn session_totals_are_selected_and_day_minute_totals_cross_sessions() {
        let mut tracker = SpendTracker::default();
        tracker.record(SpendEntry {
            session_id: "session-a".into(),
            occurred_at_unix_ms: 10_000,
            attribution: SpendAttribution::MainAgent,
            amount: SpendAmount::ApiCost { micros_usd: 60 },
        });
        tracker.record(SpendEntry {
            session_id: "session-b".into(),
            occurred_at_unix_ms: 20_000,
            attribution: SpendAttribution::Subagent,
            amount: SpendAmount::ApiCost { micros_usd: 50 },
        });
        let status = tracker.evaluate(
            "session-a",
            20_000,
            SpendCaps {
                session_cost_cap_micros_usd: Some(100),
                daily_cost_cap_micros_usd: Some(100),
                ..SpendCaps::default()
            },
        );
        assert!(status.is_ok_and(|value| {
            value.session_cost_micros_usd == 60
                && value.daily_cost_micros_usd == 110
                && value.cost_rate_micros_usd_per_minute == 110
                && value.hard_stop
                && value.signals.iter().any(|signal| {
                    signal.kind == SpendSignalKind::DailyCapReached && signal.current == Some(110)
                })
                && !value
                    .signals
                    .iter()
                    .any(|signal| signal.kind == SpendSignalKind::SessionCapReached)
        }));
    }

    #[test]
    fn utc_day_rollover_keeps_session_total_but_resets_cross_session_day() {
        let mut tracker = SpendTracker::default();
        tracker.record(SpendEntry {
            session_id: "session-a".into(),
            occurred_at_unix_ms: 86_399_999,
            attribution: SpendAttribution::MainAgent,
            amount: SpendAmount::ApiCost { micros_usd: 70 },
        });
        tracker.record(SpendEntry {
            session_id: "session-b".into(),
            occurred_at_unix_ms: 86_400_001,
            attribution: SpendAttribution::MainAgent,
            amount: SpendAmount::ApiCost { micros_usd: 20 },
        });
        let status = tracker.evaluate("session-a", 86_400_001, SpendCaps::default());
        assert!(status.is_ok_and(|value| {
            value.session_cost_micros_usd == 70
                && value.daily_cost_micros_usd == 20
                && value.cost_rate_micros_usd_per_minute == 90
        }));
    }

    #[test]
    fn future_entries_are_excluded_from_every_scope() {
        let mut tracker = SpendTracker::default();
        tracker.record(SpendEntry {
            session_id: "session-a".into(),
            occurred_at_unix_ms: 2_000,
            attribution: SpendAttribution::MainAgent,
            amount: SpendAmount::ApiCost { micros_usd: 999 },
        });
        tracker.record(SpendEntry {
            session_id: "session-b".into(),
            occurred_at_unix_ms: 3_000,
            attribution: SpendAttribution::MainAgent,
            amount: SpendAmount::Unavailable {
                reason: "future fixture".into(),
            },
        });
        let status = tracker.evaluate("session-a", 1_000, SpendCaps::default());
        assert!(status.is_ok_and(|value| {
            value.session_cost_micros_usd == 0
                && value.daily_cost_micros_usd == 0
                && value.cost_rate_micros_usd_per_minute == 0
                && value.cost_accounting_complete.session
                && value.cost_accounting_complete.daily
                && value.cost_accounting_complete.trailing_minute
        }));
    }
}
