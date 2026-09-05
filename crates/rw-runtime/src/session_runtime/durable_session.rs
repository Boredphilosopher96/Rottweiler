mod accounting;
mod canonical;
mod child_lifecycle;
pub(super) use child_lifecycle::ChildLifecycleReader;
mod provider_recovery;
mod reads;
use super::accounting_projection::is_session_projection_boundary;
use super::prompt_shapes::PromptShapeJournal;
use crate::journal_service::JournalReadLease;
use crate::journal_service::JournalRegistration;
use crate::journal_service::JournalService;
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
use rw_core::{AdmittedEventBatch, EventBatchPlan, EventBatchReservation};
use rw_store::session::AccountingLedger;
use rw_store::session::SessionEventLog;
use rw_store::session::SessionEventPageLimits;
use rw_store::session::UtcTimestamp;
#[cfg(test)]
use rw_types::ToolOutput;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tracing::Instrument;

#[derive(Debug)]
pub(super) struct DurableReadView {
    pub(super) lease: JournalReadLease,
    reads: Arc<reads::ReadOperations>,
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
        self.reads
            .run(lease, move |lease| {
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
                            AgentLoopError::Persistence(
                                "transient event in durable journal".to_owned(),
                            )
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
    }
}

pub(super) struct DurableEventSink {
    pub(super) journal_service: Arc<JournalService>,
    pub(super) registration: JournalRegistration,
    pub(super) log: Arc<Mutex<SessionEventLog>>,
    commit_order: Arc<tokio::sync::Mutex<()>>,
    reads: Arc<reads::ReadOperations>,
    canonical: std::sync::OnceLock<Arc<canonical::CanonicalSession>>,
    pub(super) storage_root: PathBuf,
    pub(super) session_id: String,
    search_update: Mutex<()>,
    pub(super) prompt_shapes: Arc<PromptShapeJournal>,
    pub(super) accounting_dirty: Arc<AtomicBool>,
    accounting_progress:
        Arc<tokio::sync::Mutex<Option<rw_store::session::journal::JournalPrefixIdentity>>>,
}

impl DurableEventSink {
    pub(super) fn new(
        log: SessionEventLog,
        storage_root: PathBuf,
        session_id: String,
        journal_service: Arc<JournalService>,
    ) -> Result<Arc<Self>> {
        let log = Arc::new(Mutex::new(log));
        let registration = journal_service.register(
            &session_id,
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_view(),
        )?;
        let prompt_shapes = Arc::new(PromptShapeJournal::open(&storage_root, &session_id)?);
        let sink = Arc::new(Self {
            journal_service,
            registration,
            log,
            commit_order: Arc::new(tokio::sync::Mutex::new(())),
            canonical: std::sync::OnceLock::new(),
            reads: reads::ReadOperations::new(),
            storage_root,
            session_id,
            search_update: Mutex::new(()),
            prompt_shapes,
            accounting_dirty: Arc::new(AtomicBool::new(false)),
            accounting_progress: Arc::new(tokio::sync::Mutex::new(None)),
        });
        sink.synchronize_search()?;
        Ok(sink)
    }

    pub(super) fn synchronize_search(&self) -> Result<()> {
        let _update = self
            .search_update
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lease = self
            .journal_service
            .admit_read()?
            .capture(&self.session_id)?;
        super::search_projection::synchronize(&self.storage_root, &self.session_id, &lease.view)
    }

    fn update_search(&self, persisted: &[EngineEvent]) {
        if !persisted.iter().any(is_session_projection_boundary) {
            return;
        }
        if let Err(error) = self.synchronize_search() {
            tracing::warn!(session_id = %self.session_id, reason = %error,
                "session search projection will retry from its persisted source cursor");
        }
    }
}

#[async_trait]
impl SessionEventSink for DurableEventSink {
    async fn completed_turn(
        &self,
        turn: u64,
    ) -> std::result::Result<Option<rw_core::CompletedTurn>, AgentLoopError> {
        self.read_canonical(move |history| {
            history.completed_boundary(turn).map(|boundary| {
                boundary.map(|boundary| rw_core::CompletedTurn {
                    sequence_id: boundary.source_sequence,
                    completed_turns: boundary.control.completed_turns,
                })
            })
        })
        .await
    }

    async fn todo_state(
        &self,
    ) -> std::result::Result<rw_types::todo::TodoSnapshot, AgentLoopError> {
        self.read_canonical(rw_core::recovery::CanonicalHistory::todo_state)
            .await
    }
    async fn source_rewind_target(
        &self,
        expected_through: rw_types::SequenceId,
        source: rw_types::SequenceId,
        turn: u64,
        position: rw_types::RewindSourcePosition,
    ) -> std::result::Result<u64, AgentLoopError> {
        self.read_canonical(move |history| {
            history.resolve_source_rewind(expected_through, source, turn, position)
        })
        .await
    }

