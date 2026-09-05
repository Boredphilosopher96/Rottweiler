#![cfg(test)]

use crate::engine::builtin_hook_dispatcher;
use crate::engine::projection::project_session_events;
use crate::engine::session::SessionActor;
use crate::engine::session::StartupNotification;
use crate::engine::tests::fixtures::models::InstructionModel;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use rw_ext::HookDispatcher;
use rw_tools::ToolRegistry;
use rw_types::Block;
use rw_types::EngineEvent;
use rw_types::Role;
use rw_types::Turn;
use rw_types::TurnMeta;
use rw_types::config::PermissionDecision;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;

#[tokio::test]
async fn startup_notifications_are_persisted_as_status_and_ui_events() {
    let root = tempfile::tempdir().expect("root");
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        HookDispatcher::new(),
    );
    actor_config.event_sink = sink.clone();
    actor_config.startup_notifications = vec![StartupNotification {
        plugin_id: "wasm:fixture".to_owned(),
        status: "unavailable".to_owned(),
        title: "WASM extension unavailable".to_owned(),
        message: "The component failed validation.".to_owned(),
    }];
    let handle = SessionActor::spawn(actor_config).expect("actor");
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if sink.events.lock().expect("events").len() >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("startup events");
    let events = sink.events.lock().expect("events");
    assert!(matches!(
        &events[0].wire,
        EngineEvent::PluginStatusChanged { plugin_id, status, .. }
            if plugin_id == "wasm:fixture" && status == "unavailable"
    ));
    assert!(matches!(
        &events[1].wire,
        EngineEvent::UiNotification { plugin_id, title, message, .. }
            if plugin_id == "wasm:fixture"
                && title == "WASM extension unavailable"
                && message == "The component failed validation."
    ));
    drop(events);
    drop(handle);
}

#[tokio::test]
async fn initial_project_instructions_steer_replay_without_entering_committed_history() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(InstructionModel::default());
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.initial_session_context = vec![Turn {
        role: Role::System,
        blocks: vec![Block::Text {
            text: "Root AGENTS.md: reply kennel".to_owned(),
        }],
        meta: TurnMeta::default(),
    }];
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("what word?").await.expect("message");
    collect_turn(&mut events).await;
    assert!(model.observed.load(Ordering::SeqCst));
    let persisted = sink.events.lock().expect("event sink").clone();
    let wire = persisted
        .iter()
        .map(|event| event.wire.clone())
        .collect::<Vec<_>>();
    let recovered = project_session_events(&wire).expect("project persisted events");
    assert!(
        recovered
            .conversation
            .iter()
            .all(|turn| turn.role != Role::System)
    );
    assert_eq!(recovered.conversation.len(), 2);
}
