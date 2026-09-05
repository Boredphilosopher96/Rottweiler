#![cfg(test)]

use crate::engine::AgentLoopError;
use crate::engine::durability::NoopSessionEventSink;
use crate::engine::durability::SessionEventSink;
use crate::engine::event_clock::BudgetLedgerQuery;
use crate::engine::event_clock::BudgetLedgerTotals;
use crate::engine::pending_event::PendingEvent;
use crate::engine::replay;
use crate::engine::replay::SessionEventReadView;
use crate::engine::replay::SessionReplayLimits;
use crate::engine::tests;
use crate::engine::tests::fixtures::support::SessionEvent;
use crate::engine::tests::fixtures::support::observe_event;
use crate::engine::turn;
use async_trait::async_trait;
use futures_util::StreamExt;
use rw_types::Cost;
use rw_types::EngineError;
use rw_types::EngineErrorCategory;
use rw_types::EngineEvent;
use rw_types::SequenceId;
use rw_types::SubscriptionTokenAccounting;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use thiserror::Error;
use tokio::sync::Notify;

#[derive(Default)]
pub(in crate::engine::tests) struct RecordingSink {
    pub(in crate::engine::tests) events: Mutex<Vec<SessionEvent>>,
    pub(in crate::engine::tests) batch_sizes: Mutex<Vec<usize>>,
    pub(in crate::engine::tests) tail_floor: Mutex<Option<SequenceId>>,
}

#[async_trait]
impl SessionEventSink for RecordingSink {
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        self.batch_sizes.lock().expect("batch sizes").push(1);
        self.events
            .lock()
            .expect("event sink lock")
            .push(observe_event(event.clone()).expect("durable fixture event"));
        Ok(event)
    }

    async fn append_batch(
        &self,
        events: Vec<EngineEvent>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        self.batch_sizes
            .lock()
            .expect("batch sizes")
            .push(events.len());
        self.events.lock().expect("event sink lock").extend(
            events
                .iter()
                .cloned()
                .map(|event| observe_event(event).expect("durable fixture event")),
        );
        Ok(events)
    }

    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        let events: Vec<EngineEvent> = {
            Ok(self
                .events
                .lock()
                .expect("event sink lock")
                .iter()
                .map(|event| event.wire.clone())
                .collect())
        }?;
        let actual = events
            .last()
            .and_then(EngineEvent::meta)
            .map(|meta| meta.sequence_id);
        let floor = *self.tail_floor.lock().expect("tail floor");
        let tail = match (floor, actual) {
            (Some(floor), Some(actual)) => Some(floor.max(actual)),
            (floor, actual) => floor.or(actual),
        };
        Ok(Arc::new(replay::MemoryEventReadView::new(
            Arc::new(Mutex::new(events)),
            tail,
        )))
    }

    async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        let floor = *self.tail_floor.lock().expect("tail floor");
        let actual = self
            .events
            .lock()
            .expect("event sink lock")
            .last()
            .map(|event| event.sequence);
        Ok(match (floor, actual) {
            (Some(floor), Some(actual)) => Some(floor.max(actual)),
            (floor, actual) => floor.or(actual),
        })
    }
}

#[derive(Default)]
pub(in crate::engine::tests) struct AccountingRecordingSink {
    pub(in crate::engine::tests) inner: RecordingSink,
}

