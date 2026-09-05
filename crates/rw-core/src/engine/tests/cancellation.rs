#![cfg(test)]

use crate::engine::AgentTurnStatus;
use crate::engine::MessageDisposition;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::ActorCommand;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::checkpoints::RecordingCheckpoints;
use crate::engine::tests::fixtures::controllers::SessionResourceFixture;
use crate::engine::tests::fixtures::models::CleanupModel;
use crate::engine::tests::fixtures::models::GatedCleanupProvider;
use crate::engine::tests::fixtures::models::PendingModel;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::protocol_meta;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::tool_script;
use crate::engine::tests::fixtures::tools::CleanupTool;
use crate::engine::tests::fixtures::tools::ExternalCleanupTool;
use crate::engine::tests::fixtures::tools::PanickingTool;
use rw_tools::ToolRegistry;
use rw_types::ClientCommand;
use rw_types::ClientRole;
use rw_types::CommandOutcome;
use rw_types::EngineEvent;
use rw_types::SessionId;
use rw_types::ToolOutput;
use rw_types::config::PermissionDecision;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::time::timeout;

#[tokio::test]
async fn actor_shutdown_runs_registered_tool_session_cleanup() {
    let root = TempDir::new().expect("tempdir");
    let ended = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(SessionResourceFixture {
            ended: Arc::clone(&ended),
        }))
        .expect("resource tool");
    let handle = SessionActor::spawn(config(
        root.path(),
        Arc::new(ScriptedModel::default()),
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    assert_eq!(
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("driver", "attach-resource"),
                session_id: SessionId("fixture-session".to_owned()),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach"),
        CommandOutcome::Accepted {}
    );
    assert!(handle.snapshot().await.expect("snapshot").active_background);
    assert!(matches!(
        handle
            .dispatch(ClientCommand::UserShellStarted {
                meta: protocol_meta("driver", "blocked-shell"),
                session_id: SessionId("fixture-session".to_owned()),
                command: "echo blocked".to_owned(),
            })
            .await
            .expect("shell outcome"),
        CommandOutcome::Rejected { .. }
    ));
    drop(handle);
    timeout(Duration::from_secs(3), async {
        while ended.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("tool session cleanup");
    assert_eq!(ended.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_provider_start_cannot_finish_turn_before_local_effect_settlement() {
    let root = TempDir::new().expect("tempdir");
    let provider = Arc::new(GatedCleanupProvider::default());
    let model = Arc::new(CleanupModel(
        rw_providers::ProviderRouter::new(
            BTreeMap::from([("fast".to_owned(), vec!["gated/model".to_owned()])]),
            [provider.clone() as Arc<dyn rw_providers::Provider>],
            rw_providers::RetryPolicy::default(),
        )
        .expect("router"),
    ));
    let actor_config = config(
        root.path(),
        model,
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    timeout(Duration::from_secs(2), provider.invoked.notified())
        .await
        .expect("provider started");
    assert!(handle.interrupt().await.expect("interrupt"));
    timeout(Duration::from_secs(2), provider.cleanup.notified())
        .await
        .expect("cleanup began after future drop");
    while let Ok(event) = events.receiver.try_recv() {
        assert!(!matches!(event.event, EngineEvent::TurnFinished { .. }));
    }
    assert!(!provider.settled.load(Ordering::SeqCst));
    provider.release.notify_one();
    let _ = collect_turn(&mut events).await;
    assert!(provider.settled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn cancellation_waits_for_tool_cleanup_before_result_checkpoint_and_terminal_events() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([tool_script(
        &[("cleanup-id", "cleanup_tool", json!({}))],
        &[],
    )]));
    let cleanup_finished = Arc::new(AtomicBool::new(false));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(CleanupTool {
            cleanup_finished: cleanup_finished.clone(),
        }))
        .expect("register cleanup tool");
    let checkpoints = Arc::new(RecordingCheckpoints::default());
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.checkpoints = checkpoints.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut receiver = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    next_matching(&mut receiver, |kind| {
        matches!(kind, PendingEvent::ToolCallStarted { .. })
    })
    .await;
    assert!(handle.interrupt().await.expect("interrupt"));
    let events = collect_turn(&mut receiver).await;
    assert!(cleanup_finished.load(Ordering::SeqCst));
    let cleanup_index = events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                PendingEvent::ToolOutput { chunk, .. } if chunk == "cleanup complete"
            )
        })
        .expect("cleanup output");
    let result_index = events
        .iter()
        .position(|event| matches!(event.kind, PendingEvent::ToolCallFinished { .. }))
        .expect("tool result");
    let terminal_index = events
        .iter()
        .position(|event| matches!(event.kind, PendingEvent::TurnFinished { .. }))
        .expect("terminal event");
    assert!(cleanup_index < result_index && result_index < terminal_index);
    assert!(
        checkpoints
            .events
            .lock()
            .expect("checkpoint events")
            .last()
            .is_some_and(|event| event.ends_with(":Cancelled"))
    );
    assert!(
        timeout(Duration::from_millis(50), receiver.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn dropped_tool_future_keeps_checkpoint_open_until_external_effects_settle() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([tool_script(
        &[("external-id", "external_cleanup", json!({}))],
        &[],
    )]));
    let tool = Arc::new(ExternalCleanupTool::default());
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).expect("register tool");
    let checkpoints = Arc::new(RecordingCheckpoints::default());
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.checkpoints = checkpoints.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    timeout(Duration::from_secs(3), tool.started.notified())
        .await
        .expect("tool started");
    assert!(handle.interrupt().await.expect("interrupt"));
    timeout(Duration::from_secs(4), tool.cleanup_started.notified())
        .await
        .expect("cleanup barrier");
    assert_eq!(checkpoints.events.lock().expect("checkpoints").len(), 1);
    while let Ok(event) = events.receiver.try_recv() {
        assert!(!matches!(
            event.event,
            EngineEvent::ToolCallFinished { .. } | EngineEvent::TurnFinished { .. }
        ));
    }
    tool.release_cleanup.notify_one();
    let _ = collect_turn(&mut events).await;
    assert!(tool.cleanup_finished.load(Ordering::SeqCst));
    assert!(
        checkpoints
            .events
            .lock()
            .expect("checkpoints")
            .last()
            .is_some_and(|event| event.ends_with(":Cancelled"))
    );
}

#[tokio::test]
async fn panicking_mutating_tool_is_failed_checkpointed_and_actor_remains_usable() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(&[("panic-id", "panic_tool", json!({}))], &[]),
        stop_script("recovered after panic", &[]),
        stop_script("next turn works", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(PanickingTool))
        .expect("register panic tool");
    let checkpoints = Arc::new(RecordingCheckpoints::default());
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.checkpoints = checkpoints.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut receiver = handle.subscribe().expect("subscription");
    handle
        .send_message("panic once")
        .await
        .expect("first message");
    let first = collect_turn(&mut receiver).await;
    assert!(first.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::ToolCallFinished {
            output: ToolOutput::Text { text },
            is_error: true,
            ..
        } if text.contains("panicked")
    )));
    assert!(
        checkpoints
            .events
            .lock()
            .expect("checkpoint events")
            .iter()
            .any(|event| event.ends_with(":Failed"))
    );
    assert_eq!(
        handle
            .send_message("still alive")
            .await
            .expect("second message"),
        MessageDisposition::Started
    );
    let second = collect_turn(&mut receiver).await;
    assert!(matches!(
        second.last().map(|event| &event.kind),
        Some(PendingEvent::TurnFinished {
            status: AgentTurnStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn queued_message_starts_after_a_well_formed_interrupted_turn() {
    let root = TempDir::new().expect("tempdir");
    let handle = SessionActor::spawn(config(
        root.path(),
        Arc::new(PendingModel),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    assert_eq!(
        handle.send_message("first").await.expect("first"),
        MessageDisposition::Started
    );
    assert_eq!(
        handle.send_message("second").await.expect("second"),
        MessageDisposition::Queued
    );
    assert!(handle.interrupt().await.expect("interrupt"));
    let first_finished = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::TurnFinished { turn: 1, .. })
    })
    .await;
    assert!(matches!(
        first_finished.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Interrupted,
            ..
        }
    ));
    next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::TurnStarted { turn: 2 })
    })
    .await;
    let (respond, receive) = oneshot::channel();
    handle
        .commands
        .send(ActorCommand::Interrupt {
            target_turn: 1,
            respond,
        })
        .await
        .expect("stale interrupt command");
    assert!(!receive.await.expect("stale interrupt response"));
    assert!(handle.snapshot().await.expect("snapshot").running);
    assert!(handle.interrupt().await.expect("cleanup interrupt"));
}
