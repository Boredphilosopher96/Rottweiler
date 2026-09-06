#![allow(clippy::expect_used)]
use super::*;
use crate::engine::PendingEvent;
use rw_types::{CommandAckMeta, CommandOutcome, EventMeta, PROTOCOL_VERSION, SessionId};

fn durable(sequence: u64, bytes: usize) -> EngineEvent {
    PendingEvent::Error {
        message: "x".repeat(bytes),
    }
    .stamp(meta(sequence))
}
fn meta(sequence: u64) -> EventMeta {
    EventMeta {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId("delivery-test".into()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-09-06T00:00:00.000Z".into(),
        caused_by: None,
    }
}
fn ack(client: &str) -> RoutedEvent {
    RoutedEvent {
        target: Some(ClientId(client.into())),
        event: EngineEvent::CommandAcknowledged {
            meta: CommandAckMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId(client.into()),
                request_id: rw_types::RequestId("request".into()),
                emitted_at: "2026-09-06T00:00:00.000Z".into(),
            },
            session_id: Some(SessionId("delivery-test".into())),
            outcome: CommandOutcome::Accepted {},
        },
    }
}

#[tokio::test]
async fn shared_payload_keeps_credit_after_ring_eviction_until_final_consumer_drop() {
    let budget = Budget::new(1024 * 1024);
    let events = LiveEvents::with_budget(1, Arc::clone(&budget)).expect("channel");
    let mut first = events.subscribe(ClientId("a".into())).expect("first");
    let mut second = events.subscribe(ClientId("b".into())).expect("second");
    events
        .send(RoutedEvent {
            target: None,
            event: durable(0, 64 * 1024),
        })
        .expect("send");
    let Received::Event(first_event) = first.recv().await.expect("first") else {
        panic!("payload");
    };
    let Received::Event(second_event) = second.recv().await.expect("second") else {
        panic!("payload");
    };
    assert!(Arc::ptr_eq(&first_event.0, &second_event.0));
    let retained = budget.used();
    events
        .send(RoutedEvent {
            target: None,
            event: durable(1, 0),
        })
        .expect("evict");
    drop(first_event);
    assert!(budget.used() >= retained);
    drop(second_event);
    assert!(budget.used() < retained);
}

#[tokio::test]
async fn overwritten_reply_fails_only_its_slow_target_and_wakes_without_later_events() {
    let events = LiveEvents::with_budget(1, Budget::new(64 * 1024)).expect("channel");
    let mut target = events.subscribe(ClientId("target".into())).expect("target");
    let mut observer = events
        .subscribe(ClientId("observer".into()))
        .expect("observer");
    events.send(ack("target")).expect("ack");
    events
        .send(RoutedEvent {
            target: None,
            event: durable(0, 0),
        })
        .expect("overwrite");
    assert!(matches!(
        target.recv().await,
        Err(AgentLoopError::EventDeliverySaturated)
    ));
    assert!(matches!(
        observer.recv().await.expect("observer gap"),
        Received::CatchUp
    ));
    assert!(matches!(
        observer.recv().await.expect("observer payload"),
        Received::Event(_)
    ));
}

#[tokio::test]
async fn transient_byte_saturation_is_explicit_and_durable_fence_still_publishes() {
    let budget = Budget::new(64 * 1024);
    let events = LiveEvents::with_budget(1, Arc::clone(&budget)).expect("ring");
    let mut receiver = events.subscribe(ClientId("target".into())).expect("target");
    let mut independent = events.subscribe(ClientId("other".into())).expect("other");
    let _held = budget
        .reserve(64 * 1024 - budget.used())
        .expect("fill payload budget");
    assert!(matches!(
        events.send(ack("target")),
        Err(AgentLoopError::EventDeliverySaturated)
    ));
    assert!(matches!(
        receiver.recv().await,
        Err(AgentLoopError::EventDeliverySaturated)
    ));
    events
        .send(RoutedEvent {
            target: None,
            event: durable(0, 4096),
        })
        .expect("durable fence");
    assert!(matches!(
        independent.recv().await.expect("source gap"),
        Received::CatchUp
    ));
}

#[test]
fn capacity_and_subscription_admission_are_finite_and_refunded() {
    assert!(LiveEvents::new(0).is_err());
    assert!(LiveEvents::new(MAX_EVENT_CAPACITY + 1).is_err());
    let events = LiveEvents::with_budget(1, Budget::new(64 * 1024)).expect("channel");
    let receivers = (0..MAX_SESSION_SUBSCRIPTIONS)
        .map(|_| {
            events
                .subscribe(ClientId("same".into()))
                .expect("bounded subscriber")
        })
        .collect::<Vec<_>>();
    assert!(events.subscribe(ClientId("excess".into())).is_err());
    drop(receivers);
    assert!(events.subscribe(ClientId("reused".into())).is_ok());
}

#[tokio::test]
async fn closed_receiver_does_not_wait_on_surviving_handle() {
    let events = LiveEvents::with_budget(1, Budget::new(64 * 1024)).expect("channel");
    let mut receiver = events
        .subscribe(ClientId("client".into()))
        .expect("subscriber");
    events.close();
    assert!(matches!(
        receiver.recv().await.expect("closed"),
        Received::Closed
    ));
}

#[tokio::test]
async fn retained_ack_by_one_consumer_cannot_hide_eviction_from_another() {
    let events = LiveEvents::with_budget(1, Budget::new(64 * 1024)).expect("channel");
    let mut first = events.subscribe(ClientId("target".into())).expect("first");
    let mut stalled = events
        .subscribe(ClientId("target".into()))
        .expect("stalled");
    events.send(ack("target")).expect("ack");
    let Received::Event(held) = first.recv().await.expect("first ack") else {
        panic!("ack payload");
    };
    for sequence in 0..100 {
        events
            .send(RoutedEvent {
                target: None,
                event: durable(sequence, 0),
            })
            .expect("advance");
    }
    assert!(matches!(
        stalled.recv().await,
        Err(AgentLoopError::EventDeliverySaturated)
    ));
    drop(held);
    assert!(matches!(
        stalled.recv().await,
        Err(AgentLoopError::EventDeliverySaturated)
    ));
}

#[test]
fn receiver_retains_ring_credit_after_all_senders_drop() {
    let budget = Budget::new(64 * 1024);
    let events = LiveEvents::with_budget(8, Arc::clone(&budget)).expect("channel");
    let receiver = events
        .subscribe(ClientId("client".into()))
        .expect("receiver");
    let retained = budget.used();
    events.close();
    drop(events);
    assert_eq!(budget.used(), retained);
    drop(receiver);
    assert_eq!(budget.used(), 0);
}
