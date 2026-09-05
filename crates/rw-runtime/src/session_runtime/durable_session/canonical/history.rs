//! Charged canonical read transactions retained across bounded context pages.
use super::{DurableEventSink, persistence};
use crate::journal_service::JournalReadLease;
use async_trait::async_trait;
use rw_core::{
    AgentLoopError,
    recovery::{
        CanonicalHistory, ConversationCut, ConversationPage, HistoryMaterializationLimits,
        RecoveryBootstrap, SessionHistory, SessionHistoryView,
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
        let lease = self
            .journal_service
            .capture(&self.session_id)
            .map_err(persistence)?;
        let publication = Arc::clone(&self.registration.publisher);
        let reads = Arc::clone(&self.reads);
        self.reads
            .run(lease, move |lease| {
                let history = owner.snapshot(lease, &publication)?;
                let cut = history.head().conversation;
                let through = history.head().next_sequence.checked_sub(1).map(SequenceId);
                Ok(Arc::new(CapturedHistory {
                    history: Arc::new(Mutex::new(history)),
                    lease: lease.clone(),
                    reads,
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
    async fn bootstrap(&self) -> Result<RecoveryBootstrap, AgentLoopError> {
        self.query(CanonicalHistory::bootstrap).await
    }
    async fn conversation_page(
        &self,
        range: Range<u64>,
        limits: HistoryMaterializationLimits,
    ) -> Result<ConversationPage, AgentLoopError> {
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
    ) -> Result<T, AgentLoopError> {
        let lease = self.lease.clone();
        // The transaction must remain owned by the worker if its waiter drops.
        let history = Arc::clone(&self.history);
        self.reads
            .run((history, lease), move |(history, _lease)| {
                let history = history
                    .lock()
                    .map_err(|_| persistence("history reader poisoned"))?;
                query(&history).map_err(persistence)
            })
            .await
    }
}
