//! Durable accounting facts, reconciliation, and bounded reporting.
mod progress;
pub(super) mod totals;
use super::{
    SessionStoreError,
    journal_io::validate_session_id,
    sqlite_schema,
    sqlite_snapshot::{read_only_index_snapshot, same_file_identity, validate_read_only_index},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, TransactionBehavior, params};
use rw_types::{AccountingAttribution, Cost, SequenceId, TurnId, Usage};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Validated UTC calendar-day key used by accounting queries and projections.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct UtcDayKey(String);

impl UtcDayKey {
    /// Parses an exact `YYYY-MM-DD` UTC day key.
    ///
    /// # Errors
    ///
    /// Returns [`SessionStoreError::InvalidAccountingTimestamp`] for malformed
    /// or impossible calendar dates.
    pub fn parse(value: impl Into<String>) -> Result<Self, SessionStoreError> {
        let value = value.into();
        validate_utc_day(&value)?;
        Ok(Self(value))
    }

    /// Returns the validated wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for UtcDayKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for UtcDayKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Validated millisecond-precision UTC timestamp used as an injected budget
/// and spend-rate clock boundary.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct UtcTimestamp(String);

impl UtcTimestamp {
    /// Parses an exact `YYYY-MM-DDTHH:MM:SS.mmmZ` UTC timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`SessionStoreError::InvalidAccountingTimestamp`] for malformed
    /// or impossible timestamps.
    pub fn parse(value: impl Into<String>) -> Result<Self, SessionStoreError> {
        let value = value.into();
        validate_utc_timestamp(&value)?;
        Ok(Self(value))
    }

    /// Converts milliseconds since the Unix epoch to the normalized UTC wire
    /// representation used by event metadata.
    ///
    /// # Errors
    ///
    /// Returns [`SessionStoreError::InvalidAccountingTimestamp`] when the
    /// instant is outside the four-digit year range supported by the schema.
    pub fn from_unix_millis(unix_millis: u64) -> Result<Self, SessionStoreError> {
        let seconds = unix_millis / 1_000;
        let millis = unix_millis % 1_000;
        let days = i64::try_from(seconds / 86_400)
            .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
        let second_of_day = seconds % 86_400;
        let (year, month, day) = civil_from_days(days);
        if !(1..=9_999).contains(&year) {
            return Err(SessionStoreError::InvalidAccountingTimestamp);
        }
        let hour = second_of_day / 3_600;
        let minute = (second_of_day % 3_600) / 60;
        let second = second_of_day % 60;
        Self::parse(format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
        ))
    }

    /// Returns the validated wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Derives the UTC calendar day from this already-validated timestamp.
    #[must_use]
    pub fn utc_day(&self) -> UtcDayKey {
        UtcDayKey(self.0[..10].to_owned())
    }
}

impl<'de> Deserialize<'de> for UtcTimestamp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for UtcTimestamp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
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

/// One authoritative per-turn accounting fact projected from a durable
/// `TurnFinished` engine event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TurnAccountingEntry {
    /// Session whose turn incurred the cost or quota usage.
    pub session_id: String,
    /// Stable engine turn identifier.
    pub turn_id: TurnId,
    /// Sequence of the authoritative `TurnFinished` event.
    pub sequence_id: SequenceId,
    /// Normalized UTC event timestamp (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
    pub emitted_at_utc: UtcTimestamp,
    /// UTC calendar day (`YYYY-MM-DD`) derived from the injected event clock.
    pub utc_day: UtcDayKey,
    /// Runtime role which incurred this usage and cost.
    pub attribution: AccountingAttribution,
    /// Provider-normalized token usage for this accounting fact.
    pub usage: Usage,
    /// Provider-neutral accounting disposition. Subscription and unavailable
    /// values remain typed instead of becoming zero-cost monetary entries.
    pub cost: Cost,
}

