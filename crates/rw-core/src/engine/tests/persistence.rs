#![cfg(test)]

use crate::engine::AgentLoopError;
use crate::engine::SessionActor;
use crate::engine::MessageDisposition;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::SessionRecoveredState;
use crate::engine::projection::project_session_events;
use crate::engine::tests::fixtures::models::PendingModel;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::FailFirstTextDeltaSink;
use crate::engine::tests::fixtures::sinks::FailNextBatchSink;
use crate::engine::tests::fixtures::sinks::FailingSink;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::TestEventSinkExt;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::stop_script;
use rw_tools::ToolRegistry;
use rw_types::Block;
use rw_types::EngineEvent;
use rw_types::Role;
use rw_types::SessionId;
use rw_types::config::PermissionDecision;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[test]
fn actor_rejects_session_ids_outside_the_storage_safe_alphabet() {
    let root = TempDir::new().expect("tempdir");
    let mut actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.session_id = SessionId("../escape".to_owned());
    assert!(matches!(
        SessionActor::spawn(actor_config),
        Err(AgentLoopError::InvalidConfiguration(_))
    ));
}

#[tokio::test]
async fn recovered_sequence_and_user_message_are_appended_before_broadcast() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([stop_script("done", &[])]));
    let sink = Arc::new(RecordingSink {
        events: Mutex::new(Vec::new()),
        batch_sizes: Mutex::new(Vec::new()),
        tail_floor: Mutex::new(Some(40.into())),
    });
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    actor_config.recovered = SessionRecoveredState {
        conversation: Vec::new(),
        queued_messages: Vec::new(),
        completed_turns: 6,
        next_turn: 7,
        last_sequence: Some(40.into()),
        interrupted_turn: None,
        turn_ends: BTreeMap::new(),
        ..SessionRecoveredState::default()
    };
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("persist me").await.expect("message");
    let started = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::TurnStarted { .. })
    })
    .await;
    assert_eq!(started.sequence, 42.into());
    assert!(matches!(
        started.kind,
        PendingEvent::TurnStarted { turn: 7 }
    ));
    let accepted = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::UserMessageAccepted { .. })
    })
    .await;
    assert_eq!(accepted.sequence, 43.into());
    assert!(matches!(
        &accepted.kind,
        PendingEvent::UserMessageAccepted { turn: 7, content, .. }
            if content == "persist me"
    ));
    let persisted = sink.events.lock().expect("sink lock");
    assert_eq!(persisted.get(2), Some(&accepted));
}

#[tokio::test]
async fn persistence_failure_is_returned_before_provider_work_or_broadcast() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([stop_script("unused", &[])]));
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = Arc::new(FailingSink);
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    assert!(matches!(
        handle.send_message("must persist").await,
        Err(AgentLoopError::Persistence(_))
    ));
    assert_eq!(model.request_count(), 0);
}

#[tokio::test]
async fn transient_turn_opening_failure_does_not_poison_the_live_session() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(FailNextBatchSink::default());
    let mut actor_config = config(
        root.path(),
        Arc::new(PendingModel),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    handle.ensure_local_driver().await.expect("local driver");

    sink.fail_next.store(true, Ordering::Release);
    assert!(matches!(
        handle.send_message("first attempt").await,
        Err(AgentLoopError::Persistence(_))
    ));

    assert_eq!(
        handle.send_message("retry normally").await.expect("retry"),
        MessageDisposition::Started
    );
    let persisted = sink.inner.events.lock().expect("persisted events");
    assert!(persisted.iter().any(|event| {
        matches!(
            &event.kind,
            PendingEvent::UserMessageAccepted { content, .. } if content == "retry normally"
        )
    }));
}

#[tokio::test]
async fn transient_turn_signal_failure_recovers_journal_and_accepts_next_turn() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(FailFirstTextDeltaSink::default());
    let model = Arc::new(ScriptedModel::new([
        stop_script("first response", &[]),
        stop_script("second response", &[]),
    ]));
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.ensure_local_driver().await.expect("local driver");

    assert_eq!(
        handle
            .send_message("first attempt")
            .await
            .expect("first turn"),
        MessageDisposition::Started
    );
    let repaired = timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.expect("recovery event");
            if matches!(
                event,
                EngineEvent::TurnFinished {
                    status: rw_types::TurnStatus::Interrupted,
                    ..
                }
            ) {
                break;
            }
        }
    })
    .await;
    assert!(
        repaired.is_ok(),
        "interrupted turn should be durably repaired"
    );

    assert_eq!(
        handle.send_message("retry normally").await.expect("retry"),
        MessageDisposition::Started
    );
    let completed = timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.expect("completion event");
            if matches!(
                event,
                EngineEvent::TurnFinished {
                    status: rw_types::TurnStatus::Completed,
                    ..
                }
            ) {
                break;
            }
        }
    })
    .await;
    assert!(
        completed.is_ok(),
        "the recovered actor should complete a later turn"
    );

    let durable = sink
        .inner
        .test_events_after(None)
        .await
        .expect("durable log");
    let recovered = project_session_events(&durable).expect("replay repaired journal");
    assert_eq!(recovered.completed_turns, 2);
    assert!(recovered.conversation.iter().any(|turn| {
        turn.role == Role::Assistant
            && turn
                .blocks
                .iter()
                .any(|block| matches!(block, Block::Text { text } if text == "second response"))
    }));
}
