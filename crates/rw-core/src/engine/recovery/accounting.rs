//! Incremental accounting reconciliation reads exact durable facts by source sequence.

use super::{CanonicalHistory, RecoveryError, read::SourceReader, state::ACCOUNTING};
use rw_store::session::journal::{JournalPrefixIdentity, JournalReadView};
use rw_store::session::recovery_index::{MAX_RECOVERY_BATCH_BYTES, MAX_RECOVERY_BATCH_ROWS};
use rw_types::{EngineEvent, SequenceId};
use std::collections::VecDeque;

pub const MAX_ACCOUNTING_PAGE_BYTES: u64 = 32 * 1024 * 1024;

/// One bounded accounting source page, excluding ordinary conversation and output events.
pub struct RecoveryAccountingPage {
    pub events: Vec<EngineEvent>,
    pub next_cursor: Option<SequenceId>,
    pub has_more: bool,
    pub source_bytes: u64,
}

/// A bounded source page plus the exact prefix committed with its accounting facts.
pub struct AccountingReconciliationPage {
    pub page: RecoveryAccountingPage,
    pub source: JournalReadView,
}

impl CanonicalHistory {
    /// Validate the ledger's previous content identity and resolve its next source page.
    /// A final page advances over non-accounting events to this exact captured tail.
    ///
    /// # Errors
    /// Rejects a foreign, modified, or future prefix and invalid page admission.
    pub fn accounting_reconciliation_page(
        &self,
        after: Option<JournalPrefixIdentity>,
        max_events: usize,
        max_source_bytes: u64,
    ) -> Result<AccountingReconciliationPage, RecoveryError> {
        if let Some(prefix) = after {
            self.source.at_prefix(prefix)?;
        }
        let cursor = after
            .and_then(|prefix| prefix.next_sequence.checked_sub(1))
            .map(SequenceId);
        let page = self.accounting_page(cursor, max_events, max_source_bytes)?;
        let source = if page.has_more {
            self.source.prefix_through(page.next_cursor)?
        } else {
            self.source.clone()
        };
        Ok(AccountingReconciliationPage { page, source })
    }

    /// Read exact accounting events after a source cursor, at this captured prefix.
    /// A byte-limited page advances only through events returned to the caller.
    ///
    /// # Errors
    /// Rejects excessive limits, future cursors, invalid source bindings, or an event
    /// that cannot fit in an empty page. No accounting fact is silently skipped.
    pub fn accounting_page(
        &self,
        after: Option<SequenceId>,
        max_events: usize,
        max_source_bytes: u64,
    ) -> Result<RecoveryAccountingPage, RecoveryError> {
        if max_events == 0
            || max_events > MAX_RECOVERY_BATCH_ROWS
            || max_source_bytes == 0
            || max_source_bytes > MAX_ACCOUNTING_PAGE_BYTES
        {
            return Err(RecoveryError::Limit("accounting page limits"));
        }
        if after.is_some_and(|cursor| cursor.0 >= self.head.next_sequence) {
            return Err(RecoveryError::Invalid(
                "accounting cursor beyond captured prefix",
            ));
        }
        let rows = self.read.page(
            ACCOUNTING,
            0,
            after.map(|sequence| sequence.0),
            max_events,
            MAX_RECOVERY_BATCH_BYTES,
        )?;
        let mut result = RecoveryAccountingPage {
            events: Vec::with_capacity(rows.rows.len()),
            next_cursor: after,
            has_more: rows.has_more,
            source_bytes: 0,
        };
        let mut reader = SourceReader {
            source: &self.source,
            events: VecDeque::new(),
        };
        for row in rows.rows {
            let sequence: SequenceId = serde_json::from_slice(&row.payload)?;
            if row.key.ordinal != sequence.0 || sequence.0 >= self.head.next_sequence {
                return Err(RecoveryError::Invalid("accounting source identity"));
            }
            let event = reader.event(sequence)?;
            if !matches!(
                event,
                EngineEvent::ProviderCallAccounted { .. }
                    | EngineEvent::TurnFinished { .. }
                    | EngineEvent::SessionTitleUpdated {
                        usage: Some(_),
                        cost: Some(_),
                        ..
                    }
                    | EngineEvent::CompactionFinished {
                        usage: Some(_),
                        cost: Some(_),
                        ..
                    }
                    | EngineEvent::CompactionAttemptFinished { .. }
            ) {
                return Err(RecoveryError::Invalid("accounting source selector"));
            }
            let bytes = super::encoding::serialized_size(&event)?;
            if bytes > max_source_bytes - result.source_bytes {
                if result.events.is_empty() {
                    return Err(RecoveryError::Limit(
                        "accounting event exceeds page byte limit",
                    ));
                }
                result.has_more = true;
                break;
            }
            result.source_bytes += bytes;
            result.next_cursor = Some(sequence);
            result.events.push(event);
        }
        Ok(result)
    }
}