/// Durable totals used by session and calendar-day budget decisions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountingTotals {
    /// Session selected by the query.
    pub session_id: String,
    /// UTC calendar day selected by the query.
    pub utc_day: UtcDayKey,
    /// Inclusive start of the selected UTC day.
    pub utc_day_start_utc: UtcTimestamp,
    /// Inclusive start of the injected trailing spend-rate window.
    pub trailing_window_start_utc: UtcTimestamp,
    /// Inclusive end of the injected trailing spend-rate window.
    pub trailing_window_end_utc: UtcTimestamp,
    /// All-time USD micro-cost for the selected session.
    pub session_micros_usd: u64,
    /// USD micro-cost across all sessions during the selected UTC day.
    pub day_micros_usd: u64,
    /// USD micro-cost for the selected session inside the trailing window.
    pub trailing_session_micros_usd: u64,
    /// USD micro-cost across all sessions inside the trailing window.
    pub trailing_all_sessions_micros_usd: u64,
    /// All-time AI-credit micro-units for the selected session.
    pub session_ai_credit_micros: u64,
    /// AI-credit micro-units across all sessions during the selected UTC day.
    pub day_ai_credit_micros: u64,
    /// AI-credit micro-units for the selected session inside the trailing window.
    pub trailing_session_ai_credit_micros: u64,
    /// AI-credit micro-units across all sessions inside the trailing window.
    pub trailing_all_sessions_ai_credit_micros: u64,
    /// Subscription tokens for the selected session.
    pub session_subscription_tokens: u64,
    /// Subscription tokens across all sessions during the selected UTC day.
    pub day_subscription_tokens: u64,
    /// Subscription tokens for the selected session inside the trailing window.
    pub trailing_session_subscription_tokens: u64,
    /// Subscription tokens across all sessions inside the trailing window.
    pub trailing_all_sessions_subscription_tokens: u64,
    /// Subscription-quota turns in the selected session.
    pub session_subscription_quota_turns: u64,
    /// Subscription-quota turns during the selected UTC day.
    pub day_subscription_quota_turns: u64,
    /// Subscription turns whose quota was absent or not token-denominated.
    pub session_unmetered_subscription_quota_turns: u64,
    /// Unmetered subscription turns during the selected UTC day.
    pub day_unmetered_subscription_quota_turns: u64,
    /// Cost-unavailable turns in the selected session.
    pub session_unavailable_turns: u64,
    /// Cost-unavailable turns during the selected UTC day.
    pub day_unavailable_turns: u64,
    /// Non-USD monetary turns retained for the selected session but excluded
    /// from USD caps.
    pub session_non_usd_monetary_turns: u64,
    /// Non-USD monetary turns during the selected UTC day.
    pub day_non_usd_monetary_turns: u64,
}

/// Durable accounting facts reconciled idempotently from authoritative journal events.
/// Search rebuilds and conversation rewinds never erase charged entries.
#[derive(Clone, Debug)]
pub struct AccountingLedger {
    path: PathBuf,
}

impl AccountingLedger {
    /// Opens the shared database using the declared accounting schema.
    /// Unsupported accounting layouts are rejected without modifying their rows.
    ///
    /// # Errors
    ///
    /// Returns an I/O or `SQLite` schema error.
    pub fn open(root: &Path) -> Result<Self, SessionStoreError> {
        fs::create_dir_all(root)?;
        let ledger = Self {
            path: root.join("index.sqlite"),
        };
        totals::catch_up(&mut ledger.connection()?)?;
        Ok(ledger)
    }

    /// Idempotently records one event-log-derived turn entry.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/time fields, conflicting event
    /// identities, serialization failure, or `SQLite` failure.
    pub fn record(&self, entry: &TurnAccountingEntry) -> Result<(), SessionStoreError> {
        validate_accounting_entry(entry)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_accounting_entry(&transaction, entry)?;
        transaction.commit()?;
        Ok(())
    }

