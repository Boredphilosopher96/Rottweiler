//! Test actors use the same canonical source selectors as durable production actors.
#![cfg(test)]
use crate::engine::{
    AgentLoopError, SessionActor, SessionActorConfig, SessionHandle,
    durability::{
        AdmittedEventBatch, EventBatchPlan, EventBatchReservation, ExtensionStateView,
        SessionEventSink,
    },
    event_clock::{BudgetLedgerQuery, BudgetLedgerTotals},
    recovery::{
        CanonicalHistory, CanonicalRecovery, ConversationCut, ConversationPage,
        HistoryMaterializationLimits, HistoryRead, RecoveryBootstrap, SessionHistory,
        SessionHistoryView,
    },
    replay::{SessionEventReadView, SessionReplayLimits},
};
use async_trait::async_trait;
use rw_ext::ModeRegistry;
use rw_store::session::journal::{JournalReadView, SegmentedJournal};
use rw_types::SequenceId;
use std::{
    ops::Range,
    sync::{Arc, Mutex},
};

pub(crate) struct UnboundHistory;
#[async_trait]
impl SessionHistory for UnboundHistory {
    async fn capture_history(&self) -> Result<Arc<dyn SessionHistoryView>, AgentLoopError> {
        Err(failure("test actor history must be bound before spawn"))
    }
}