#[async_trait]
impl SessionEventSink for AccountingRecordingSink {
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        self.inner.append(event).await
    }

    async fn append_batch(
        &self,
        events: Vec<EngineEvent>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        self.inner.append_batch(events).await
    }

    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        self.inner.capture_read_view()
    }

    async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        self.inner.last_sequence().await
    }

    async fn budget_totals(
        &self,
        _query: BudgetLedgerQuery,
    ) -> Result<BudgetLedgerTotals, AgentLoopError> {
        let mut totals = BudgetLedgerTotals {
            authoritative: true,
            ..BudgetLedgerTotals::default()
        };
        for event in self.inner.events.lock().expect("event sink lock").iter() {
            let cost = match &event.kind {
                PendingEvent::TurnFinished { cost, .. }
                | PendingEvent::CompactionAttemptFinished { cost, .. }
                | PendingEvent::CompactionFinished {
                    cost: Some(cost), ..
                } => Some(cost),
                _ => None,
            };
            match cost {
                Some(Cost::Monetary {
                    amount_micros,
                    currency,
                }) if currency.eq_ignore_ascii_case("USD") => {
                    totals.session_cost_micros_usd = totals
                        .session_cost_micros_usd
                        .saturating_add(*amount_micros);
                    totals.daily_cost_micros_usd =
                        totals.daily_cost_micros_usd.saturating_add(*amount_micros);
                }
                Some(Cost::Monetary { .. }) => {
                    totals.session_non_usd_monetary_entries =
                        totals.session_non_usd_monetary_entries.saturating_add(1);
                    totals.daily_non_usd_monetary_entries =
                        totals.daily_non_usd_monetary_entries.saturating_add(1);
                }
                Some(cost @ Cost::SubscriptionQuota { .. }) => {
                    totals.session_subscription_quota_entries =
                        totals.session_subscription_quota_entries.saturating_add(1);
                    totals.daily_subscription_quota_entries =
                        totals.daily_subscription_quota_entries.saturating_add(1);
                    match cost.subscription_token_accounting() {
                        SubscriptionTokenAccounting::Metered(tokens) => {
                            totals.session_subscription_tokens =
                                totals.session_subscription_tokens.saturating_add(tokens);
                            totals.daily_subscription_tokens =
                                totals.daily_subscription_tokens.saturating_add(tokens);
                            totals.trailing_minute_subscription_tokens = totals
                                .trailing_minute_subscription_tokens
                                .saturating_add(tokens);
                        }
                        SubscriptionTokenAccounting::Unavailable => {
                            totals.session_unmetered_subscription_quota_entries = totals
                                .session_unmetered_subscription_quota_entries
                                .saturating_add(1);
                            totals.daily_unmetered_subscription_quota_entries = totals
                                .daily_unmetered_subscription_quota_entries
                                .saturating_add(1);
                        }
                        SubscriptionTokenAccounting::NotApplicable => {}
                    }
                }
                Some(Cost::Unavailable { .. }) => {
                    totals.session_cost_unavailable_entries =
                        totals.session_cost_unavailable_entries.saturating_add(1);
                    totals.daily_cost_unavailable_entries =
                        totals.daily_cost_unavailable_entries.saturating_add(1);
                }
                Some(Cost::AiCredits { .. }) | None => {}
            }
        }
        Ok(totals)
    }
}

pub(in crate::engine::tests) struct FailingSink;

#[async_trait]
impl SessionEventSink for FailingSink {
    async fn append(&self, _event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        Err(AgentLoopError::Persistence("fixture failure".to_owned()))
    }

    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        Err(AgentLoopError::Persistence("fixture failure".to_owned()))
    }
}

#[derive(Default)]
pub(in crate::engine::tests) struct FailCompactionLedgerSink {
    pub(in crate::engine::tests) inner: RecordingSink,
}

#[async_trait]
impl SessionEventSink for FailCompactionLedgerSink {
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        self.inner.append(event).await
    }

    async fn append_batch(
        &self,
        events: Vec<EngineEvent>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        self.inner.append_batch(events).await
    }

    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        self.inner.capture_read_view()
    }

    async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        self.inner.last_sequence().await
    }

    async fn budget_totals(
        &self,
        _query: BudgetLedgerQuery,
    ) -> Result<BudgetLedgerTotals, AgentLoopError> {
        if self
            .inner
            .events
            .lock()
            .expect("event sink lock")
            .iter()
            .any(|event| {
                matches!(
                    &event.kind,
                    PendingEvent::ConversationTurnCommitted { turn, .. } if turn.meta.summary
                )
            })
        {
            return Err(AgentLoopError::Persistence(
                "compaction ledger fixture failure".to_owned(),
            ));
        }
        Ok(BudgetLedgerTotals::default())
    }
}

#[derive(Default)]
pub(in crate::engine::tests) struct FailNextBatchSink {
    pub(in crate::engine::tests) inner: RecordingSink,
    pub(in crate::engine::tests) fail_next: AtomicBool,
}

#[async_trait]
impl SessionEventSink for FailNextBatchSink {
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        self.append_batch(vec![event])
            .await?
            .pop()
            .ok_or_else(|| AgentLoopError::Persistence("empty fixture batch".to_owned()))
    }

    async fn append_batch(
        &self,
        events: Vec<EngineEvent>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        if self.fail_next.swap(false, Ordering::AcqRel) {
            return Err(AgentLoopError::Persistence(
                "transient fixture failure".to_owned(),
            ));
        }
        self.inner.append_batch(events).await
    }

    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        self.inner.capture_read_view()
    }

    async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        self.inner.last_sequence().await
    }
}

#[derive(Default)]
pub(in crate::engine::tests) struct FailFirstTextDeltaSink {
    pub(in crate::engine::tests) inner: RecordingSink,
    pub(in crate::engine::tests) failed: AtomicBool,
}