    /// Reconciles a projected event-log prefix without deleting rows written by
    /// concurrently active sessions.
    ///
    /// # Errors
    ///
    /// Returns an invalid-entry, conflict, serialization, or transaction error.
    pub fn reconcile(&self, entries: &[TurnAccountingEntry]) -> Result<(), SessionStoreError> {
        for entry in entries {
            validate_accounting_entry(entry)?;
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for entry in entries {
            insert_accounting_entry(&transaction, entry)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Returns typed entries for one session in numeric event-sequence order.
    ///
    /// # Errors
    ///
    /// Returns an invalid-id, corrupt-row, JSON, or `SQLite` error.
    pub fn entries_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<TurnAccountingEntry>, SessionStoreError> {
        validate_session_id(session_id)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT session_id,turn_id,sequence_id,emitted_at_utc,utc_day,\
                    attribution_json,usage_json,cost_json \
             FROM turn_accounting WHERE session_id=?1 \
             ORDER BY length(sequence_id),sequence_id",
        )?;
        let rows = statement.query_map([session_id], accounting_entry_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(SessionStoreError::from)
    }

    /// Returns every typed entry in stable session and numeric sequence order.
    /// This is primarily useful for validating or copying a derived projection;
    /// rebuild callers should prefer entries projected directly from JSONL.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-row, JSON, or `SQLite` error.
    pub fn entries(&self) -> Result<Vec<TurnAccountingEntry>, SessionStoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT session_id,turn_id,sequence_id,emitted_at_utc,utc_day,\
                    attribution_json,usage_json,cost_json \
             FROM turn_accounting \
             ORDER BY session_id,length(sequence_id),sequence_id",
        )?;
        let rows = statement.query_map([], accounting_entry_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(SessionStoreError::from)
    }

    /// Reads a bounded UTC range from an existing accounting projection
    /// without opening the live database for writes or creating its schema.
    ///
    /// The database and any committed WAL are first copied through the same
    /// descriptor-stable snapshot boundary used by historical session search.
    /// Event logs remain authoritative; this surface is intended for
    /// read-only historical reporting over the continuously reconciled index.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or reversed UTC bounds, an unsafe or
    /// oversized index, a corrupt row, or a result larger than `max_entries`.
    pub fn entries_read_only_bounded(
        root: &Path,
        start_utc: &UtcTimestamp,
        end_utc: &UtcTimestamp,
        max_entries: usize,
    ) -> Result<Vec<TurnAccountingEntry>, SessionStoreError> {
        if start_utc > end_utc {
            return Err(SessionStoreError::InvalidAccountingTimestamp);
        }
        if max_entries > 1_000_000 {
            return Err(SessionStoreError::AccountingQueryLimitTooLarge);
        }
        let sql_limit = max_entries
            .checked_add(1)
            .ok_or(SessionStoreError::LimitOverflow)?;
        let sql_limit = i64::try_from(sql_limit).map_err(|_| SessionStoreError::LimitOverflow)?;
        let path = root.join("index.sqlite");
        let before = validate_read_only_index(&path)?;
        let canonical_root = fs::canonicalize(root)?;
        let canonical_path = fs::canonicalize(&path)?;
        if canonical_path.parent() != Some(canonical_root.as_path()) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        let snapshot = read_only_index_snapshot(&canonical_root, &before)?;
        let snapshot_path = fs::canonicalize(snapshot.path().join("index.sqlite"))?;
        let connection = Connection::open_with_flags(
            snapshot_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let after = validate_read_only_index(&path)?;
        if !same_file_identity(&before, &after) {
            return Err(SessionStoreError::UnsafeSessionIndex);
        }
        sqlite_schema::validate_accounting(&connection)?;
        let mut statement = connection.prepare(
            "SELECT session_id,turn_id,sequence_id,emitted_at_utc,utc_day,\
                    attribution_json,usage_json,cost_json \
             FROM turn_accounting \
             WHERE emitted_at_utc>=?1 AND emitted_at_utc<=?2 \
             ORDER BY emitted_at_utc,session_id,length(sequence_id),sequence_id \
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![start_utc.as_str(), end_utc.as_str(), sql_limit],
            accounting_entry_from_row,
        )?;
        let mut entries = rows.collect::<Result<Vec<_>, _>>()?;
        if entries.len() > max_entries {
            return Err(SessionStoreError::AccountingResultTooLarge { max_entries });
        }
        entries.shrink_to_fit();
        Ok(entries)
    }

    fn connection(&self) -> Result<Connection, SessionStoreError> {
        sqlite_schema::open_accounting_connection(&self.path)
    }
}

impl AccountingTotals {
    fn empty(
        session_id: &str,
        utc_day: &UtcDayKey,
        trailing_window_start_utc: &UtcTimestamp,
        trailing_window_end_utc: &UtcTimestamp,
    ) -> Self {
        Self {
            session_id: session_id.to_owned(),
            utc_day: utc_day.clone(),
            utc_day_start_utc: UtcTimestamp(format!("{utc_day}T00:00:00.000Z")),
            trailing_window_start_utc: trailing_window_start_utc.clone(),
            trailing_window_end_utc: trailing_window_end_utc.clone(),
            session_micros_usd: 0,
            day_micros_usd: 0,
            trailing_session_micros_usd: 0,
            trailing_all_sessions_micros_usd: 0,
            session_ai_credit_micros: 0,
            day_ai_credit_micros: 0,
            trailing_session_ai_credit_micros: 0,
            trailing_all_sessions_ai_credit_micros: 0,
            session_subscription_tokens: 0,
            day_subscription_tokens: 0,
            trailing_session_subscription_tokens: 0,
            trailing_all_sessions_subscription_tokens: 0,
            session_subscription_quota_turns: 0,
            day_subscription_quota_turns: 0,
            session_unmetered_subscription_quota_turns: 0,
            day_unmetered_subscription_quota_turns: 0,
            session_unavailable_turns: 0,
            day_unavailable_turns: 0,
            session_non_usd_monetary_turns: 0,
            day_non_usd_monetary_turns: 0,
        }
    }
}

pub(super) fn validate_accounting_entry(
    entry: &TurnAccountingEntry,
) -> Result<(), SessionStoreError> {
    validate_session_id(&entry.session_id)?;
    if entry.turn_id.0.is_empty() || entry.turn_id.0.len() > 128 {
        return Err(SessionStoreError::InvalidAccountingIdentity);
    }
    if entry.emitted_at_utc.utc_day() != entry.utc_day {
        return Err(SessionStoreError::InvalidAccountingTimestamp);
    }
    totals::validate_cost(&entry.cost)
}

fn validate_utc_day(value: &str) -> Result<(), SessionStoreError> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| !matches!(index, 4 | 7) && !byte.is_ascii_digit())
    {
        return Err(SessionStoreError::InvalidAccountingTimestamp);
    }
    let year = value[0..4]
        .parse::<u32>()
        .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
    let month = value[5..7]
        .parse::<u32>()
        .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
    let day = value[8..10]
        .parse::<u32>()
        .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
    let leap_year =
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return Err(SessionStoreError::InvalidAccountingTimestamp),
    };
    if year == 0 || !(1..=days_in_month).contains(&day) {
        return Err(SessionStoreError::InvalidAccountingTimestamp);
    }
    Ok(())
}

