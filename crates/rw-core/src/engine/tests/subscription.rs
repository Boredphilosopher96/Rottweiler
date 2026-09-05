#![cfg(test)]
use crate::engine::SessionEventSink;

use crate::engine::AgentLoopError;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::project_session_events;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::models::PendingModel;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::CorruptGapSink;
use crate::engine::tests::fixtures::sinks::CountedReplaySink;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::sinks::ToggleLeaseSink;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::protocol_meta;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::wire_event;
use rw_tools::ToolRegistry;
use rw_types::ClientCommand;
use rw_types::ClientId;
use rw_types::ClientRole;
use rw_types::CommandOutcome;
use rw_types::EngineEvent;
use rw_types::PROTOCOL_VERSION;
use rw_types::SequenceId;
use rw_types::SessionId;
use rw_types::config::PermissionDecision;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[tokio::test]
async fn large_reconnect_pages_pin_cursor_and_preserve_attach_ack_after_lag() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(CountedReplaySink::default());
    let mut seeded = vec![wire_event(
        0,
        PendingEvent::SessionCreated {
            driver_client_id: ClientId("prior".to_owned()),
        },
    )];
    seeded.extend((1..10_000).map(|sequence| {
        wire_event(
            sequence,
            PendingEvent::TextDelta {
                turn: 1,
                text: "x".to_owned(),
            },
        )
    }));
    let recovered = project_session_events(&seeded).expect("projection");
    crate::commit_session_events(Arc::clone(&sink), seeded)
        .await
        .expect("seed");
    let mut actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    actor_config.recovered = recovered;
    actor_config.event_capacity = 1;
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let session = SessionId("fixture-session".to_owned());
    let mut observer = handle
        .subscribe_client(ClientId("observer".to_owned()), Some(SequenceId(499)))
        .expect("subscription");
    observer.prime().await.expect("prime historical prefix");
    assert_eq!(*sink.pages.lock().expect("pages"), vec![256]);
    handle
        .dispatch(ClientCommand::SendMessage {
            meta: protocol_meta("prior", "new-between-subscribe-attach"),
            session_id: session.clone(),
            content: "/status".to_owned(),
            attachments: Vec::new(),
        })
        .await
        .expect("new durable event");
    assert_eq!(
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("observer", "attach"),
                session_id: session.clone(),
                last_seen_sequence: Some(SequenceId(499)),
                role: ClientRole::Observer,
            })
            .await
            .expect("attach"),
        CommandOutcome::Accepted {}
    );
    let mut sequences = Vec::new();
    timeout(Duration::from_secs(3), async {
        loop {
            let event = observer.recv().await.expect("replay");
            if let Some(meta) = event.meta() {
                sequences.push(meta.sequence_id.0);
            }
            if matches!(event, EngineEvent::CommandAcknowledged { .. }) {
                break;
            }
        }
    })
    .await
    .expect("bounded replay finishes");
    let tail = sink
        .last_sequence()
        .await
        .expect("tail")
        .expect("nonempty")
        .0;
    assert_eq!(sequences, (500..=tail).collect::<Vec<_>>());
    let pages = sink.pages.lock().expect("pages").clone();
    assert!(pages.len() >= 38);
    assert!(pages.iter().all(|size| *size <= 256));
    assert!(matches!(
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("observer", "future"),
                session_id: session,
                last_seen_sequence: Some(SequenceId(tail + 1)),
                role: ClientRole::Observer,
            })
            .await
            .expect("future rejected"),
        CommandOutcome::Rejected { .. }
    ));
}

