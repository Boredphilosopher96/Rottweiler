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
    _derived_admission: Option<crate::journal_service::JournalReadAdmission>,
    reads: Arc<super::super::reads::ReadOperations>,
    journal: Arc<crate::journal_service::JournalService>,
    prompt_shapes: Arc<crate::session_runtime::prompt_shapes::PromptShapeJournal>,
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
        let prompt_shapes = Arc::clone(&self.prompt_shapes);
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
                    _derived_admission: None,
                    reads,
                    journal,
                    prompt_shapes,
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
    async fn recovery_at_completed_turn(
        &self,
        turn: u64,
    ) -> Result<HistoryRead<RecoveryBootstrap>, AgentLoopError> {
        self.query(move |history| history.recovery_at_completed_turn(turn))
            .await
    }
    fn verify_prompt(&self, turn: u64, dump: &rw_types::PromptDump) -> Result<(), AgentLoopError> {
        let (profile, record) = self
            .prompt_shapes
            .shape_at_source(
                turn,
                self.through
                    .ok_or_else(|| persistence("historical prompt source is absent"))?
                    .0,
            )
            .map_err(persistence)?
            .ok_or_else(|| {
                persistence("historical prompt is unavailable: required request shape is missing")
            })?;
        crate::session_runtime::prompt_shapes::validate_historical_prompt_shape(
            dump,
            &dump.tools,
            &profile,
            &record,
        )
        .map_err(persistence)
    }
    async fn prompt_at_turn(
        &self,
        turn: u64,
    ) -> Result<Arc<dyn SessionHistoryView>, AgentLoopError> {
        let admission = self.journal.admit_read().map_err(persistence)?;
        let history = Arc::clone(&self.history);
        let lease = self.lease.clone();
        let reads = Arc::clone(&self.reads);
        let prompt_shapes = Arc::clone(&self.prompt_shapes);
        let journal = Arc::clone(&self.journal);
        self.reads
            .run(Some(admission), move |admission| {
                let selected = history
                    .lock()
                    .map_err(|_| persistence("history reader poisoned"))?
                    .prompt_at_turn(turn)
                    .map_err(persistence)?;
                let cut = selected.head().conversation;
                let through = selected.head().next_sequence.checked_sub(1).map(SequenceId);
                Ok(Arc::new(Self {
                    history: Arc::new(Mutex::new(selected)),
                    lease,
                    reads,
                    journal,
                    prompt_shapes,
                    cut,
                    through,
                    _derived_admission: Some(
                        admission
                            .take()
                            .ok_or_else(|| persistence("prompt view already delivered"))?,
                    ),
                }) as Arc<dyn SessionHistoryView>)
            })
            .await
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
    async fn query<T: RetainedResult + Send + 'static>(
        &self,
        query: impl FnOnce(&CanonicalHistory) -> Result<T, rw_core::recovery::RecoveryError>
        + Send
        + 'static,
    ) -> Result<HistoryRead<T>, AgentLoopError> {
        let retention = self.journal.retain_history()?;
        let admission = self.journal.admit_read().map_err(persistence)?;
        let lease = self.lease.clone();
        // The transaction must remain owned by the worker if its waiter drops.
        let history = Arc::clone(&self.history);
        self.reads
            .run(
                (history, lease, admission, Some(retention)),
                move |(history, _lease, _admission, retention)| {
                    let history = history
                        .lock()
                        .map_err(|_| persistence("history reader poisoned"))?;
                    let mut value = query(&history).map_err(persistence)?;
                    let bytes = value.prepare_retained()?;
                    let mut retention = retention
                        .take()
                        .ok_or_else(|| persistence("history query already completed"))?;
                    retention.resize(bytes)?;
                    Ok(HistoryRead::new(value, retention))
                },
            )
            .await
    }
}

trait RetainedResult {
    fn prepare_retained(&mut self) -> Result<usize, AgentLoopError>;
}
impl RetainedResult for RecoveryBootstrap {
    fn prepare_retained(&mut self) -> Result<usize, AgentLoopError> {
        let bytes = self.retained_bytes().map_err(persistence)?;
        self.prepare_allocations();
        Ok(bytes)
    }
}
impl RetainedResult for ConversationPage {
    fn prepare_retained(&mut self) -> Result<usize, AgentLoopError> {
        self.retained_bytes().map_err(persistence)
    }
}
