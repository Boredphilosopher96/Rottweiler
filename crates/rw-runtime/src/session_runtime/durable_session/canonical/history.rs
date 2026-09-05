//! Charged canonical read transactions retained across bounded context pages.
use super::{DurableEventSink, persistence};
use crate::journal_service::JournalReadLease;
use async_trait::async_trait;
use rw_core::{
    AgentLoopError,
    recovery::{
        CanonicalHistory, ConversationCut, ConversationPage, HistoryMaterializationLimits,
        HistoryRead, RecoveryBootstrap, SessionHistory, SessionHistoryView,
    },
};
use rw_types::SequenceId;
use std::{
    ops::Range,
    sync::{Arc, Mutex},
};

struct CapturedHistory {
    history: Arc<Mutex<CanonicalHistory>>,
    lease: JournalReadLease,
    reads: Arc<super::super::reads::ReadOperations>,
    journal: Arc<crate::journal_service::JournalService>,
    cut: ConversationCut,
    through: Option<SequenceId>,
}

#[async_trait]
impl SessionHistory for DurableEventSink {
    async fn capture_history(&self) -> Result<Arc<dyn SessionHistoryView>, AgentLoopError> {
        let owner = Arc::clone(
            self.canonical
                .get()
                .ok_or_else(|| persistence("canonical owner is not bound to this session"))?,
        );
        let admission = self.journal_service.admit_read().map_err(persistence)?;
        let session = self.session_id.clone();
        let journal = Arc::clone(&self.journal_service);
        let publication = Arc::clone(&self.registration.publisher);
        let reads = Arc::clone(&self.reads);
        self.reads
            .run(Some(admission), move |admission| {
                let mut lease = admission
                    .take()
                    .ok_or_else(|| persistence("history capture already started"))?
                    .capture(&session)
                    .map_err(persistence)?;
                let history = owner.snapshot(&mut lease, &publication)?;
                let cut = history.head().conversation;
                let through = history.head().next_sequence.checked_sub(1).map(SequenceId);
                Ok(Arc::new(CapturedHistory {
                    history: Arc::new(Mutex::new(history)),
                    lease,
                    reads,
                    journal,
                    cut,
                    through,
                }) as Arc<dyn SessionHistoryView>)
            })
            .await
    }
}

// The Arc is the concrete view. Cloning it retains one admission, never another
// uncharged history transaction. Individual jobs also have bounded read admission.
#[async_trait]
impl SessionHistoryView for CapturedHistory {
    fn through(&self) -> Option<SequenceId> {
        self.through
    }
    fn conversation(&self) -> ConversationCut {
        self.cut
    }
    async fn bootstrap(&self) -> Result<HistoryRead<RecoveryBootstrap>, AgentLoopError> {
        self.query(CanonicalHistory::bootstrap).await
    }
    async fn conversation_page(
        &self,
        range: Range<u64>,
        limits: HistoryMaterializationLimits,
    ) -> Result<HistoryRead<ConversationPage>, AgentLoopError> {
        self.query(move |history| history.conversation_page(range, limits))
            .await
    }
}
impl CapturedHistory {
    async fn query<T: Send + 'static>(
        &self,
        query: impl FnOnce(&CanonicalHistory) -> Result<T, rw_core::recovery::RecoveryError>
        + Send
        + 'static,
    ) -> Result<HistoryRead<T>, AgentLoopError> {
        let admission = self.journal.admit_read().map_err(persistence)?;
        let lease = self.lease.clone();
        // The transaction must remain owned by the worker if its waiter drops.
        let history = Arc::clone(&self.history);
        self.reads
            .run(
                (history, lease, Some(admission)),
                move |(history, _lease, admission)| {
                    let history = history
                        .lock()
                        .map_err(|_| persistence("history reader poisoned"))?;
                    let value = query(&history).map_err(persistence)?;
                    let admission = admission
                        .take()
                        .ok_or_else(|| persistence("history query already completed"))?;
                    Ok(HistoryRead::new(value, admission))
                },
            )
            .await
    }
}
