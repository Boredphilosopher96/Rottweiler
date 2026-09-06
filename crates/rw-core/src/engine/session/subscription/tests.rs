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
        } else if let EngineEvent::Error { message, .. } = event.as_ref() {
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
    assert!(matches!(event.as_ref(), EngineEvent::Error { message, .. } if message == &body));
    assert!(matches!(
        subscription.recv().await,
        Err(AgentLoopError::Closed)
    ));
}
