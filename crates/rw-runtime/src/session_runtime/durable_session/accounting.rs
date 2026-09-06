//! Source-prefix reconciliation owns bounded pages and accounting transactions.
use super::super::accounting_projection::{inherited_journal_through, project_accounting};
use super::DurableEventSink;
use miette::{IntoDiagnostic, Result, miette};
use rw_core::{
    EngineEvent,
    recovery::{CanonicalHistory, MAX_ACCOUNTING_PAGE_BYTES, RecoveryError},
};
use rw_store::session::{AccountingLedger, journal::JournalPrefixIdentity};
use std::sync::{Arc, atomic::Ordering};

impl DurableEventSink {
    pub(in crate::session_runtime) async fn reconcile_indexed_accounting(&self) -> Result<()> {
        // Acquire before capturing canonical source. Publication may advance while
        // this worker runs, but accounting cannot overtake its captured prefix.
        let mut progress = Arc::clone(&self.accounting_progress).lock_owned().await;
        let root = self.storage_root.clone();
        let session = self.session_id.clone();
        let dirty = Arc::clone(&self.accounting_dirty);
        self.read_canonical(move |history| {
            let ledger = AccountingLedger::open(&root)?;
            let prefix = reconcile_pages(history, &ledger, &session)?;
            *progress = Some(prefix);
            dirty.store(false, Ordering::Release);
            Ok(())
        })
        .await
        .map_err(|error| miette!("session accounting could not reconcile: {error}"))
    }

    pub(super) fn reconcile_committed_accounting(&self, events: &[EngineEvent]) -> Result<()> {
        let Some(first) = events.first().and_then(EngineEvent::meta) else {
            return Ok(());
        };
        let mut progress = self.accounting_progress.blocking_lock();
        let source = self.registration.publisher.capture();
        let next = source.prefix_identity();
        if *progress == Some(next) {
            return Ok(());
        }
        let inherited = if let Some(canonical) = self.canonical.get() {
            canonical.inherited_journal_through()
        } else {
            inherited_journal_through(&self.storage_root, &self.session_id)?
        };
        let entries = project_accounting(&self.session_id, events, inherited)?;
        if progress.map_or(0, |prefix| prefix.next_sequence) != first.sequence_id.0 {
            // A missing page must be recovered from its source, never skipped by
            // promoting only this newly committed batch's accounting facts.
            if !entries.is_empty() {
                AccountingLedger::open(&self.storage_root)
                    .into_diagnostic()?
                    .reconcile(&entries)
                    .into_diagnostic()?;
            }
            self.accounting_dirty.store(true, Ordering::Release);
            return Ok(());
        }
        if entries.is_empty() {
            // Text-only batches advance bounded local coverage without a SQLite
            // transaction. The next billed boundary durably checkpoints the gap.
            *progress = Some(next);
            return Ok(());
        }
        let ledger = AccountingLedger::open(&self.storage_root).into_diagnostic()?;
        let mut expected = ledger
            .reconciled_prefix(&self.session_id)
            .into_diagnostic()?;
        let mut chunks = entries.chunks(128).peekable();
        while let Some(entries) = chunks.next() {
            let through = if chunks.peek().is_none() {
                source.clone()
            } else {
                source
                    .prefix_through(entries.last().map(|entry| entry.sequence_id))
                    .into_diagnostic()?
            };
            ledger
                .reconcile_prefix(&self.session_id, expected, &through, entries)
                .into_diagnostic()?;
            expected = Some(through.prefix_identity());
        }
        *progress = Some(next);
        Ok(())
    }
}

fn reconcile_pages(
    history: &CanonicalHistory,
    ledger: &AccountingLedger,
    session: &str,
) -> Result<JournalPrefixIdentity, RecoveryError> {
    let mut expected = ledger.reconciled_prefix(session)?;
    loop {
        let batch =
            history.accounting_reconciliation_page(expected, 64, MAX_ACCOUNTING_PAGE_BYTES)?;
        let next = batch.source.prefix_identity();
        if expected == Some(next) {
            return Ok(next);
        }
        let entries = project_accounting(
            session,
            &batch.page.events,
            history.head().inherited_journal_through,
        )
        .map_err(|_| RecoveryError::Invalid("accounting event projection"))?;
        ledger.reconcile_prefix(session, expected, &batch.source, &entries)?;
        if !batch.page.has_more {
            return Ok(next);
        }
        if expected.is_some_and(|prefix| next.next_sequence <= prefix.next_sequence) {
            return Err(RecoveryError::Invalid("accounting page did not advance"));
        }
        expected = Some(next);
    }
}

#[cfg(test)]
mod tests;
