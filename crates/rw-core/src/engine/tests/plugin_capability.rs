#![cfg(test)]

use crate::engine::MAX_PLUGIN_ID_BYTES;
use crate::engine::MAX_PLUGIN_MESSAGE_BYTES;
use crate::engine::MAX_PLUGIN_NOTIFICATION_MESSAGE_BYTES;
use crate::engine::MAX_PLUGIN_NOTIFICATION_TITLE_BYTES;
use crate::engine::MAX_PLUGIN_STATUS_BYTES;
use crate::engine::MessageDisposition;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::dispatch;
use crate::engine::projection::project_session_events;
use crate::engine::session;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::models::PendingModel;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::CanarySecretRedactor;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::protocol_meta;
use crate::engine::turn;
use rw_tools::ToolRegistry;
use rw_types::ClientCommand;
use rw_types::ClientId;
use rw_types::ClientRole;
use rw_types::CommandOutcome;
use rw_types::EngineEvent;
use rw_types::SessionId;
use rw_types::config::PermissionDecision;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn plugin_machine_capability_preserves_driver_queue_and_durable_order() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        Arc::new(PendingModel),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    actor_config.secret_redactor = Arc::new(CanarySecretRedactor);
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let session_id = SessionId("fixture-session".to_owned());
    assert_eq!(
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("tui", "attach-tui"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach TUI"),
        CommandOutcome::Accepted
    );
    assert_eq!(
        handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("tui", "start-turn"),
                session_id: session_id.clone(),
                content: "first".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("start pending turn"),
        CommandOutcome::Accepted
    );

    let plugin = handle
        .plugin_session_capability("fixture-plugin")
        .expect("plugin capability");
    assert_eq!(
        plugin
            .inject_message("/help KNOWN_CANARY")
            .await
            .expect("queue injected message"),
        MessageDisposition::Queued
    );
    plugin
        .set_status("working KNOWN_CANARY")
        .await
        .expect("plugin status");
    plugin
        .notify("fixture", "notice KNOWN_CANARY")
        .await
        .expect("plugin notification");
    assert_eq!(
        handle
            .snapshot()
            .await
            .expect("queued snapshot")
            .queued_messages,
        vec!["/help [REDACTED]"]
    );

    let before_denials = sink.events.lock().expect("events").len();
    assert!(handle.plugin_session_capability("Invalid-Plugin").is_err());
    assert!(
        handle
            .plugin_session_capability("x".repeat(MAX_PLUGIN_ID_BYTES.saturating_add(1)))
            .is_err()
    );
    assert!(plugin.inject_message("bad\nmessage").await.is_err());
    assert!(
        plugin
            .inject_message("x".repeat(MAX_PLUGIN_MESSAGE_BYTES.saturating_add(1)))
            .await
            .is_err()
    );
    assert!(plugin.set_status("bad\tstatus").await.is_err());
    assert!(
        plugin
            .set_status("x".repeat(MAX_PLUGIN_STATUS_BYTES.saturating_add(1)))
            .await
            .is_err()
    );
    assert!(plugin.notify("bad\ntitle", "message").await.is_err());
    assert!(
        plugin
            .notify(
                "x".repeat(MAX_PLUGIN_NOTIFICATION_TITLE_BYTES.saturating_add(1)),
                "message",
            )
            .await
            .is_err()
    );
    assert!(
        plugin
            .notify(
                "title",
                "x".repeat(MAX_PLUGIN_NOTIFICATION_MESSAGE_BYTES.saturating_add(1)),
            )
            .await
            .is_err()
    );
    assert_eq!(
        sink.events.lock().expect("events").len(),
        before_denials,
        "rejected inputs must never reach the actor log"
    );

    let wires = sink
        .events
        .lock()
        .expect("events")
        .iter()
        .map(|event| event.wire.clone())
        .collect::<Vec<_>>();
    let queued = wires
            .iter()
            .position(|event| matches!(event, EngineEvent::MessageQueued { content, .. } if content == "/help [REDACTED]"))
            .expect("queued event");
    let injected = wires
            .iter()
            .position(|event| matches!(event, EngineEvent::PluginMessageInjected { plugin_id, content, queued: true, .. } if plugin_id == "fixture-plugin" && content == "/help [REDACTED]"))
            .expect("injection audit event");
    let status = wires
            .iter()
            .position(|event| matches!(event, EngineEvent::PluginStatusChanged { plugin_id, status, .. } if plugin_id == "fixture-plugin" && status == "working [REDACTED]"))
            .expect("status event");
    let notification = wires
            .iter()
            .position(|event| matches!(event, EngineEvent::UiNotification { plugin_id, title, message, .. } if plugin_id == "fixture-plugin" && title == "fixture" && message == "notice [REDACTED]"))
            .expect("notification event");
    assert!(queued < injected && injected < status && status < notification);
    let first_sequence = wires[queued].meta().expect("queued metadata").sequence_id.0;
    assert_eq!(
        [queued, injected, status, notification].map(|index| wires[index]
            .meta()
            .expect("durable metadata")
            .sequence_id
            .0),
        [
            first_sequence,
            first_sequence.saturating_add(1),
            first_sequence.saturating_add(2),
            first_sequence.saturating_add(3),
        ]
    );
    assert!(
        wires
            .iter()
            .all(|event| { !matches!(event, EngineEvent::DriverChanged { .. }) })
    );
    assert!(matches!(
        wires.first(),
        Some(EngineEvent::SessionCreated {
            driver_client_id: ClientId(driver),
            ..
        }) if driver == "tui"
    ));

    assert_eq!(
        handle
            .dispatch(ClientCommand::Interrupt {
                meta: protocol_meta("tui", "interrupt-first"),
                session_id: session_id.clone(),
            })
            .await
            .expect("interrupt first turn"),
        CommandOutcome::Accepted
    );
    timeout(Duration::from_secs(3), async {
        loop {
            let processed = sink.events.lock().expect("events").iter().any(|event| {
                matches!(
                    &event.wire,
                    EngineEvent::UserMessageAccepted { content, .. }
                        if content == "/help [REDACTED]"
                )
            });
            if processed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("queued injection must start through normal sequencing");
    let final_wires = sink
        .events
        .lock()
        .expect("events")
        .iter()
        .map(|event| event.wire.clone())
        .collect::<Vec<_>>();
    assert!(final_wires.iter().all(|event| {
        !matches!(event, EngineEvent::CommandFinished { name, .. } if name == "help")
    }));
    let recovered = project_session_events(&final_wires).expect("project plugin events");
    assert_eq!(recovered.driver_client_id, Some(ClientId("tui".to_owned())));

    let _ = handle
        .dispatch(ClientCommand::Interrupt {
            meta: protocol_meta("tui", "interrupt-second"),
            session_id,
        })
        .await;
}