pub(crate) struct TestActorConfig {
    pub recovered: crate::engine::SessionRecoveredState,
    pub inner: SessionActorConfig,
}
impl std::ops::Deref for TestActorConfig {
    type Target = SessionActorConfig;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl std::ops::DerefMut for TestActorConfig {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
impl From<SessionActorConfig> for TestActorConfig {
    fn from(inner: SessionActorConfig) -> Self {
        Self {
            inner,
            recovered: crate::engine::SessionRecoveredState::default(),
        }
    }
}

pub(crate) async fn spawn(
    input: impl Into<TestActorConfig>,
) -> Result<SessionHandle, AgentLoopError> {
    SessionActor::spawn(bind(input).await?)
}

pub(crate) async fn bind(
    input: impl Into<TestActorConfig>,
) -> Result<SessionActorConfig, AgentLoopError> {
    let TestActorConfig {
        mut inner,
        recovered,
    } = input.into();
    let config = &mut inner;
    let root = Arc::new(tempfile::tempdir().map_err(failure)?);
    let mut log = SegmentedJournal::open(root.path(), &config.session_id.0).map_err(failure)?;
    let source = config.event_sink.capture_read_view()?;
    let mut seed = Vec::new();
    let mut after = None;
    let mut source_bytes = 0_usize;
    while after != source.last_sequence() {
        let page = source
            .read_page(after, SessionReplayLimits::default())
            .await?;
        if page.is_empty() {
            return Err(failure("fixture prefix has no source events"));
        }
        for event in page {
            source_bytes = source_bytes
                .checked_add(
                    rw_types::session_controls::encoded_size(&event, 16 * 1024 * 1024)
                        .map_err(failure)?,
                )
                .filter(|bytes| *bytes <= 32 * 1024 * 1024)
                .ok_or_else(|| failure("fixture seed exceeds bounded source allowance"))?;
            after = event.meta().map(|meta| meta.sequence_id);
            seed.push(event);
        }
    }
    if seed.is_empty() {
        seed = super::history_seed::events(config, &recovered)?;
        if !seed.is_empty() {
            crate::commit_session_events(Arc::clone(&config.event_sink), seed.clone()).await?;
        }
    }
    if !seed.is_empty() {
        log.append_batch(seed).map_err(failure)?;
    }
    let modes = Arc::clone(&config.modes);
    let mut index = CanonicalRecovery::open(&log.read_view(), &modes, None).map_err(failure)?;
    advance(&mut index, &log.read_view(), &modes)?;
    let authority = Arc::new(JournalFixture {
        inner: Arc::clone(&config.event_sink),
        log: Mutex::new(log),
        index: Mutex::new(index),
        modes,
        root,
    });
    config.recovered = crate::engine::SessionActorRecovery::from_bootstrap(
        authority.capture_history().await?.bootstrap().await?,
    )?;
    config.history = authority.clone();
    config.event_sink = authority;
    Ok(inner)
}

fn failure(error: impl std::fmt::Display) -> AgentLoopError {
    AgentLoopError::Persistence(error.to_string())
}
fn advance(
    index: &mut CanonicalRecovery,
    source: &JournalReadView,
    modes: &ModeRegistry,
) -> Result<(), AgentLoopError> {
    loop {
        if !index.advance(source, modes).map_err(failure)?.has_more {
            return Ok(());
        }
    }
}
struct JournalFixture {
    inner: Arc<dyn SessionEventSink>,
    log: Mutex<SegmentedJournal>,
    index: Mutex<CanonicalRecovery>,
    modes: Arc<ModeRegistry>,
    root: Arc<tempfile::TempDir>,
}
impl JournalFixture {
    fn history(&self) -> Result<CanonicalHistory, AgentLoopError> {
        let source = self.log.lock().map_err(failure)?.read_view();
        let mut index = self.index.lock().map_err(failure)?;
        advance(&mut index, &source, &self.modes)?;
        index
            .snapshot()
            .map_err(failure)?
            .bind_source(&source)
            .map_err(failure)
    }
}
#[async_trait]
impl SessionEventSink for JournalFixture {
    async fn completed_turn(
        &self,
        turn: u64,
    ) -> Result<Option<crate::engine::CompletedTurn>, AgentLoopError> {
        Ok(self
            .history()?
            .completed_boundary(turn)
            .map_err(failure)?
            .map(|boundary| crate::engine::CompletedTurn {
                sequence_id: boundary.source_sequence,
                completed_turns: boundary.control.completed_turns,
            }))
    }
    async fn todo_state(&self) -> Result<rw_types::todo::TodoSnapshot, AgentLoopError> {
        self.history()?.todo_state().map_err(failure)
    }
    async fn source_rewind_target(
        &self,
        expected_through: SequenceId,
        source: SequenceId,
        turn: u64,
        position: rw_types::RewindSourcePosition,
    ) -> Result<u64, AgentLoopError> {
        self.history()?
            .resolve_source_rewind(expected_through, source, turn, position)
            .map_err(failure)
    }
    async fn extension_state(&self, plugin_id: &str) -> Result<ExtensionStateView, AgentLoopError> {
        self.inner.extension_state(plugin_id).await
    }
    async fn reserve(
        &self,
        plan: &EventBatchPlan,
    ) -> Result<EventBatchReservation, AgentLoopError> {
        self.inner.reserve(plan).await
    }
    async fn commit(
        self: Arc<Self>,
        batch: Arc<AdmittedEventBatch>,
    ) -> Result<Arc<AdmittedEventBatch>, AgentLoopError> {
        let committed = Arc::clone(&self.inner).commit(batch).await?;
        self.log
            .lock()
            .map_err(failure)?
            .append_batch(committed.events().iter().cloned())
            .map_err(failure)?;
        Ok(committed)
    }
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        self.inner.settle_effects().await
    }
    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        self.inner.capture_read_view()
    }
    async fn budget_totals(
        &self,
        query: BudgetLedgerQuery,
    ) -> Result<BudgetLedgerTotals, AgentLoopError> {
        self.inner.budget_totals(query).await
    }
}
#[async_trait]
impl SessionHistory for JournalFixture {
    async fn capture_history(&self) -> Result<Arc<dyn SessionHistoryView>, AgentLoopError> {
        let history = self.history()?;
        let cut = history.head().conversation;
        let through = history.head().next_sequence.checked_sub(1).map(SequenceId);
        Ok(Arc::new(View {
            history: Mutex::new(history),
            cut,
            through,
            root: Arc::clone(&self.root),
        }))
    }
}
struct View {
    history: Mutex<CanonicalHistory>,
    cut: ConversationCut,
    through: Option<SequenceId>,
    root: Arc<tempfile::TempDir>,
}
#[async_trait]
impl SessionHistoryView for View {
    fn through(&self) -> Option<SequenceId> {
        self.through
    }
    fn conversation(&self) -> ConversationCut {
        self.cut
    }
    fn reserve_working_set(&self) -> Result<HistoryRead<()>, AgentLoopError> {
        Ok(HistoryRead::new((), Arc::clone(&self.root)))
    }
    async fn bootstrap(&self) -> Result<HistoryRead<RecoveryBootstrap>, AgentLoopError> {
        Ok(HistoryRead::new(
            self.history
                .lock()
                .map_err(failure)?
                .bootstrap()
                .map_err(failure)?,
            Arc::clone(&self.root),
        ))
    }
    async fn recovery_at_completed_turn(
        &self,
        turn: u64,
    ) -> Result<HistoryRead<RecoveryBootstrap>, AgentLoopError> {
        Ok(HistoryRead::new(
            self.history
                .lock()
                .map_err(failure)?
                .recovery_at_completed_turn(turn)
                .map_err(failure)?,
            Arc::clone(&self.root),
        ))
    }
    fn verify_prompt(
        &self,
        _turn: u64,
        _dump: &rw_types::PromptDump,
    ) -> Result<(), AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "fixture has no recorded provider request".into(),
        ))
    }
    async fn prompt_at_turn(
        &self,
        turn: u64,
    ) -> Result<Arc<dyn SessionHistoryView>, AgentLoopError> {
        let history = self
            .history
            .lock()
            .map_err(failure)?
            .prompt_at_turn(turn)
            .map_err(failure)?;
        let cut = history.head().conversation;
        let through = history.head().next_sequence.checked_sub(1).map(SequenceId);
        Ok(Arc::new(Self {
            history: Mutex::new(history),
            cut,
            through,
            root: Arc::clone(&self.root),
        }))
    }
    async fn conversation_page(
        &self,
        range: Range<u64>,
        limits: HistoryMaterializationLimits,
    ) -> Result<HistoryRead<ConversationPage>, AgentLoopError> {
        Ok(HistoryRead::new(
            self.history
                .lock()
                .map_err(failure)?
                .conversation_page(range, limits)
                .map_err(failure)?,
            Arc::clone(&self.root),
        ))
    }
}
