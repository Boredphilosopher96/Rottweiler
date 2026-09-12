use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Replay-injectable timestamp source for event metadata.
pub trait EventClock: Send + Sync {
    fn emitted_at(&self) -> String;

    /// Milliseconds since the Unix epoch used for deterministic budget windows.
    fn unix_time_millis(&self) -> u64 {
        0
    }
}

/// UTC wall-clock timestamps for production sessions.
#[derive(Debug, Default)]
pub struct SystemEventClock;

impl EventClock for SystemEventClock {
    fn emitted_at(&self) -> String {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        format_unix_rfc3339(elapsed.as_secs(), elapsed.subsec_millis())
    }

    fn unix_time_millis(&self) -> u64 {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    }
}

/// Time bounds for a storage-neutral cross-session accounting query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetLedgerQuery {
    pub now_unix_ms: u64,
    pub utc_day_start_unix_ms: u64,
    pub trailing_minute_start_unix_ms: u64,
}

/// Reconciled totals supplied by durable storage for budget enforcement.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BudgetLedgerTotals {
    /// True when totals came from a durable reconciled cross-session ledger.
    pub authoritative: bool,
    pub session_cost_micros_usd: u64,
    pub session_ai_credit_micros: u64,
    pub daily_cost_micros_usd: u64,
    pub daily_ai_credit_micros: u64,
    pub trailing_minute_cost_micros_usd: u64,
    pub trailing_minute_ai_credit_micros: u64,
    pub session_subscription_tokens: u64,
    pub daily_subscription_tokens: u64,
    pub trailing_minute_subscription_tokens: u64,
    pub session_subscription_quota_entries: u64,
    pub session_cost_unavailable_entries: u64,
    pub session_non_usd_monetary_entries: u64,
    pub daily_subscription_quota_entries: u64,
    pub session_unmetered_subscription_quota_entries: u64,
    pub daily_unmetered_subscription_quota_entries: u64,
    pub daily_cost_unavailable_entries: u64,
    pub daily_non_usd_monetary_entries: u64,
}

pub(super) fn format_unix_rfc3339(seconds: u64, millis: u32) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let second_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

pub(super) fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch.saturating_add(719_468);
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}
