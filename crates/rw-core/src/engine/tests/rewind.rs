#![cfg(test)]

use crate::engine::AgentLoopError;
use crate::engine::MessageDisposition;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::dispatch;
use crate::engine::model;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::checkpoints::OrderedRewindCoordinator;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::OrderedRewindSink;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::protocol_meta;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::turn;
use rw_tools::ToolRegistry;
use rw_types::ClientCommand;
use rw_types::ClientRole;
use rw_types::CommandOutcome;
use rw_types::RewindTarget;
use rw_types::SessionId;
use rw_types::Turn;
use rw_types::TurnId;
use rw_types::UnrestorablePath;
use rw_types::config::PermissionDecision;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn rewind_applies_then_persists_then_acknowledges_and_never_acks_failed_append() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        stop_script("one", &[]),
        stop_script("two", &[]),
        stop_script("three", &[]),
    ]));
    let order = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(OrderedRewindSink {
        fail_rewind: AtomicBool::new(false),
        order: order.clone(),
        events: Mutex::new(Vec::new()),
    });
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    let fail_ack = Arc::new(AtomicBool::new(false));
    actor_config.checkpoints = Arc::new(OrderedRewindCoordinator {
        order: order.clone(),
        fail_ack: fail_ack.clone(),
        unrestorable_paths: Vec::new(),
    });
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("first").await.expect("first");
    collect_turn(&mut events).await;
    handle.send_message("second").await.expect("second");
    collect_turn(&mut events).await;
    assert_eq!(
        handle
            .send_message("/rewind 1")
            .await
            .expect("rewind command"),
        MessageDisposition::Command
    );
    next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::ConversationRewound { to_turn: 1, .. })
    })
    .await;
    assert_eq!(
        order.lock().expect("rewind order").as_slice(),
        &["apply", "persist", "ack"]
    );
    assert_eq!(
        handle
            .snapshot()
            .await
            .expect("snapshot")
            .conversation
            .len(),
        2
    );

    handle.send_message("third").await.expect("third");
    collect_turn(&mut events).await;
    assert_eq!(
        handle
            .snapshot()
            .await
            .expect("snapshot")
            .conversation
            .len(),
        4
    );
    order.lock().expect("rewind order").clear();
    fail_ack.store(true, Ordering::SeqCst);
    assert!(matches!(
        handle.rewind(1).await,
        Err(AgentLoopError::Persistence(_))
    ));
    assert_eq!(
        handle
            .snapshot()
            .await
            .expect("snapshot")
            .conversation
            .len(),
        2
    );
    assert_eq!(
        order.lock().expect("rewind order").as_slice(),
        &["apply", "persist", "ack"]
    );
    fail_ack.store(false, Ordering::SeqCst);
    handle.rewind(1).await.expect("retry pending ack");
    assert_eq!(
        order.lock().expect("rewind order").as_slice(),
        &["apply", "persist", "ack", "ack"]
    );

    order.lock().expect("rewind order").clear();
    sink.fail_rewind.store(true, Ordering::SeqCst);
    assert!(matches!(
        handle.rewind(1).await,
        Err(AgentLoopError::Persistence(_))
    ));
    assert_eq!(
        order.lock().expect("rewind order").as_slice(),
        &["apply", "persist"]
    );
}

#[tokio::test]
async fn slash_rewind_reports_unrestorable_paths_in_command_event() {
    let root = TempDir::new().expect("tempdir");
    let mut actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::new([stop_script("one", &[])])),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.checkpoints = Arc::new(OrderedRewindCoordinator {
        order: Arc::new(Mutex::new(Vec::new())),
        fail_ack: Arc::new(AtomicBool::new(false)),
        unrestorable_paths: vec![UnrestorablePath {
            path: "missing.txt".to_owned(),
            reason: "deleted outside the checkpoint scope".to_owned(),
        }],
    });
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("first").await.expect("first");
    collect_turn(&mut events).await;
    handle
        .send_message("/rewind 1")
        .await
        .expect("rewind command");
    let command = next_matching(
        &mut events,
        |kind| matches!(kind, PendingEvent::CommandFinished { name, .. } if name == "rewind"),
    )
    .await;
    assert!(matches!(
        command.kind,
        PendingEvent::CommandFinished {
            unrestorable_paths,
            ..
        } if unrestorable_paths == vec![UnrestorablePath {
            path: "missing.txt".to_owned(),
            reason: "deleted outside the checkpoint scope".to_owned(),
        }]
    ));
}

#[tokio::test]
async fn invalid_protocol_rewind_is_rejected_without_poisoning_the_session() {
    let root = TempDir::new().expect("tempdir");
    let handle = SessionActor::spawn(config(
        root.path(),
        Arc::new(ScriptedModel::new([stop_script("healthy", &[])])),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let session_id = SessionId("fixture-session".to_owned());
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
        handle
            .dispatch(ClientCommand::Rewind {
                meta: protocol_meta("driver", "bad-rewind"),
                session_id: session_id.clone(),
                target: RewindTarget::Turn {
                    turn_id: TurnId("999".to_owned()),
                },
            })
            .await
            .expect("rewind outcome"),
        CommandOutcome::Rejected { .. }
    ));
    assert_eq!(
        handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "healthy-message"),
                session_id,
                content: "continue".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("healthy command"),
        CommandOutcome::Accepted
    );
    timeout(Duration::from_secs(1), async {
        loop {
            if handle.snapshot().await.expect("snapshot").completed_turns == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("healthy turn completion");
}
