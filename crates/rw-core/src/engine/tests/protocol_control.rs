#![cfg(test)]

use crate::PermissionGate;
use crate::PermissionOutcome;
use crate::PermissionRequest;
use crate::engine::MAX_PERMISSION_RULES_PER_SCOPE;
use crate::engine::MessageDisposition;
use crate::engine::approval_diff;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::dispatch;
use crate::engine::dispatch::permission_state;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::project_session_events;
use crate::engine::session;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::controllers::StaticApprover;
use crate::engine::tests::fixtures::models::PendingModel;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::TestEventSinkExt;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::next_permission_state;
use crate::engine::tests::fixtures::support::protocol_meta;
use rw_tools::ToolRegistry;
use rw_types::ApprovalDecision;
use rw_types::ClientCommand;
use rw_types::ClientId;
use rw_types::ClientRole;
use rw_types::CommandAckMeta;
use rw_types::CommandOutcome;
use rw_types::EngineError;
use rw_types::EngineEvent;
use rw_types::EventMeta;
use rw_types::PermissionApprovalScope;
use rw_types::PermissionModeDescriptor;
use rw_types::RequestId;
use rw_types::SessionId;
use rw_types::ToolCapability;
use rw_types::config::PermissionDecision;
use rw_types::config::PermissionRule;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn protocol_ack_lease_observer_and_takeover_are_one_durable_event_stream() {
    let root = TempDir::new().expect("tempdir");
    let handle = SessionActor::spawn(config(
        root.path(),
        Arc::new(PendingModel),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let session_id = SessionId("fixture-session".to_owned());
    let mut driver_events = handle
        .subscribe_client(ClientId("driver".to_owned()), None)
        .expect("subscription");
    assert_eq!(
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("driver", "attach-driver"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach"),
        CommandOutcome::Accepted
    );
    let created = driver_events.recv().await.expect("session created");
    assert!(matches!(
        &created,
        EngineEvent::SessionCreated {
            meta: EventMeta {
                caused_by: Some(RequestId(request)),
                emitted_at,
                ..
            },
            driver_client_id: ClientId(driver),
        } if request == "attach-driver"
            && emitted_at == "2026-01-02T03:04:05.006Z"
            && driver == "driver"
    ));
    assert!(matches!(
        driver_events.recv().await.expect("attach ack"),
        EngineEvent::CommandAcknowledged {
            meta: CommandAckMeta { emitted_at, .. },
            outcome: CommandOutcome::Accepted,
            ..
        } if emitted_at == "2026-01-02T03:04:05.006Z"
    ));

    let mut observer_events = handle
        .subscribe_client(ClientId("observer".to_owned()), None)
        .expect("subscription");
    assert_eq!(
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("observer", "attach-observer"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Observer,
            })
            .await
            .expect("observer attach"),
        CommandOutcome::Accepted
    );
    assert!(matches!(
        observer_events.recv().await.expect("observer durable gap"),
        EngineEvent::SessionCreated { .. }
    ));
    assert!(matches!(
        observer_events.recv().await.expect("observer attach ack"),
        EngineEvent::CommandAcknowledged { .. }
    ));
    assert!(matches!(
        handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("observer", "observer-mutation"),
                session_id: session_id.clone(),
                content: "must reject".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("observer rejection"),
        CommandOutcome::Rejected { .. }
    ));
    assert_eq!(
        handle
            .dispatch(ClientCommand::TakeDriver {
                meta: protocol_meta("observer", "take-driver"),
                session_id: session_id.clone(),
            })
            .await
            .expect("take driver"),
        CommandOutcome::Accepted
    );
    let changed = loop {
        let event = observer_events.recv().await.expect("driver changed");
        if matches!(event, EngineEvent::DriverChanged { .. }) {
            break event;
        }
    };
    assert!(matches!(
        changed,
        EngineEvent::DriverChanged {
            meta: EventMeta {
                caused_by: Some(RequestId(ref request)),
                ..
            },
            driver_client_id: ClientId(ref driver),
        } if request == "take-driver" && driver == "observer"
    ));
    assert!(matches!(
        driver_events.recv().await.expect("old driver notification"),
        EngineEvent::DriverChanged { .. }
    ));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn queued_message_mutations_are_durable_broadcast_and_reject_stale_targets() {
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
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let session_id = SessionId("fixture-session".to_owned());
    let mut driver_events = handle
        .subscribe_client(ClientId("local".to_owned()), None)
        .expect("subscription");
    let mut observer_events = handle
        .subscribe_client(ClientId("observer".to_owned()), None)
        .expect("subscription");
    for (client, role) in [
        ("local", ClientRole::Driver),
        ("observer", ClientRole::Observer),
    ] {
        assert_eq!(
            handle
                .dispatch(ClientCommand::AttachSession {
                    meta: protocol_meta(client, &format!("attach-{client}")),
                    session_id: session_id.clone(),
                    last_seen_sequence: None,
                    role,
                })
                .await
                .expect("attach"),
            CommandOutcome::Accepted
        );
    }

    assert_eq!(
        handle
            .send_message("running")
            .await
            .expect("running message"),
        MessageDisposition::Started
    );
    assert_eq!(
        handle.send_message("remove me").await.expect("first queue"),
        MessageDisposition::Queued
    );
    assert_eq!(
        handle.send_message("keep me").await.expect("second queue"),
        MessageDisposition::Queued
    );
    assert_eq!(
        handle
            .snapshot()
            .await
            .expect("queued snapshot")
            .queued_messages,
        ["remove me", "keep me"]
    );

    assert_eq!(
        handle
            .dispatch(ClientCommand::RemoveQueuedMessage {
                meta: protocol_meta("local", "remove-queued"),
                session_id: session_id.clone(),
                position: "1".to_owned(),
            })
            .await
            .expect("remove queued message"),
        CommandOutcome::Accepted
    );
    for receiver in [&mut driver_events, &mut observer_events] {
        let removed = next_matching(receiver, |event| {
            matches!(event, PendingEvent::QueuedMessageRemoved { position: 1 })
        })
        .await;
        assert!(matches!(
            removed.wire,
            EngineEvent::QueuedMessageRemoved {
                meta: EventMeta {
                    caused_by: Some(RequestId(ref request)),
                    ..
                },
                position: 1,
            } if request == "remove-queued"
        ));
    }
    assert_eq!(
        handle
            .snapshot()
            .await
            .expect("removed snapshot")
            .queued_messages,
        ["keep me"]
    );

    let unknown = handle
        .dispatch(ClientCommand::RemoveQueuedMessage {
            meta: protocol_meta("local", "remove-unknown"),
            session_id: session_id.clone(),
            position: "99".to_owned(),
        })
        .await
        .expect("unknown removal outcome");
    assert!(matches!(
        unknown,
        CommandOutcome::Rejected {
            error: EngineError { ref code, .. }
        } if code == "queued_message_not_found"
    ));
    assert_eq!(
        handle.send_message("new tail").await.expect("third queue"),
        MessageDisposition::Queued
    );

    let durable_after_remove = sink
        .test_events_after(None)
        .await
        .expect("durable removal log");
    assert!(durable_after_remove.iter().any(|event| matches!(
        event,
        EngineEvent::MessageQueued {
            position: 3,
            content,
            ..
        } if content == "new tail"
    )));
    let recovered_after_remove =
        project_session_events(&durable_after_remove).expect("recover removed queue");
    assert_eq!(
        recovered_after_remove.queued_messages,
        ["keep me", "new tail"]
    );
    assert_eq!(recovered_after_remove.queued_message_positions, [2, 3]);

    assert_eq!(
        handle
            .dispatch(ClientCommand::ClearQueuedMessages {
                meta: protocol_meta("local", "clear-queued"),
                session_id: session_id.clone(),
            })
            .await
            .expect("clear queued messages"),
        CommandOutcome::Accepted
    );
    for receiver in [&mut driver_events, &mut observer_events] {
        let cleared = next_matching(receiver, |event| {
            matches!(event, PendingEvent::QueuedMessagesCleared)
        })
        .await;
        assert!(matches!(
            cleared.wire,
            EngineEvent::QueuedMessagesCleared {
                meta: EventMeta {
                    caused_by: Some(RequestId(ref request)),
                    ..
                },
            } if request == "clear-queued"
        ));
    }
    assert!(
        handle
            .snapshot()
            .await
            .expect("cleared snapshot")
            .queued_messages
            .is_empty()
    );
    let durable_after_clear = sink
        .test_events_after(None)
        .await
        .expect("durable clear log");
    assert!(
        project_session_events(&durable_after_clear)
            .expect("recover cleared queue")
            .queued_messages
            .is_empty()
    );

    let empty = handle
        .dispatch(ClientCommand::ClearQueuedMessages {
            meta: protocol_meta("local", "clear-empty"),
            session_id,
        })
        .await
        .expect("empty clear outcome");
    assert!(matches!(
        empty,
        CommandOutcome::Rejected {
            error: EngineError { ref code, .. }
        } if code == "queued_messages_empty"
    ));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn typed_permission_inventory_is_observer_safe_and_mutations_are_driver_gated() {
    let root = TempDir::new().expect("tempdir");
    let permissions = Arc::new(
        PermissionGate::from_config(rw_types::config::PermissionConfig {
            default: PermissionDecision::Ask,
            rules: vec![PermissionRule {
                pattern: "bash(rm *)".to_owned(),
                action: PermissionDecision::Deny,
            }],
        })
        .with_workspace_roots([root.path()]),
    );
    permissions
        .add_session_rule(PermissionRule {
            pattern: "bash(cargo test*)".to_owned(),
            action: PermissionDecision::Ask,
        })
        .expect("session rule");
    assert_eq!(
        permissions
            .authorize(
                PermissionRequest {
                    invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
                    id: "remember-session".to_owned(),
                    tool_name: "write".to_owned(),
                    arguments: json!({
                        "path":"secret-never-listed",
                        "content":"private approval payload"
                    }),
                    capabilities: vec![
                        ToolCapability::ReadFilesystem,
                        ToolCapability::WriteFilesystem,
                    ],
                    approval_diff: None,
                },
                &StaticApprover(ApprovalDecision::AllowSession),
            )
            .await,
        PermissionOutcome::Allowed
    );
    let mut actor_config = config(
        root.path(),
        Arc::new(PendingModel),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Ask,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.permissions = Arc::clone(&permissions);
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let session_id = SessionId("fixture-session".to_owned());
    let mut driver_events = handle
        .subscribe_client(ClientId("driver".to_owned()), None)
        .expect("subscription");
    let mut observer_events = handle
        .subscribe_client(ClientId("observer".to_owned()), None)
        .expect("subscription");
    for (client, role) in [
        ("driver", ClientRole::Driver),
        ("observer", ClientRole::Observer),
    ] {
        assert_eq!(
            handle
                .dispatch(ClientCommand::AttachSession {
                    meta: protocol_meta(client, &format!("attach-{client}")),
                    session_id: session_id.clone(),
                    last_seen_sequence: None,
                    role,
                })
                .await
                .expect("attach"),
            CommandOutcome::Accepted
        );
    }

    assert_eq!(
        handle
            .dispatch(ClientCommand::ListPermissions {
                meta: protocol_meta("observer", "observer-list"),
                session_id: session_id.clone(),
            })
            .await
            .expect("observer list"),
        CommandOutcome::Accepted
    );
    let listed = next_permission_state(&mut observer_events).await;
    assert_eq!(listed.default, PermissionDecision::Ask);
    assert_eq!(listed.runtime_mode, None);
    assert_eq!(listed.effective_rules.len(), 1);
    assert!(listed.project_rules.is_empty());
    assert_eq!(listed.session_rules.len(), 1);
    assert_eq!(listed.approvals.len(), 1);
    assert_eq!(listed.approvals[0].scope, PermissionApprovalScope::Session);
    let encoded = serde_json::to_string(&listed).expect("permission inventory JSON");
    assert!(!encoded.contains("runtime_mode"));
    assert!(!encoded.contains("secret-never-listed"));

    assert_eq!(
        handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "driver-mode"),
                session_id: session_id.clone(),
                content: "/permissions mode auto-safe".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("driver permission mode"),
        CommandOutcome::Accepted
    );
    let mode_changed = next_matching(&mut driver_events, |kind| {
        matches!(kind, PendingEvent::PermissionModeChanged { .. })
    })
    .await;
    assert!(matches!(
        mode_changed.kind,
        PendingEvent::PermissionModeChanged {
            mode: Some(rw_types::PermissionModeDescriptor::AutoSafe)
        }
    ));
    assert_eq!(
        handle
            .dispatch(ClientCommand::ListPermissions {
                meta: protocol_meta("driver", "driver-list-mode"),
                session_id: session_id.clone(),
            })
            .await
            .expect("driver list active mode"),
        CommandOutcome::Accepted
    );
    let active_mode = next_permission_state(&mut driver_events).await;
    assert_eq!(
        active_mode.runtime_mode,
        Some(PermissionModeDescriptor::AutoSafe)
    );
    assert!(
        serde_json::to_string(&active_mode)
            .expect("active permission inventory JSON")
            .contains(r#""runtime_mode":"auto-safe""#)
    );

    assert!(matches!(
        handle
            .dispatch(ClientCommand::AddSessionPermissionRule {
                meta: protocol_meta("observer", "observer-add"),
                session_id: session_id.clone(),
                pattern: "write(**)".to_owned(),
                action: PermissionDecision::Allow,
            })
            .await
            .expect("observer mutation"),
        CommandOutcome::Rejected { .. }
    ));
    assert_eq!(permissions.snapshot().session_rules.len(), 1);

    assert_eq!(
        handle
            .dispatch(ClientCommand::AddSessionPermissionRule {
                meta: protocol_meta("driver", "driver-add"),
                session_id: session_id.clone(),
                pattern: "write(**)".to_owned(),
                action: PermissionDecision::Allow,
            })
            .await
            .expect("driver add"),
        CommandOutcome::Accepted
    );
    let added = next_permission_state(&mut driver_events).await;
    let added_rule = added
        .session_rules
        .iter()
        .find(|rule| rule.pattern == "write(**)")
        .expect("typed added row");
    assert_eq!(
        handle
            .dispatch(ClientCommand::RemoveSessionPermissionRule {
                meta: protocol_meta("driver", "driver-remove"),
                session_id: session_id.clone(),
                rule_id: added_rule.id.clone(),
            })
            .await
            .expect("driver remove"),
        CommandOutcome::Accepted
    );
    let removed = next_permission_state(&mut driver_events).await;
    assert!(
        removed
            .session_rules
            .iter()
            .all(|rule| rule.pattern != "write(**)")
    );
    let approval = removed.approvals.first().expect("remembered approval");
    assert_eq!(
        handle
            .dispatch(ClientCommand::RevokePermissionApproval {
                meta: protocol_meta("driver", "driver-revoke"),
                session_id,
                approval_id: approval.id.clone(),
                scope: approval.scope,
            })
            .await
            .expect("driver revoke"),
        CommandOutcome::Accepted
    );
    let revoked = next_permission_state(&mut driver_events).await;
    assert!(revoked.approvals.is_empty());
}

#[test]
fn typed_permission_inventory_is_bounded_and_marks_truncation() {
    let permissions = PermissionGate::new(PermissionDecision::Ask);
    for index in 0..MAX_PERMISSION_RULES_PER_SCOPE + 5 {
        permissions
            .add_session_rule(PermissionRule {
                pattern: format!("bash(command-{index}*)"),
                action: PermissionDecision::Ask,
            })
            .expect("bounded fixture rule");
    }
    let state = permission_state(&permissions);
    assert_eq!(state.session_rules.len(), MAX_PERMISSION_RULES_PER_SCOPE);
    assert!(state.truncated);
}
