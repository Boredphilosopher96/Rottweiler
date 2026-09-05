use super::accounting_projection::compact_title;
use super::accounting_projection::inherited_accounting_through;
use super::accounting_projection::is_session_projection_boundary;
use super::accounting_projection::project_accounting;
use super::accounting_projection::session_projection_updated_at;
use super::accounting_projection::upsert_session_projection;
use super::append_tool_output;
use super::prompt_shapes::PromptShapeJournal;
use super::restore_todo_state;
use crate::journal_reads::JournalReadLease;
use crate::journal_reads::JournalReads;
use crate::journal_reads::JournalRegistration;
use async_trait::async_trait;
use miette::Result;
use miette::miette;
use rw_core::AgentLoopError;
use rw_core::BudgetLedgerQuery;
use rw_core::BudgetLedgerTotals;
use rw_core::EngineEvent;
use rw_core::SequenceId;
use rw_core::SessionEventReadView;
use rw_core::SessionEventSink;
use rw_core::SessionReplayLimits;
use rw_core::project_session_events;
use rw_store::session::AccountingLedger;
use rw_store::session::SessionEventLog;
use rw_store::session::SessionEventPageLimits;
use rw_store::session::SessionProjection;
use rw_store::session::SessionSummary;
use rw_store::session::UtcTimestamp;
use rw_tools::TodoTool;
use rw_types::SessionId;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

#[derive(Debug)]
pub(super) struct DurableReadView {
    pub(super) lease: JournalReadLease,
    pub(super) session_id: String,
}

#[async_trait]
impl SessionEventReadView for DurableReadView {
    fn last_sequence(&self) -> Option<SequenceId> {
        self.lease.view.last_sequence()
    }

    async fn read_page(
        &self,
        after: Option<SequenceId>,
        limits: SessionReplayLimits,
    ) -> std::result::Result<Vec<EngineEvent>, AgentLoopError> {
        let lease = self.lease.clone();
        let session = self.session_id.clone();
        tokio::task::spawn_blocking(move || {
            let page = lease
                .view
                .page::<EngineEvent>(
                    after,
                    SessionEventPageLimits {
                        max_page_events: limits.max_events,
                        max_page_bytes: limits.max_bytes as u64,
                        ..SessionEventPageLimits::default()
                    },
                )
                .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
            page.events
                .into_iter()
                .map(|envelope| {
                    let meta = envelope.event.meta().ok_or_else(|| {
                        AgentLoopError::Persistence("transient event in durable journal".to_owned())
                    })?;
                    if meta.session_id.0 != session || meta.sequence_id != envelope.sequence {
                        return Err(AgentLoopError::Persistence(
                            "durable event identity differs from its envelope".to_owned(),
                        ));
                    }
                    Ok(envelope.event)
                })
                .collect()
        })
        .await
        .map_err(|error| AgentLoopError::Persistence(format!("journal reader failed: {error}")))?
    }
}

pub(super) struct DurableEventSink {
    pub(super) journal_reads: Arc<JournalReads>,
    pub(super) _registration: JournalRegistration,
    pub(super) log: Arc<Mutex<SessionEventLog>>,
    pub(super) storage_root: PathBuf,
    pub(super) session_id: String,
    pub(super) hosted_projection: Option<Mutex<HostedSessionProjection>>,
    pub(super) prompt_shapes: Arc<PromptShapeJournal>,
    pub(super) accounting_dirty: AtomicBool,
    pub(super) todo_restore: Mutex<Option<TodoRestoreBinding>>,
}

pub(super) struct HostedSessionProjection {
    pub(super) projection: SessionProjection,
    pub(super) explicit_title: bool,
    pub(super) saw_user_message: bool,
}

impl HostedSessionProjection {
    pub(super) fn from_events(session_id: &str, events: &[EngineEvent], path: &Path) -> Self {
        let mut hosted = Self {
            projection: SessionProjection {
                summary: SessionSummary {
                    id: session_id.to_owned(),
                    title: "New session".to_owned(),
                    updated_unix_ms: session_projection_updated_at(path),
                    cost_micros: 0,
                    turn_count: 0,
                },
                transcript: String::new(),
                projected_through: None,
            },
            explicit_title: false,
            saw_user_message: false,
        };
        hosted.apply(events, path);
        hosted
    }