fn validate_utc_timestamp(value: &str) -> Result<(), SessionStoreError> {
    let bytes = value.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
        || bytes.iter().enumerate().any(|(index, byte)| {
            !matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) && !byte.is_ascii_digit()
        })
    {
        return Err(SessionStoreError::InvalidAccountingTimestamp);
    }
    validate_utc_day(&value[..10])?;
    let hour = value[11..13]
        .parse::<u32>()
        .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
    let minute = value[14..16]
        .parse::<u32>()
        .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
    let second = value[17..19]
        .parse::<u32>()
        .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
    let millis = value[20..23]
        .parse::<u32>()
        .map_err(|_| SessionStoreError::InvalidAccountingTimestamp)?;
    if hour > 23 || minute > 59 || second > 59 || millis > 999 {
        return Err(SessionStoreError::InvalidAccountingTimestamp);
    }
    Ok(())
}

pub(super) fn insert_accounting_entry(
    connection: &Connection,
    entry: &TurnAccountingEntry,
) -> Result<(), SessionStoreError> {
    totals::require_complete(connection)?;
    let sequence = entry.sequence_id.0.to_string();
    let attribution_json = serde_json::to_string(&entry.attribution)?;
    let usage_json = serde_json::to_string(&entry.usage)?;
    let cost_json = serde_json::to_string(&entry.cost)?;
    let inserted = connection.execute(
        "INSERT OR IGNORE INTO turn_accounting(\
           session_id,turn_id,sequence_id,emitted_at_utc,utc_day,\
           attribution_json,usage_json,cost_json\
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            entry.session_id,
            entry.turn_id.0,
            sequence,
            entry.emitted_at_utc.as_str(),
            entry.utc_day.as_str(),
            attribution_json,
            usage_json,
            cost_json,
        ],
    )?;
    if inserted == 1 {
        return totals::record(connection, entry);
    }
    let existing = connection
        .query_row(
            "SELECT session_id,turn_id,sequence_id,emitted_at_utc,utc_day,\
                    attribution_json,usage_json,cost_json \
             FROM turn_accounting WHERE session_id=?1 AND sequence_id=?2 \
             LIMIT 1",
            params![entry.session_id, entry.sequence_id.0.to_string()],
            accounting_entry_from_row,
        )
        .optional()?;
    if existing.as_ref() == Some(entry) {
        Ok(())
    } else {
        Err(SessionStoreError::AccountingConflict)
    }
}

fn accounting_entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TurnAccountingEntry> {
    let sequence = row.get::<_, String>(2)?;
    let attribution_json = row.get::<_, String>(5)?;
    let usage_json = row.get::<_, String>(6)?;
    let cost_json = row.get::<_, String>(7)?;
    let sequence_id = sequence.parse::<u64>().map(SequenceId).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let attribution =
        serde_json::from_str::<AccountingAttribution>(&attribution_json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    let usage = serde_json::from_str::<Usage>(&usage_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let cost = serde_json::from_str::<Cost>(&cost_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(TurnAccountingEntry {
        session_id: row.get(0)?,
        turn_id: TurnId(row.get(1)?),
        sequence_id,
        emitted_at_utc: UtcTimestamp::parse(row.get::<_, String>(3)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        utc_day: UtcDayKey::parse(row.get::<_, String>(4)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        attribution,
        usage,
        cost,
    })
}