    async fn extension_state(
        &self,
        plugin_id: &str,
    ) -> std::result::Result<rw_core::ExtensionStateView, AgentLoopError> {
        self.read_extension_state(plugin_id).await
    }
    async fn settle_effects(&self) -> std::result::Result<(), AgentLoopError> {
        let settled = self
            .journal_service
            .commits
            .enter(Arc::clone(&self.commit_order))
            .await?;
        drop(settled);
        self.reads.settle().await?;
        Ok(())
    }
    async fn reserve(
        &self,
        plan: &EventBatchPlan,
    ) -> std::result::Result<EventBatchReservation, AgentLoopError> {
        self.journal_service.commits.reserve(plan)
    }
    async fn commit(
        self: Arc<Self>,
        batch: Arc<AdmittedEventBatch>,
    ) -> std::result::Result<Arc<AdmittedEventBatch>, AgentLoopError> {
        let queue = Arc::clone(&self.journal_service.commits);
        let order = queue.enter(Arc::clone(&self.commit_order)).instrument(tracing::trace_span!(target: "rw_performance", "journal.order_wait", session_id = %self.session_id)).await?;
        let owner = Arc::clone(&self);
        let submitted = Arc::clone(&batch);
        queue
            .execute(
                owner,
                batch,
                order,
                async move { self.persist(submitted).await },
            )
            .await
    }

    fn capture_read_view(
        &self,
    ) -> std::result::Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        let lease = self
            .journal_service
            .capture(&self.session_id)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        Ok(Arc::new(DurableReadView {
            reads: Arc::clone(&self.reads),
            lease,
            session_id: self.session_id.clone(),
        }))
    }

    async fn last_sequence(&self) -> std::result::Result<Option<SequenceId>, AgentLoopError> {
        Ok(self.registration.publisher.last_sequence())
    }

    async fn budget_totals(
        &self,
        query: BudgetLedgerQuery,
    ) -> std::result::Result<BudgetLedgerTotals, AgentLoopError> {
        if self.accounting_dirty.load(Ordering::Acquire) {
            let repair = self.reconcile_indexed_accounting().await;
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
        let root = self.storage_root.clone();
        let session = self.session_id.clone();
        let admission = self
            .journal_service
            .admit_read()
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        let totals = self
            .reads
            .run(admission, move |_| {
                AccountingLedger::open(&root)
                    .and_then(|ledger| {
                        ledger.totals(&session, &day_start.utc_day(), &trailing_start, &now)
                    })
                    .map_err(|error| AgentLoopError::Persistence(error.to_string()))
            })
            .await?;
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

impl DurableEventSink {
    fn append_and_publish(
        &self,
        events: &[EngineEvent],
    ) -> std::result::Result<(), AgentLoopError> {
        let mut log = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (offset, event) in events.iter().enumerate() {
            let offset = u64::try_from(offset).map_err(|_| {
                AgentLoopError::Persistence("event batch length overflow".to_owned())
            })?;
            let expected = log
                .next_sequence()
                .checked_add(offset)
                .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?;
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
        let first = events
            .first()
            .and_then(EngineEvent::meta)
            .ok_or_else(|| AgentLoopError::Persistence("empty journal batch".to_owned()))?
            .sequence_id;
        let prepared = rw_store::session::journal::JournalAppendPlan::measure(first, events)
            .and_then(|plan| plan.encode(events))
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        log.append_prepared(prepared)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        self.registration.publisher.publish(log.read_view());
        Ok(())
    }

    async fn persist(
        self: Arc<Self>,
        batch: Arc<AdmittedEventBatch>,
    ) -> std::result::Result<Arc<AdmittedEventBatch>, AgentLoopError> {
        let owner = Arc::clone(&self);
        let submitted = Arc::clone(&batch);
        tokio::task::spawn_blocking(move || owner.append_and_publish(submitted.events()))
            .await
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))??;
        let batch = tokio::task::spawn_blocking(move || {
            let persisted = batch.events();
            for event in persisted {
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
            self.update_search(persisted);
            if let Err(error) = self.reconcile_committed_accounting(persisted) {
                self.accounting_dirty.store(true, Ordering::Release);
                tracing::warn!(
                    session_id = %self.session_id,
                    reason = %error,
                    "durable accounting projection will be repaired on the next query"
                );
            }
            batch
        })
        .await
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;

        Ok(batch)
    }
}

#[cfg(test)]
mod commit_tests;

#[cfg(test)]
pub(super) fn append_tool_output(target: &mut String, output: &ToolOutput) {
    match output {
        ToolOutput::Text { text } => target.push_str(text),
        ToolOutput::Structured { value } => target.push_str(&value.to_string()),
        ToolOutput::Mixed { parts } => {
            let _ = std::fmt::Write::write_fmt(target, format_args!("{parts:?}"));
        }
    }
}