#[async_trait]
impl SessionEventSink for FailFirstTextDeltaSink {
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        self.append_batch(vec![event])
            .await?
            .pop()
            .ok_or_else(|| AgentLoopError::Persistence("empty fixture batch".to_owned()))
    }

    async fn append_batch(
        &self,
        events: Vec<EngineEvent>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        if !self.failed.load(Ordering::Acquire)
            && events
                .iter()
                .any(|event| matches!(event, EngineEvent::TextDelta { .. }))
        {
            self.failed.store(true, Ordering::Release);
            return Err(AgentLoopError::Persistence(
                "transient text-delta fixture failure".to_owned(),
            ));
        }
        self.inner.append_batch(events).await
    }

    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        self.inner.capture_read_view()
    }

    async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        self.inner.last_sequence().await
    }
}

#[derive(Default)]
pub(in crate::engine::tests) struct WorkspaceChangeFailingSink {
    pub(in crate::engine::tests) inner: RecordingSink,
}

#[async_trait]
impl SessionEventSink for WorkspaceChangeFailingSink {
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        if matches!(&event, EngineEvent::WorkspaceRootsChanged { .. }) {
            return Err(AgentLoopError::Persistence(
                "workspace change fixture failure".to_owned(),
            ));
        }
        self.inner.append(event).await
    }

    async fn append_batch(
        &self,
        events: Vec<EngineEvent>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        if events
            .iter()
            .any(|event| matches!(event, EngineEvent::WorkspaceRootsChanged { .. }))
        {
            return Err(AgentLoopError::Persistence(
                "workspace change fixture failure".to_owned(),
            ));
        }
        self.inner.append_batch(events).await
    }

    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        self.inner.capture_read_view()
    }

    async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        self.inner.last_sequence().await
    }
}

#[derive(Clone, Copy)]
pub(in crate::engine::tests) enum MalformedBatchMode {
    Payload,
    Sequence,
}

pub(in crate::engine::tests) struct MalformedBatchSink {
    pub(in crate::engine::tests) mode: MalformedBatchMode,
    pub(in crate::engine::tests) inner: NoopSessionEventSink,
}

#[async_trait]
impl SessionEventSink for MalformedBatchSink {
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        self.inner.append(event).await
    }

    async fn append_batch(
        &self,
        mut events: Vec<EngineEvent>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        if events.len() == 1 {
            return self.inner.append_batch(events).await;
        }
        match self.mode {
            MalformedBatchMode::Payload => {
                if let Some(event) = events.get_mut(1) {
                    let meta = event.meta().expect("durable event").clone();
                    *event = EngineEvent::Error {
                        meta,
                        error: EngineError {
                            category: EngineErrorCategory::Internal,
                            code: "substituted".to_owned(),
                            message: "substituted".to_owned(),
                            retryable: false,
                            details: None,
                        },
                    };
                }
            }
            MalformedBatchMode::Sequence => {
                if let Some(event) = events.get_mut(1) {
                    event.meta_mut().expect("durable event").sequence_id = 9.into();
                }
            }
        }
        Ok(events)
    }

    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        self.inner.capture_read_view()
    }
}

pub(in crate::engine::tests) struct BlockingBatchSink {
    pub(in crate::engine::tests) persisted: Mutex<Vec<EngineEvent>>,
    pub(in crate::engine::tests) blocked_once: AtomicBool,
    pub(in crate::engine::tests) entered: Notify,
    pub(in crate::engine::tests) release: Notify,
}

impl BlockingBatchSink {
    pub(in crate::engine::tests) fn persist(&self, events: Vec<EngineEvent>) -> Vec<EngineEvent> {
        self.persisted
            .lock()
            .expect("persisted events")
            .extend(events.iter().cloned());
        events
    }
}

#[async_trait]
impl SessionEventSink for BlockingBatchSink {
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        self.persist(vec![event])
            .pop()
            .ok_or_else(|| AgentLoopError::Persistence("fixture append failed".to_owned()))
    }

    async fn append_batch(
        &self,
        events: Vec<EngineEvent>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        if events.len() > 1 && !self.blocked_once.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(self.persist(events))
    }

    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        let events: Vec<EngineEvent> = {
            Ok(self
                .persisted
                .lock()
                .expect("persisted events")
                .iter()
                .filter(|event| event.meta().is_some())
                .cloned()
                .collect())
        }?;
        let tail = events
            .last()
            .and_then(EngineEvent::meta)
            .map(|meta| meta.sequence_id);
        Ok(Arc::new(replay::MemoryEventReadView::new(
            Arc::new(Mutex::new(events)),
            tail,
        )))
    }
}

pub(in crate::engine::tests) struct OrderedRewindSink {
    pub(in crate::engine::tests) fail_rewind: AtomicBool,
    pub(in crate::engine::tests) order: Arc<Mutex<Vec<String>>>,
    pub(in crate::engine::tests) events: Mutex<Vec<EngineEvent>>,
}