    pub(super) fn apply(&mut self, events: &[EngineEvent], path: &Path) {
        for event in events {
            match event {
                EngineEvent::SessionTitleUpdated { title, .. } => {
                    self.projection.summary.title.clone_from(title);
                    self.explicit_title = true;
                }
                EngineEvent::UserMessageAccepted { content, .. } => {
                    if !self.saw_user_message && !self.explicit_title {
                        self.projection.summary.title = compact_title(content);
                    }
                    self.saw_user_message = true;
                    self.projection.summary.turn_count =
                        self.projection.summary.turn_count.saturating_add(1);
                    self.projection.transcript.push_str("user: ");
                    self.projection.transcript.push_str(content);
                    self.projection.transcript.push('\n');
                }
                EngineEvent::TextDelta { text, .. } => {
                    self.projection.transcript.push_str(text);
                }
                EngineEvent::ToolCallFinished { output, .. } => {
                    self.projection.transcript.push_str("\ntool: ");
                    append_tool_output(&mut self.projection.transcript, output);
                    self.projection.transcript.push('\n');
                }
                _ => {}
            }
            self.projection.projected_through = event.meta().map(|meta| meta.sequence_id);
        }
        self.projection.summary.updated_unix_ms = session_projection_updated_at(path);
    }
}

impl DurableEventSink {
    pub(super) fn new(
        log: SessionEventLog,
        storage_root: PathBuf,
        session_id: String,
        journal_reads: Arc<JournalReads>,
    ) -> Result<Self> {
        Self::new_with_hosted_projection(log, storage_root, session_id, None, journal_reads)
    }

    pub(super) fn new_hosted(
        log: SessionEventLog,
        storage_root: PathBuf,
        session_id: String,
        recovered_events: &[EngineEvent],
        journal_reads: Arc<JournalReads>,
    ) -> Result<Self> {
        let projection =
            HostedSessionProjection::from_events(&session_id, recovered_events, log.path());
        Self::new_with_hosted_projection(
            log,
            storage_root,
            session_id,
            Some(projection),
            journal_reads,
        )
    }

    pub(super) fn new_with_hosted_projection(
        log: SessionEventLog,
        storage_root: PathBuf,
        session_id: String,
        hosted_projection: Option<HostedSessionProjection>,
        journal_reads: Arc<JournalReads>,
    ) -> Result<Self> {
        let log = Arc::new(Mutex::new(log));
        let registration = journal_reads.register(
            &session_id,
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_view(),
        )?;
        let prompt_shapes = Arc::new(PromptShapeJournal::open(&storage_root, &session_id)?);
        Ok(Self {
            journal_reads,
            _registration: registration,
            log,
            storage_root,
            session_id,
            hosted_projection: hosted_projection.map(Mutex::new),
            prompt_shapes,
            accounting_dirty: AtomicBool::new(false),
            todo_restore: Mutex::new(None),
        })
    }

    pub(super) async fn update_hosted_projection(&self, persisted: &[EngineEvent]) {
        let projection = self.hosted_projection.as_ref().and_then(|hosted| {
            let path = self
                .storage_root
                .join("sessions")
                .join(&self.session_id)
                .join("journal");
            let mut hosted = hosted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            hosted.apply(persisted, &path);
            persisted
                .iter()
                .any(is_session_projection_boundary)
                .then(|| hosted.projection.clone())
        });
        let Some(projection) = projection else {
            return;
        };
        let storage_root = self.storage_root.clone();
        let update = move || upsert_session_projection(&storage_root, &projection);
        let update_result = match tokio::runtime::Handle::current().runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(update),
            _ => match tokio::task::spawn_blocking(update).await {
                Ok(result) => result,
                Err(error) => Err(miette!(error.to_string())),
            },
        };
        if let Err(error) = update_result {
            tracing::warn!(
                session_id = %self.session_id,
                reason = %error,
                "hosted session search projection will retry at the next durable boundary"
            );
        }
    }

    pub(super) fn bind_todo(&self, binding: TodoRestoreBinding) {
        *self
            .todo_restore
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(binding);
    }

    pub(super) fn load(&self) -> Result<Vec<EngineEvent>> {
        let log = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        load_session_events(&log)
    }
}

