use super::{DeliveryContext, EventConsumer, PluginFanoutWorker};
use async_trait::async_trait;
use rw_core::SESSION_EVENT_VERSION;
use rw_core::{
    AgentLoopError, EngineEvent, SessionEventReadView, SessionEventSink, SessionReplayLimits,
};
use rw_ext::PluginRpcError;
use rw_tools::CancellationToken;
use rw_types::{
    EventMeta, SequenceId, SessionId,
    extension_contract::{
        ExtensionStateCommitOutcome, ExtensionStateSnapshot, ExtensionStateTransaction,
    },
    extension_events::{
        ExtensionEventKind, ExtensionEventNotice, ExtensionEventOutcome, ExtensionEventRead,
    },
};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Notify;

#[derive(Debug)]
struct Journal {
    events: Vec<EngineEvent>,
}
#[async_trait]
impl SessionEventReadView for Journal {
    fn last_sequence(&self) -> Option<SequenceId> {
        self.events
            .last()
            .and_then(EngineEvent::meta)
            .map(|meta| meta.sequence_id)
    }
    async fn read_page(
        &self,
        after: Option<SequenceId>,
        limits: SessionReplayLimits,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        assert_eq!(limits.max_events, 1);
        Ok(self
            .events
            .iter()
            .filter(|event| {
                after.is_none_or(|after| event.meta().is_some_and(|meta| meta.sequence_id > after))
            })
            .take(1)
            .cloned()
            .collect())
    }
}
#[async_trait]
impl SessionEventSink for Journal {
    async fn completed_turn(
        &self,
        _turn: u64,
    ) -> Result<Option<rw_core::CompletedTurn>, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "delivery fixture has no completed turn index".into(),
        ))
    }

    async fn todo_state(&self) -> Result<rw_types::todo::TodoSnapshot, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "delivery fixture does not expose task state".into(),
        ))
    }

    async fn source_rewind_target(
        &self,
        _expected: SequenceId,
        _source: SequenceId,
        _turn: u64,
        _position: rw_types::RewindSourcePosition,
    ) -> Result<u64, AgentLoopError> {
        Err(AgentLoopError::Closed)
    }

    async fn extension_state(
        &self,
        _plugin: &str,
    ) -> Result<rw_core::ExtensionStateView, AgentLoopError> {
        Err(AgentLoopError::Closed)
    }
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }
    async fn reserve(
        &self,
        _plan: &rw_core::EventBatchPlan,
    ) -> Result<rw_core::EventBatchReservation, AgentLoopError> {
        Err(AgentLoopError::Closed)
    }
    async fn commit(
        self: Arc<Self>,
        _batch: Arc<rw_core::AdmittedEventBatch>,
    ) -> Result<Arc<rw_core::AdmittedEventBatch>, AgentLoopError> {
        Err(AgentLoopError::Closed)
    }
    fn capture_read_view(&self) -> Result<Arc<dyn SessionEventReadView>, AgentLoopError> {
        Ok(Arc::new(Self {
            events: self.events.clone(),
        }))
    }
    async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        Ok(SessionEventReadView::last_sequence(self))
    }
    async fn budget_totals(
        &self,
        _query: rw_core::BudgetLedgerQuery,
    ) -> Result<rw_core::BudgetLedgerTotals, AgentLoopError> {
        Err(AgentLoopError::Closed)
    }
}
struct Consumer {
    state: Mutex<ExtensionStateSnapshot>,
    committed: Mutex<Vec<ExtensionStateTransaction>>,
    entered: Notify,
    release: Notify,
    blocked: AtomicBool,
    fail: AtomicBool,
    settled: AtomicBool,
    notices: Mutex<Vec<ExtensionEventNotice>>,
}
impl Consumer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ExtensionStateSnapshot {
                revision: None,
                entries: Vec::new(),
                acknowledged: None,
                delivery_start: None,
            }),
            committed: Mutex::new(Vec::new()),
            entered: Notify::new(),
            release: Notify::new(),
            blocked: AtomicBool::new(false),
            fail: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            notices: Mutex::new(Vec::new()),
        })
    }
}
#[async_trait]
impl EventConsumer for Consumer {
    async fn snapshot(
        &self,
        _cancellation: &CancellationToken,
    ) -> Result<ExtensionStateSnapshot, AgentLoopError> {
        Ok(self
            .state
            .lock()
            .unwrap_or_else(|error| panic!("fixture: {error:?}"))
            .clone())
    }
    async fn commit(
        &self,
        transaction: ExtensionStateTransaction,
        _cancellation: &CancellationToken,
    ) -> Result<ExtensionStateCommitOutcome, AgentLoopError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|error| panic!("fixture: {error:?}"));
        assert_eq!(transaction.expected_revision, state.revision);
        state.acknowledged = transaction.acknowledged.clone();
        let revision = SequenceId(state.revision.map_or(1000, |revision| revision.0 + 1));
        state.revision = Some(revision);
        self.committed
            .lock()
            .unwrap_or_else(|error| panic!("fixture: {error:?}"))
            .push(transaction);
        Ok(ExtensionStateCommitOutcome::Committed { revision })
    }
    async fn deliver(
        &self,
        notice: ExtensionEventNotice,
        _cancellation: &CancellationToken,
    ) -> Result<ExtensionEventOutcome, PluginRpcError> {
        self.notices
            .lock()
            .unwrap_or_else(|error| panic!("fixture: {error:?}"))
            .push(notice);
        self.entered.notify_one();
        if self.blocked.load(Ordering::Acquire) {
            self.release.notified().await;
        }
        if self.fail.swap(false, Ordering::AcqRel) {
            return Err(PluginRpcError {
                code: "fixture".into(),
                message: "delivery failed".into(),
            });
        }
        Ok(ExtensionEventOutcome {
            mutations: Vec::new(),
        })
    }
    async fn settle(&self) -> Result<(), PluginRpcError> {
        self.settled.store(true, Ordering::Release);
        Ok(())
    }
}
fn journal(count: u64) -> Arc<Journal> {
    Arc::new(Journal {
        events: (0..count)
            .map(|sequence| EngineEvent::PluginStatusChanged {
                meta: EventMeta {
                    protocol_version: SESSION_EVENT_VERSION,
                    session_id: SessionId("session".into()),
                    sequence_id: SequenceId(sequence),
                    emitted_at: "2026-09-05T00:00:00Z".into(),
                    caused_by: None,
                },
                plugin_id: "example".into(),
                status: "ready".into(),
            })
            .collect(),
    })
}
fn start(
    journal: Arc<Journal>,
    consumer: Arc<Consumer>,
) -> (
    PluginFanoutWorker,
    Arc<crate::extension_runtime::PluginEventSources>,
) {
    let budget = Arc::new(crate::extension_runtime::PluginDeliveryBudget::default());
    let sources = Arc::new(crate::extension_runtime::PluginEventSources::default());
    let permit = budget
        .worker()
        .unwrap_or_else(|error| panic!("fixture: {error:?}"));
    let context = DeliveryContext {
        source: journal,
        sources: sources.clone(),
        budget,
        consumer,
        redactor: rw_providers::FixtureRedactor::default(),
    };
    let worker = PluginFanoutWorker::start(
        "example".into(),
        BTreeSet::from([ExtensionEventKind::PluginStatusChanged]),
        context,
        permit,
    );
    worker.activate();
    (worker, sources)
}
async fn wait_for(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|error| panic!("fixture: {error:?}"));
}

