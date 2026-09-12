#![allow(clippy::expect_used)]
use super::*;
use crate::engine::live_events::LiveEvents;
use crate::engine::{
    AgentTurnStatus, NoopSessionEventSink, PendingEvent, SessionUsage, commit_session_events,
};
use rw_types::{ClientId, Cost, EventMeta};

fn stamp(sequence: u64, event: PendingEvent) -> EngineEvent {
    event.stamp(EventMeta {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId("replay-test".into()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-09-06T00:00:00.000Z".into(),
        caused_by: None,
    })
}
fn subscribe(events: &LiveEvents, sink: Arc<dyn SessionEventSink>) -> SessionSubscription {
    SessionSubscription {
        session_id: SessionId("replay-test".into()),
        receiver: events
            .subscribe(ClientId("client".into()))
            .expect("receiver"),
        sink,
        last_sequence: None,
        initial_tail: None,
        pending: std::collections::VecDeque::new(),
        replay: None,
        read: None,
        needs_initial_replay: false,
    }
}
async fn publish(events: &LiveEvents, sink: &Arc<NoopSessionEventSink>, event: EngineEvent) {
    let committed = commit_session_events(Arc::clone(sink), vec![event])
        .await
        .expect("commit");
    for event in committed.events() {
        events
            .send(crate::engine::RoutedEvent {
                target: None,
                event: event.clone(),
            })
            .expect("publish fence");
    }
}

#[tokio::test]
async fn slow_big_event_consumer_recovers_exact_final_source_without_another_send() {
    let events = LiveEvents::with_limit(2, 8192).expect("small live allowance");
    let sink = Arc::new(NoopSessionEventSink::default());
    let mut subscription = subscribe(&events, sink.clone());
    for sequence in 0..8 {
        publish(
            &events,
            &sink,
            stamp(
                sequence,
                PendingEvent::Error {
                    message: format!("{sequence}:{}", "x".repeat(128 * 1024)),
                },
            ),
        )
        .await;
    }
    publish(
        &events,
        &sink,
        stamp(
            8,
            PendingEvent::TurnFinished {
                turn: 1,
                status: AgentTurnStatus::Completed,
                usage: SessionUsage::default(),
                cost: Cost::Unavailable {
                    reason: "fixture".into(),
                },
            },
        ),
    )
    .await;
    // The actor exits here. The handle and its sender remain alive, and no
    // further append can be used to wake or evict the terminal event.
    events.close();
    for sequence in 0..9 {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), subscription.recv())
            .await
            .expect("bounded replay")
            .expect("event");
        assert_eq!(
            event.meta().expect("meta").sequence_id,
            SequenceId(sequence)
        );
        if sequence == 8 {
            assert!(matches!(event.as_ref(), EngineEvent::TurnFinished { .. }));
        } else if let EngineEvent::Error {
            error: rw_types::EngineError { message, .. },
            ..
        } = event.as_ref()
        {
            assert_eq!(message, &format!("{sequence}:{}", "x".repeat(128 * 1024)));
        } else {
            panic!("wrong event");
        }
    }
    assert!(matches!(
        subscription.recv().await,
        Err(AgentLoopError::Closed)
    ));
}

#[tokio::test]
async fn replay_admits_single_legal_record_larger_than_default_eight_megabyte_page() {
    let events = LiveEvents::with_limit(1, 8192).expect("fence only");
    let sink = Arc::new(NoopSessionEventSink::default());
    let mut subscription = subscribe(&events, sink.clone());
    let body = "z".repeat(9 * 1024 * 1024);
    publish(
        &events,
        &sink,
        stamp(
            0,
            PendingEvent::Error {
                message: body.clone(),
            },
        ),
    )
    .await;
    events.close();
    let event = subscription.recv().await.expect("large event source");
    assert!(
        matches!(event.as_ref(), EngineEvent::Error { error: rw_types::EngineError { message, .. }, .. } if message == &body)
    );
    assert!(matches!(
        subscription.recv().await,
        Err(AgentLoopError::Closed)
    ));
}

#[derive(Debug)]
struct GatedView {
    entered: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
    finished: Arc<tokio::sync::Semaphore>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait::async_trait]
impl SessionEventReadView for GatedView {
    fn last_sequence(&self) -> Option<SequenceId> {
        Some(SequenceId(0))
    }
    async fn read_page(
        &self,
        after: Option<SequenceId>,
        limits: SessionReplayLimits,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        assert!(after.is_none());
        assert_eq!(limits.max_events, 256);
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.entered.add_permits(1);
        self.release.acquire().await.expect("release gate").forget();
        self.finished.add_permits(1);
        Ok(vec![stamp(
            0,
            PendingEvent::Error {
                message: "source".into(),
            },
        )])
    }
}
fn gated_subscription() -> (SessionSubscription, Arc<GatedView>) {
    let events = LiveEvents::with_limit(1, 8192).expect("live channel");
    let sink: Arc<dyn SessionEventSink> = Arc::new(NoopSessionEventSink::default());
    let mut subscription = subscribe(&events, sink);
    let view = Arc::new(GatedView {
        entered: Arc::new(tokio::sync::Semaphore::new(0)),
        release: Arc::new(tokio::sync::Semaphore::new(0)),
        finished: Arc::new(tokio::sync::Semaphore::new(0)),
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    subscription.replay = Some(view.clone());
    subscription.initial_tail = Some(SequenceId(0));
    subscription.needs_initial_replay = true;
    (subscription, view)
}

#[tokio::test]
async fn cancelled_recv_preserves_exact_owned_replay_task_for_next_poll() {
    let (mut subscription, view) = gated_subscription();
    tokio::select! {
        result = subscription.recv() => panic!("read must wait: {result:?}"),
        entered = view.entered.acquire() => entered.expect("read entered").forget(),
    }
    assert_eq!(view.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    view.release.add_permits(1);
    let event = subscription.recv().await.expect("same read resumes");
    assert_eq!(event.meta().expect("source").sequence_id, SequenceId(0));
    assert_eq!(view.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropped_subscription_keeps_started_source_read_owned_until_completion() {
    let (mut subscription, view) = gated_subscription();
    tokio::select! {
        result = subscription.recv() => panic!("read must wait: {result:?}"),
        entered = view.entered.acquire() => entered.expect("read entered").forget(),
    }
    drop(subscription);
    view.release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(2), view.finished.acquire())
        .await
        .expect("owned read finishes after caller loss")
        .expect("finished")
        .forget();
    assert_eq!(view.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}