#[async_trait]
impl SessionEventSink for DurableEventSink {
    async fn append(&self, event: EngineEvent) -> std::result::Result<EngineEvent, AgentLoopError> {
        self.append_batch(vec![event])
            .await?
            .pop()
            .ok_or_else(|| AgentLoopError::Persistence("event batch returned empty".to_owned()))
    }

    async fn append_batch(
        &self,
        events: Vec<EngineEvent>,
    ) -> std::result::Result<Vec<EngineEvent>, AgentLoopError> {
        let rewound = events
            .iter()
            .any(|event| matches!(event, EngineEvent::ConversationRewound { .. }));
        let log = Arc::clone(&self.log);
        let publisher = Arc::clone(&self._registration.publisher);
        let append = move || {
            let mut log = log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (offset, event) in events.iter().enumerate() {
                let offset = u64::try_from(offset).map_err(|_| {
                    AgentLoopError::Persistence("event batch length overflow".to_owned())
                })?;
                let expected = log.next_sequence().checked_add(offset).ok_or_else(|| {
                    AgentLoopError::Persistence("event sequence overflow".to_owned())
                })?;
                let meta = event.meta().ok_or_else(|| {
                    AgentLoopError::Persistence(
                        "connection acknowledgement cannot be persisted".to_owned(),
                    )
                })?;
                if meta.sequence_id.0 != expected {
                    return Err(AgentLoopError::Persistence(format!(
                        "event sequence {} does not match log sequence {expected}",
                        meta.sequence_id.0
                    )));
                }
            }
            let envelopes = log
                .append_batch(events)
                .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
            publisher.publish(log.read_view());
            Ok(envelopes)
        };
        let envelopes = match tokio::runtime::Handle::current().runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(append),
            _ => tokio::task::spawn_blocking(append)
                .await
                .map_err(|error| AgentLoopError::Persistence(error.to_string()))?,
        }?;
        if rewound {
            let binding = self
                .todo_restore
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(binding) = binding {
                let events = self
                    .load()
                    .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
                let recovered = project_session_events(&events)
                    .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
                restore_todo_state(
                    &recovered.conversation,
                    &binding.workspace,
                    &binding.session_id,
                    &binding.todo,
                )
                .await
                .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
            }
        }
        let persisted = envelopes
            .into_iter()
            .map(|envelope| envelope.event)
            .collect::<Vec<_>>();
        for event in &persisted {
            match event {
                EngineEvent::TurnStarted { turn_id, .. } => {
                    self.prompt_shapes.set_active_turn(turn_id.clone());
                }
                EngineEvent::TurnFinished { turn_id, .. } => {
                    self.prompt_shapes.clear_active_turn(turn_id);
                }
                _ => {}
            }
        }
        self.update_hosted_projection(&persisted).await;
        if let Err(error) = self.reconcile_accounting(&persisted) {
            self.accounting_dirty.store(true, Ordering::Release);
            tracing::warn!(
                session_id = %self.session_id,
                reason = %error,
                "durable accounting projection will be repaired on the next query"
            );
        }
        Ok(persisted)
    }

    fn capture_read_view(
        &self,
    ) -> std::result::Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        let lease = self
            .journal_reads
            .capture(&self.session_id)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        Ok(Arc::new(DurableReadView {
            lease,
            session_id: self.session_id.clone(),
        }))
    }

    async fn last_sequence(&self) -> std::result::Result<Option<SequenceId>, AgentLoopError> {
        Ok(self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_sequence())
    }

    async fn budget_totals(
        &self,
        query: BudgetLedgerQuery,
    ) -> std::result::Result<BudgetLedgerTotals, AgentLoopError> {
        if self.accounting_dirty.swap(false, Ordering::AcqRel) {
            let repair = self
                .load()
                .and_then(|events| self.reconcile_accounting(&events));
            if let Err(error) = repair {
                self.accounting_dirty.store(true, Ordering::Release);
                return Err(AgentLoopError::Persistence(error.to_string()));
            }
        }
        let now = UtcTimestamp::from_unix_millis(query.now_unix_ms)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        let day_start = UtcTimestamp::from_unix_millis(query.utc_day_start_unix_ms)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        let trailing_start = UtcTimestamp::from_unix_millis(query.trailing_minute_start_unix_ms)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        let totals = AccountingLedger::open(&self.storage_root)
            .and_then(|ledger| {
                ledger.totals(
                    &self.session_id,
                    &day_start.utc_day(),
                    &trailing_start,
                    &now,
                )
            })
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        Ok(BudgetLedgerTotals {
            authoritative: true,
            session_cost_micros_usd: totals.session_micros_usd,
            session_ai_credit_micros: totals.session_ai_credit_micros,
            daily_cost_micros_usd: totals.day_micros_usd,
            daily_ai_credit_micros: totals.day_ai_credit_micros,
            trailing_minute_cost_micros_usd: totals.trailing_all_sessions_micros_usd,
            trailing_minute_ai_credit_micros: totals.trailing_all_sessions_ai_credit_micros,
            session_subscription_tokens: totals.session_subscription_tokens,
            daily_subscription_tokens: totals.day_subscription_tokens,
            trailing_minute_subscription_tokens: totals.trailing_all_sessions_subscription_tokens,
            session_subscription_quota_entries: totals.session_subscription_quota_turns,
            session_cost_unavailable_entries: totals.session_unavailable_turns,
            session_non_usd_monetary_entries: totals.session_non_usd_monetary_turns,
            daily_subscription_quota_entries: totals.day_subscription_quota_turns,
            session_unmetered_subscription_quota_entries: totals
                .session_unmetered_subscription_quota_turns,
            daily_unmetered_subscription_quota_entries: totals
                .day_unmetered_subscription_quota_turns,
            daily_cost_unavailable_entries: totals.day_unavailable_turns,
            daily_non_usd_monetary_entries: totals.day_non_usd_monetary_turns,
        })
    }
}

