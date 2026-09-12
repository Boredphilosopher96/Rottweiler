//! Accounting facts and their exact reconciled journal prefix commit together.
use super::{
    AccountingLedger, TurnAccountingEntry, insert_accounting_entry, validate_accounting_entry,
};
use crate::session::{
    SessionStoreError,
    journal::{JournalPrefixIdentity, JournalReadView, JournalRoot},
    journal_io::validate_session_id,
};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};

const MAX_PAGE_ENTRIES: usize = 128;

impl AccountingLedger {
    /// Exact source prefix whose accounting facts have all been reconciled.
    ///
    /// # Errors
    /// Rejects invalid session identities, malformed metadata, or database errors.
    pub fn reconciled_prefix(
        &self,
        session_id: &str,
    ) -> Result<Option<JournalPrefixIdentity>, SessionStoreError> {
        validate_session_id(session_id)?;
        read_prefix(&self.connection()?, session_id)
    }

    /// Commit one bounded page of facts and its source prefix atomically.
    /// The caller supplies every accounting fact in the selected source interval.
    ///
    /// # Errors
    /// Rejects foreign journals, changed prefix ancestry, stale progress, invalid
    /// or conflicting entries, and transaction failures. Charged facts are never removed.
    pub fn reconcile_prefix(
        &self,
        session_id: &str,
        expected: Option<JournalPrefixIdentity>,
        through: &JournalReadView,
        entries: &[TurnAccountingEntry],
    ) -> Result<(), SessionStoreError> {
        validate_session_id(session_id)?;
        if entries.len() > MAX_PAGE_ENTRIES {
            return Err(SessionStoreError::AccountingResultTooLarge {
                max_entries: MAX_PAGE_ENTRIES,
            });
        }
        let root = self
            .path
            .parent()
            .ok_or(SessionStoreError::UnsafeSessionIndex)?;
        JournalRoot::open(root)?.validate_view(session_id, through)?;
        through.at_prefix(expected.unwrap_or_else(JournalPrefixIdentity::empty))?;
        let first = expected.map_or(0, |prefix| prefix.next_sequence);
        let next = through.prefix_identity();
        let mut previous = None;
        for entry in entries {
            validate_accounting_entry(entry)?;
            if entry.session_id != session_id
                || entry.sequence_id.0 < first
                || entry.sequence_id.0 >= next.next_sequence
                || previous.is_some_and(|sequence| entry.sequence_id <= sequence)
            {
                return Err(SessionStoreError::InvalidAccountingIdentity);
            }
            previous = Some(entry.sequence_id);
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if read_prefix(&transaction, session_id)? != expected {
            return Err(SessionStoreError::AccountingConflict);
        }
        for entry in entries {
            insert_accounting_entry(&transaction, entry)?;
        }
        transaction.execute(
            "INSERT INTO accounting_progress(session_id,next_sequence,digest) VALUES (?1,?2,?3) ON CONFLICT(session_id) DO UPDATE SET next_sequence=excluded.next_sequence,digest=excluded.digest",
            params![session_id, next.next_sequence.to_string(), next.digest.as_slice()],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

fn read_prefix(
    connection: &Connection,
    session: &str,
) -> Result<Option<JournalPrefixIdentity>, SessionStoreError> {
    let row = connection.query_row(
        "SELECT CASE WHEN length(CAST(next_sequence AS BLOB))<=20 THEN next_sequence END, CASE WHEN length(digest)=32 THEN digest END FROM accounting_progress WHERE session_id=?1",
        [session],
        |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
    ).optional()?;
    row.map(|(sequence, digest)| {
        let sequence = sequence.ok_or(SessionStoreError::CorruptProjectionWatermark)?;
        let next_sequence = sequence
            .parse::<u64>()
            .map_err(|_| SessionStoreError::CorruptProjectionWatermark)?;
        if sequence != next_sequence.to_string() {
            return Err(SessionStoreError::CorruptProjectionWatermark);
        }
        let digest = digest
            .ok_or(SessionStoreError::CorruptProjectionWatermark)?
            .try_into()
            .map_err(|_| SessionStoreError::CorruptProjectionWatermark)?;
        Ok(JournalPrefixIdentity {
            next_sequence,
            digest,
        })
    })
    .transpose()
}

#[cfg(test)]
mod tests;