#[async_trait]
impl SessionEventSink for OrderedRewindSink {
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        if matches!(event, EngineEvent::ConversationRewound { .. }) {
            self.order
                .lock()
                .expect("rewind order")
                .push("persist".to_owned());
            if self.fail_rewind.load(Ordering::SeqCst) {
                return Err(AgentLoopError::Persistence(
                    "fixture rewind append failed".to_owned(),
                ));
            }
        }
        self.events.lock().expect("events").push(event.clone());
        Ok(event)
    }

    async fn append_batch(
        &self,
        batch: Vec<EngineEvent>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        let mut events = Vec::with_capacity(batch.len());
        for event in batch {
            events.push(self.append(event).await?);
        }
        Ok(events)
    }

    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        let events: Vec<EngineEvent> = {
            Ok(self
                .events
                .lock()
                .expect("events")
                .iter()
                .filter(|event| event.meta().is_some())
                .cloned()
                .collect())
        }?;
        let tail = events
            .last()
            .and_then(EngineEvent::meta)
            .map(|meta| meta.sequence_id);
        Ok(Arc::new(replay::MemoryEventReadView::new(
            Arc::new(Mutex::new(events)),
            tail,
        )))
    }
}

#[derive(Default)]
pub(in crate::engine::tests) struct ToggleLeaseSink {
    pub(in crate::engine::tests) events: Mutex<Vec<EngineEvent>>,
    pub(in crate::engine::tests) fail_driver_change: AtomicBool,
    pub(in crate::engine::tests) fail_question_answer: AtomicBool,
}

pub(in crate::engine::tests) struct CorruptGapSink {
    pub(in crate::engine::tests) event: EngineEvent,
}

#[async_trait]
impl SessionEventSink for CorruptGapSink {
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        Ok(event)
    }

    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        let events = vec![self.event.clone()];
        let tail = events
            .last()
            .and_then(EngineEvent::meta)
            .map(|meta| meta.sequence_id);
        Ok(Arc::new(replay::MemoryEventReadView::new(
            Arc::new(Mutex::new(events)),
            tail,
        )))
    }
}

#[async_trait]
impl SessionEventSink for ToggleLeaseSink {
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        if self.fail_driver_change.load(Ordering::SeqCst)
            && matches!(event, EngineEvent::DriverChanged { .. })
        {
            return Err(AgentLoopError::Persistence(
                "fixture driver-change failure".to_owned(),
            ));
        }
        if self.fail_question_answer.load(Ordering::SeqCst)
            && matches!(event, EngineEvent::QuestionAnswered { .. })
        {
            return Err(AgentLoopError::Persistence(
                "fixture question-answer failure".to_owned(),
            ));
        }
        self.events.lock().expect("events").push(event.clone());
        Ok(event)
    }

    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        let events: Vec<EngineEvent> = {
            Ok(self
                .events
                .lock()
                .expect("events")
                .iter()
                .filter(|event| event.meta().is_some())
                .cloned()
                .collect())
        }?;
        let tail = events
            .last()
            .and_then(EngineEvent::meta)
            .map(|meta| meta.sequence_id);
        Ok(Arc::new(replay::MemoryEventReadView::new(
            Arc::new(Mutex::new(events)),
            tail,
        )))
    }
}

#[derive(Debug)]
pub(in crate::engine::tests) struct CountedReplayView {
    pub(in crate::engine::tests) inner: Arc<dyn SessionEventReadView>,
    pub(in crate::engine::tests) pages: Arc<Mutex<Vec<usize>>>,
}

#[async_trait]
impl SessionEventReadView for CountedReplayView {
    fn last_sequence(&self) -> Option<SequenceId> {
        self.inner.last_sequence()
    }
    async fn read_page(
        &self,
        after: Option<SequenceId>,
        limits: SessionReplayLimits,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        let page = self.inner.read_page(after, limits).await?;
        self.pages.lock().expect("pages").push(page.len());
        Ok(page)
    }
}

#[derive(Default)]
pub(in crate::engine::tests) struct CountedReplaySink {
    pub(in crate::engine::tests) inner: NoopSessionEventSink,
    pub(in crate::engine::tests) pages: Arc<Mutex<Vec<usize>>>,
}

#[async_trait]
impl SessionEventSink for CountedReplaySink {
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        self.inner.append(event).await
    }
    async fn append_batch(
        &self,
        events: Vec<EngineEvent>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        self.inner.append_batch(events).await
    }

    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        Ok(Arc::new(CountedReplayView {
            inner: self.inner.capture_read_view()?,
            pages: Arc::clone(&self.pages),
        }))
    }
    async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        self.inner.last_sequence().await
    }
}