#[tokio::test]
async fn backlog_exceeding_queue_size_recovers_without_payload_wakes() {
    let consumer = Consumer::new();
    let (worker, _) = start(journal(130), consumer.clone());
    wait_for(|| {
        consumer
            .committed
            .lock()
            .unwrap_or_else(|error| panic!("fixture: {error:?}"))
            .len()
            == 130
    })
    .await;
    worker.cancel();
    worker
        .settle()
        .await
        .unwrap_or_else(|error| panic!("fixture: {error:?}"));
    let committed = consumer
        .committed
        .lock()
        .unwrap_or_else(|error| panic!("fixture: {error:?}"));
    for (sequence, transaction) in committed.iter().enumerate() {
        assert_eq!(
            transaction
                .acknowledged
                .as_ref()
                .unwrap_or_else(|| panic!("fixture acknowledgement"))
                .sequence
                .0,
            sequence as u64
        );
    }
}
#[tokio::test]
async fn failed_delivery_is_replayed_from_durable_ack_on_restart() {
    let consumer = Consumer::new();
    consumer.fail.store(true, Ordering::Release);
    let (worker, _) = start(journal(2), consumer.clone());
    worker
        .settle()
        .await
        .unwrap_or_else(|error| panic!("fixture: {error:?}"));
    assert!(
        consumer
            .committed
            .lock()
            .unwrap_or_else(|error| panic!("fixture: {error:?}"))
            .is_empty()
    );
    let (resumed, _) = start(journal(2), consumer.clone());
    wait_for(|| {
        consumer
            .committed
            .lock()
            .unwrap_or_else(|error| panic!("fixture: {error:?}"))
            .len()
            == 2
    })
    .await;
    resumed.cancel();
    resumed
        .settle()
        .await
        .unwrap_or_else(|error| panic!("fixture: {error:?}"));
    let notices = consumer
        .notices
        .lock()
        .unwrap_or_else(|error| panic!("fixture: {error:?}"));
    assert_eq!(notices[0].cursor, notices[1].cursor);
}
#[tokio::test]
async fn dropping_shutdown_wait_never_discards_accepted_callback_or_source() {
    let consumer = Consumer::new();
    consumer.blocked.store(true, Ordering::Release);
    let (worker, sources) = start(journal(1), consumer.clone());
    consumer.entered.notified().await;
    let cursor = consumer
        .notices
        .lock()
        .unwrap_or_else(|error| panic!("fixture: {error:?}"))[0]
        .cursor
        .clone();
    let request = ExtensionEventRead {
        cursor,
        offset: 0,
        max_bytes: 64,
    };
    assert!(sources.read(&request).is_ok());
    worker.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), worker.settle())
            .await
            .is_err()
    );
    assert!(!consumer.settled.load(Ordering::Acquire));
    assert!(sources.read(&request).is_ok());
    consumer.release.notify_one();
    worker
        .settle()
        .await
        .unwrap_or_else(|error| panic!("fixture: {error:?}"));
    assert!(sources.read(&request).is_err());
    assert!(
        consumer
            .committed
            .lock()
            .unwrap_or_else(|error| panic!("fixture: {error:?}"))
            .is_empty()
    );
}