impl DurableEventSink {
    pub(super) fn reconcile_accounting(&self, events: &[EngineEvent]) -> Result<()> {
        let inherited_through = inherited_accounting_through(&self.storage_root, &self.session_id)?;
        let entries = project_accounting(&self.session_id, events, inherited_through)?;
        if entries.is_empty() {
            return Ok(());
        }
        AccountingLedger::open(&self.storage_root)
            .and_then(|ledger| ledger.reconcile(&entries))
            .map_err(|error| miette!("session accounting could not reconcile: {error}"))
    }
}

#[derive(Clone)]
pub(super) struct TodoRestoreBinding {
    pub(super) todo: Arc<TodoTool>,
    pub(super) workspace: PathBuf,
    pub(super) session_id: SessionId,
}

pub(super) fn load_session_events(log: &SessionEventLog) -> Result<Vec<EngineEvent>> {
    let envelopes = log
        .load::<EngineEvent>()
        .map_err(|error| miette!("session events could not load: {error}"))?;
    validate_session_event_envelopes(envelopes)
}

pub(super) fn validate_session_event_envelopes(
    envelopes: Vec<rw_store::session::EventEnvelope<EngineEvent>>,
) -> Result<Vec<EngineEvent>> {
    envelopes
        .into_iter()
        .map(|envelope| {
            let meta = envelope
                .event
                .meta()
                .ok_or_else(|| miette!("persisted command acknowledgement is invalid"))?;
            if meta.sequence_id != envelope.sequence {
                return Err(miette!(
                    "persisted event sequence {} does not match storage envelope {}",
                    meta.sequence_id.0,
                    envelope.sequence.0
                ));
            }
            Ok(envelope.event)
        })
        .collect()
}