#[tokio::test]
async fn lagged_subscription_replays_every_durable_sequence_and_continues_live() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::new([stop_script("many events", &[])])),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    actor_config.event_capacity = 1;
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let session_id = SessionId("fixture-session".to_owned());
    let mut events = handle
        .subscribe_client(ClientId("driver".to_owned()), None)
        .expect("subscription");
    handle
        .dispatch(ClientCommand::AttachSession {
            meta: protocol_meta("driver", "attach"),
            session_id: session_id.clone(),
            last_seen_sequence: None,
            role: ClientRole::Driver,
        })
        .await
        .expect("attach");
    assert!(matches!(
        events.recv().await.expect("created"),
        EngineEvent::SessionCreated { .. }
    ));
    assert!(matches!(
        events.recv().await.expect("attach ack"),
        EngineEvent::CommandAcknowledged { .. }
    ));
    handle
        .dispatch(ClientCommand::SendMessage {
            meta: protocol_meta("driver", "send"),
            session_id: session_id.clone(),
            content: "run".to_owned(),
            attachments: Vec::new(),
        })
        .await
        .expect("send");
    timeout(Duration::from_secs(1), async {
        loop {
            if handle.snapshot().await.expect("snapshot").completed_turns == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("turn completion");
    let durable_tail = sink
        .events
        .lock()
        .expect("events")
        .last()
        .expect("durable tail")
        .sequence;
    let mut replayed = Vec::new();
    while replayed.last().copied() != Some(durable_tail) {
        let event = events.recv().await.expect("gap event");
        if let Some(meta) = event.meta() {
            replayed.push(meta.sequence_id);
        }
    }
    assert_eq!(
        replayed,
        (1..=durable_tail.0).map(SequenceId).collect::<Vec<_>>()
    );
    handle
        .dispatch(ClientCommand::SendMessage {
            meta: protocol_meta("driver", "status"),
            session_id,
            content: "/status".to_owned(),
            attachments: Vec::new(),
        })
        .await
        .expect("status");
    loop {
        let event = events.recv().await.expect("continued live event");
        if let EngineEvent::CommandFinished { meta, name, .. } = event {
            assert_eq!(name, "status");
            assert_eq!(meta.sequence_id.0, durable_tail.0.saturating_add(1));
            break;
        }
    }
}

#[tokio::test]
async fn attach_and_subscription_reject_wrong_session_or_protocol_gap_events() {
    for corrupt in ["session", "protocol"] {
        let root = TempDir::new().expect("tempdir");
        let mut event = wire_event(
            0,
            PendingEvent::SessionCreated {
                driver_client_id: ClientId("prior".to_owned()),
            },
        );
        let meta = event.meta_mut().expect("meta");
        if corrupt == "session" {
            meta.session_id = SessionId("other-session".to_owned());
        } else {
            meta.protocol_version = PROTOCOL_VERSION.saturating_add(1);
        }
        let mut actor_config = config(
            root.path(),
            Arc::new(ScriptedModel::default()),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = Arc::new(CorruptGapSink { event });
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut subscription = handle
            .subscribe_client(ClientId("driver".to_owned()), None)
            .expect("subscription");
        assert!(matches!(
            handle
                .dispatch(ClientCommand::AttachSession {
                    meta: protocol_meta("driver", "attach"),
                    session_id: SessionId("fixture-session".to_owned()),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                })
                .await
                .expect("attach outcome"),
            CommandOutcome::Rejected { .. }
        ));
        assert!(matches!(
            subscription.recv().await,
            Err(AgentLoopError::Persistence(_))
        ));
    }
}

#[tokio::test]
async fn failed_takeover_does_not_mutate_the_driver_lease() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(ToggleLeaseSink::default());
    let mut actor_config = config(
        root.path(),
        Arc::new(PendingModel),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let session_id = SessionId("fixture-session".to_owned());
    assert!(matches!(
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("first", "first-attach"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("first attach"),
        CommandOutcome::Accepted {}
    ));
    assert!(matches!(
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("second", "second-attach"),
                session_id: session_id.clone(),
                last_seen_sequence: Some(0.into()),
                role: ClientRole::Observer,
            })
            .await
            .expect("second attach"),
        CommandOutcome::Accepted {}
    ));
    sink.fail_driver_change.store(true, Ordering::SeqCst);
    assert!(matches!(
        handle
            .dispatch(ClientCommand::TakeDriver {
                meta: protocol_meta("second", "failed-takeover"),
                session_id: session_id.clone(),
            })
            .await
            .expect("takeover outcome"),
        CommandOutcome::Rejected { .. }
    ));
    assert!(matches!(
        handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("second", "still-observer"),
                session_id,
                content: "must reject".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("observer outcome"),
        CommandOutcome::Rejected { .. }
    ));
}
