//! Admission precedes copying SQLite text or decoding accounting values.
use super::{TurnAccountingEntry, UtcDayKey, UtcTimestamp, validate_accounting_entry};
use crate::session::SessionStoreError;
use rusqlite::{Row, Rows, types::Type};
use rw_types::{SequenceId, TurnId};

const MAX_READ_BYTES: usize = 16 * 1024 * 1024;
const COLUMN_LIMITS: [usize; 8] = [
    128,
    128,
    20,
    24,
    10,
    32,
    1024,
    super::totals::MAX_COST_BYTES,
];

pub(super) fn sql_limit(max_entries: usize) -> Result<i64, SessionStoreError> {
    if max_entries > 1_000_000 {
        return Err(SessionStoreError::AccountingQueryLimitTooLarge);
    }
    i64::try_from(max_entries + 1).map_err(|_| SessionStoreError::LimitOverflow)
}

fn text<'a>(row: &'a Row<'_>, column: usize) -> rusqlite::Result<&'a str> {
    row.get_ref(column)?.as_str().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

fn charge(row: &Row<'_>) -> rusqlite::Result<usize> {
    let mut bytes = 2 * std::mem::size_of::<TurnAccountingEntry>();
    for (column, limit) in COLUMN_LIMITS.into_iter().enumerate() {
        let value = text(row, column)?;
        if value.len() > limit {
            return Err(conversion(
                column,
                SessionStoreError::AccountingEntryTooLarge,
            ));
        }
        // JSON string decoding cannot produce more bytes than its input. Two
        // copies cover growth capacity; the fixed charge covers Vec capacity.
        bytes += value.len() * 2;
    }
    Ok(bytes)
}

pub(super) fn collect(
    rows: &mut Rows<'_>,
    max_entries: usize,
) -> Result<Vec<TurnAccountingEntry>, SessionStoreError> {
    let mut remaining = MAX_READ_BYTES;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        if entries.len() == max_entries {
            return Err(SessionStoreError::AccountingResultTooLarge { max_entries });
        }
        remaining = remaining
            .checked_sub(charge(row)?)
            .ok_or(SessionStoreError::AccountingReadTooLarge)?;
        entries.push(decode(row)?);
    }
    Ok(entries)
}

fn conversion(
    column: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
}

pub(super) fn accounting_entry_from_row(row: &Row<'_>) -> rusqlite::Result<TurnAccountingEntry> {
    charge(row)?;
    decode(row)
}

fn decode(row: &Row<'_>) -> rusqlite::Result<TurnAccountingEntry> {
    let sequence = text(row, 2)?;
    let sequence_id = sequence
        .parse::<u64>()
        .map_err(|error| conversion(2, error))?;
    if sequence != sequence_id.to_string() {
        return Err(conversion(2, SessionStoreError::InvalidAccountingIdentity));
    }
    let entry = TurnAccountingEntry {
        session_id: text(row, 0)?.to_owned(),
        turn_id: TurnId(text(row, 1)?.to_owned()),
        sequence_id: SequenceId(sequence_id),
        emitted_at_utc: UtcTimestamp::parse(text(row, 3)?).map_err(|error| conversion(3, error))?,
        utc_day: UtcDayKey::parse(text(row, 4)?).map_err(|error| conversion(4, error))?,
        attribution: serde_json::from_str(text(row, 5)?).map_err(|error| conversion(5, error))?,
        usage: serde_json::from_str(text(row, 6)?).map_err(|error| conversion(6, error))?,
        cost: serde_json::from_str(text(row, 7)?).map_err(|error| conversion(7, error))?,
    };
    validate_accounting_entry(&entry).map_err(|error| conversion(0, error))?;
    Ok(entry)
}

#[cfg(test)]
mod tests;
