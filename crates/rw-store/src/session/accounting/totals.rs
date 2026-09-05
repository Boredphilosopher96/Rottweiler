//! Time-prefix sums over durable accounting facts. Every query visits at most
//! 49 nodes per prefix, independently of the number of retained turns.
use super::{AccountingLedger, AccountingTotals, TurnAccountingEntry, UtcDayKey, UtcTimestamp};
use crate::session::SessionStoreError;
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use rw_types::{Cost, SubscriptionTokenAccounting};

const TIME_BITS: u32 = 49;
const TIME_ROOT: u64 = 1 << TIME_BITS;
const FIELD_COUNT: usize = 7;
const NODE_BYTES: usize = FIELD_COUNT * 16;
const REBUILD_PAGE: i64 = 128;
const MAX_COST_BYTES: i64 = 1024 * 1024;
const USD: usize = 0;
const CREDITS: usize = 1;
const TOKENS: usize = 2;
const QUOTA: usize = 3;
const UNMETERED: usize = 4;
const UNAVAILABLE: usize = 5;
const NON_USD: usize = 6;

#[derive(Clone, Copy, Default)]
struct Sum([u128; FIELD_COUNT]);
impl Sum {
    fn cost(cost: &Cost) -> Self {
        let mut sum = Self::default();
        match cost {
            Cost::Monetary {
                amount_micros,
                currency,
            } if currency.eq_ignore_ascii_case("USD") => {
                sum.0[USD] = u128::from(*amount_micros);
            }
            Cost::Monetary { .. } => sum.0[NON_USD] = 1,
            Cost::AiCredits { credits_micros, .. } => sum.0[CREDITS] = u128::from(*credits_micros),
            Cost::SubscriptionQuota { .. } => {
                sum.0[QUOTA] = 1;
                match cost.subscription_token_accounting() {
                    SubscriptionTokenAccounting::Metered(tokens) => {
                        sum.0[TOKENS] = u128::from(tokens)
                    }
                    SubscriptionTokenAccounting::Unavailable => sum.0[UNMETERED] = 1,
                    SubscriptionTokenAccounting::NotApplicable => {}
                }
            }
            Cost::Unavailable { .. } => sum.0[UNAVAILABLE] = 1,
        }
        sum
    }
    fn add(&mut self, other: Self) -> Result<(), SessionStoreError> {
        for (total, value) in self.0.iter_mut().zip(other.0) {
            *total = total
                .checked_add(value)
                .ok_or(SessionStoreError::AccountingOverflow)?;
        }
        Ok(())
    }
    fn subtract(self, other: Self) -> Result<Self, SessionStoreError> {
        let mut result = Self::default();
        for ((output, total), value) in result.0.iter_mut().zip(self.0).zip(other.0) {
            *output = total
                .checked_sub(value)
                .ok_or(SessionStoreError::CorruptAccountingTotals)?;
        }
        Ok(result)
    }
    fn value(self, field: usize) -> Result<u64, SessionStoreError> {
        u64::try_from(self.0[field]).map_err(|_| SessionStoreError::AccountingOverflow)
    }
    fn encode(self) -> [u8; NODE_BYTES] {
        let mut bytes = [0; NODE_BYTES];
        for (output, value) in bytes.chunks_exact_mut(16).zip(self.0) {
            output.copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }
    fn decode(bytes: &[u8]) -> Result<Self, SessionStoreError> {
        if bytes.len() != NODE_BYTES {
            return Err(SessionStoreError::CorruptAccountingTotals);
        }
        let mut result = Self::default();
        for (output, value) in result.0.iter_mut().zip(bytes.chunks_exact(16)) {
            *output = u128::from_le_bytes(
                value
                    .try_into()
                    .map_err(|_| SessionStoreError::CorruptAccountingTotals)?,
            );
        }
        Ok(result)
    }
}

// A dense monotonic calendar key. Padding invalid calendar dates is harmless:
// queries and updates accept only validated timestamps. All supported years fit
// below 2^49 and SQLite's signed integer boundary.
fn time_key(time: &UtcTimestamp) -> u64 {
    let value = time.as_str().as_bytes();
    let number = |start: usize, end: usize| {
        value[start..end]
            .iter()
            .fold(0_u64, |n, byte| n * 10 + u64::from(byte - b'0'))
    };
    (((((number(0, 4) * 12 + number(5, 7) - 1) * 32 + number(8, 10) - 1) * 24 + number(11, 13))
        * 60
        + number(14, 16))
        * 60
        + number(17, 19))
        * 1000
        + number(20, 23)
}
fn sql_node(node: u64) -> Result<i64, SessionStoreError> {
    i64::try_from(node).map_err(|_| SessionStoreError::CorruptAccountingTotals)
}
fn read_node(connection: &Connection, scope: &str, node: u64) -> Result<Sum, SessionStoreError> {
    let bytes: Option<Option<Vec<u8>>> = connection.prepare_cached(
        "SELECT CASE WHEN typeof(totals)='blob' AND length(totals)=112 THEN totals END FROM accounting_totals WHERE scope=?1 AND node=?2"
    )?.query_row(params![scope, sql_node(node)?], |row| row.get(0)).optional()?;
    match bytes {
        None => Ok(Sum::default()),
        Some(Some(bytes)) => Sum::decode(&bytes),
        Some(None) => Err(SessionStoreError::CorruptAccountingTotals),
    }
}
fn add_fact(
    connection: &Connection,
    session: &str,
    time: &UtcTimestamp,
    cost: &Cost,
) -> Result<(), SessionStoreError> {
    let delta = Sum::cost(cost);
    for scope in ["", session] {
        let mut node = time_key(time) + 1;
        while node <= TIME_ROOT {
            let mut sum = read_node(connection, scope, node)?;
            sum.add(delta)?;
            connection.prepare_cached(
                "INSERT INTO accounting_totals(scope,node,totals) VALUES (?1,?2,?3) ON CONFLICT(scope,node) DO UPDATE SET totals=excluded.totals"
            )?.execute(params![scope, sql_node(node)?, sum.encode().as_slice()])?;
            node += node & node.wrapping_neg();
        }
    }
    Ok(())
}
fn prefix(
    connection: &Connection,
    scope: &str,
    key: Option<u64>,
) -> Result<Sum, SessionStoreError> {
    let mut node = key.map_or(0, |key| key + 1);
    let mut sum = Sum::default();
    while node != 0 {
        sum.add(read_node(connection, scope, node)?)?;
        node &= node - 1;
    }
    Ok(sum)
}
fn interval(
    connection: &Connection,
    scope: &str,
    start: u64,
    end: u64,
) -> Result<Sum, SessionStoreError> {
    if start > end {
        return Ok(Sum::default());
    }
    prefix(connection, scope, Some(end))?.subtract(prefix(connection, scope, start.checked_sub(1))?)
}
fn watermark(connection: &Connection) -> Result<i64, SessionStoreError> {
    connection
        .query_row(
            "SELECT projected_rowid FROM accounting_totals_progress WHERE id=1",
            [],
            |row| row.get(0),
        )
        .map_err(SessionStoreError::from)
}
fn tail(connection: &Connection) -> Result<i64, SessionStoreError> {
    Ok(connection
        .query_row(
            "SELECT rowid FROM turn_accounting ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}
pub(super) fn require_complete(connection: &Connection) -> Result<(), SessionStoreError> {
    if watermark(connection)? != tail(connection)? {
        return Err(SessionStoreError::IncompleteAccountingTotals);
    }
    Ok(())
}
/// Called only after a new authority row is inserted in the same transaction.
pub(super) fn record(
    connection: &Connection,
    entry: &TurnAccountingEntry,
) -> Result<(), SessionStoreError> {
    let row_id = connection.last_insert_rowid();
    add_fact(
        connection,
        &entry.session_id,
        &entry.emitted_at_utc,
        &entry.cost,
    )?;
    connection.execute(
        "UPDATE accounting_totals_progress SET projected_rowid=?1 WHERE id=1",
        [row_id],
    )?;
    Ok(())
}

/// Opens and repairs the derived projection in bounded, resumable transactions.
/// Normal reads and duplicate reconciliation do no historical folding.
pub(in crate::session) fn catch_up(connection: &mut Connection) -> Result<(), SessionStoreError> {
    if require_complete(connection).is_ok() {
        return Ok(());
    }
    while catch_up_page(connection)? {}
    Ok(())
}
fn catch_up_page(connection: &mut Connection) -> Result<bool, SessionStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let through = watermark(&transaction)?;
    let end = tail(&transaction)?;
    if through > end {
        return Err(SessionStoreError::CorruptAccountingTotals);
    }
    if through == end {
        return Ok(false);
    }
    let mut statement = transaction.prepare(
        "SELECT rowid, CASE WHEN length(CAST(session_id AS BLOB))<=128 THEN session_id END, CASE WHEN length(CAST(emitted_at_utc AS BLOB))=24 THEN emitted_at_utc END, CASE WHEN length(CAST(cost_json AS BLOB))<=?2 THEN cost_json END FROM turn_accounting WHERE rowid>?1 ORDER BY rowid LIMIT ?3"
    )?;
    let mut rows = statement.query(params![through, MAX_COST_BYTES, REBUILD_PAGE])?;
    let mut last = through;
    while let Some(row) = rows.next()? {
        last = row.get(0)?;
        let session: String = row
            .get::<_, Option<String>>(1)?
            .ok_or(SessionStoreError::CorruptAccountingTotals)?;
        super::validate_session_id(&session)?;
        let time = UtcTimestamp::parse(
            row.get::<_, Option<String>>(2)?
                .ok_or(SessionStoreError::CorruptAccountingTotals)?,
        )?;
        let cost: String = row
            .get::<_, Option<String>>(3)?
            .ok_or(SessionStoreError::CorruptAccountingTotals)?;
        add_fact(&transaction, &session, &time, &serde_json::from_str(&cost)?)?;
    }
    drop(rows);
    drop(statement);
    if last == through {
        return Err(SessionStoreError::CorruptAccountingTotals);
    }
    transaction.execute(
        "UPDATE accounting_totals_progress SET projected_rowid=?1 WHERE id=1",
        [last],
    )?;
    transaction.commit()?;
    Ok(last < end)
}

impl AccountingLedger {
    /// Exact as-of totals. Prefix subtraction uses u128 values so an overflowing
    /// lifetime sum cannot corrupt a smaller, representable time window.
    /// # Errors
    /// Rejects invalid query bounds, an incomplete/corrupt projection, or totals
    /// which cannot fit the public u64 accounting contract.
    pub fn totals(
        &self,
        session_id: &str,
        day: &UtcDayKey,
        start: &UtcTimestamp,
        end: &UtcTimestamp,
    ) -> Result<AccountingTotals, SessionStoreError> {
        super::validate_session_id(session_id)?;
        if start > end {
            return Err(SessionStoreError::InvalidAccountingTimestamp);
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        require_complete(&transaction)?;
        let day_start = UtcTimestamp::parse(format!("{day}T00:00:00.000Z"))?;
        let day_end = UtcTimestamp::parse(format!("{day}T23:59:59.999Z"))?;
        let session = prefix(&transaction, session_id, Some(time_key(end)))?;
        let daily = interval(
            &transaction,
            "",
            time_key(&day_start),
            time_key(end.min(&day_end)),
        )?;
        let window = interval(&transaction, session_id, time_key(start), time_key(end))?;
        let global_window = interval(&transaction, "", time_key(start), time_key(end))?;
        let mut totals = AccountingTotals::empty(session_id, day, start, end);
        totals.session_micros_usd = session.value(USD)?;
        totals.session_ai_credit_micros = session.value(CREDITS)?;
        totals.session_subscription_tokens = session.value(TOKENS)?;
        totals.session_subscription_quota_turns = session.value(QUOTA)?;
        totals.session_unmetered_subscription_quota_turns = session.value(UNMETERED)?;
        totals.session_unavailable_turns = session.value(UNAVAILABLE)?;
        totals.session_non_usd_monetary_turns = session.value(NON_USD)?;
        totals.day_micros_usd = daily.value(USD)?;
        totals.day_ai_credit_micros = daily.value(CREDITS)?;
        totals.day_subscription_tokens = daily.value(TOKENS)?;
        totals.day_subscription_quota_turns = daily.value(QUOTA)?;
        totals.day_unmetered_subscription_quota_turns = daily.value(UNMETERED)?;
        totals.day_unavailable_turns = daily.value(UNAVAILABLE)?;
        totals.day_non_usd_monetary_turns = daily.value(NON_USD)?;
        totals.trailing_session_micros_usd = window.value(USD)?;
        totals.trailing_session_ai_credit_micros = window.value(CREDITS)?;
        totals.trailing_session_subscription_tokens = window.value(TOKENS)?;
        totals.trailing_all_sessions_micros_usd = global_window.value(USD)?;
        totals.trailing_all_sessions_ai_credit_micros = global_window.value(CREDITS)?;
        totals.trailing_all_sessions_subscription_tokens = global_window.value(TOKENS)?;
        Ok(totals)
    }
}

#[cfg(test)]
mod tests;
