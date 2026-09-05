#![cfg(test)]

use crate::PermissionGate;
use crate::engine::AgentTurnStatus;
use crate::engine::MessageDisposition;
use crate::engine::SessionUsage;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::SessionProjectionError;
use crate::engine::projection::project_session_events;
use crate::engine::projection::project_session_events_with_modes;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::hooks::PermissionAllowHook;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::protocol_meta;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::tool_script;
use crate::engine::tests::fixtures::support::wire_event;
use crate::engine::tests::fixtures::support::wire_mode;
use crate::engine::tests::fixtures::support::workspace_tree_bytes;
use crate::engine::tests::fixtures::tools::PlanMutationTripwire;
use crate::engine::unavailable_cost;
use rw_ext::HookDispatcher;
use rw_ext::HookEvent;
use rw_ext::HookRegistration;
use rw_ext::ModeRegistry;
use rw_providers::FinishReason;
use rw_providers::ProviderEvent;
use rw_tools::ToolLimits;
use rw_tools::ToolRegistry;
use rw_tools::WriteTool;
use rw_types::ClientCommand;
use rw_types::ClientRole;
use rw_types::CommandOutcome;
use rw_types::EngineEvent;
use rw_types::ModeId;
use rw_types::SessionId;
use rw_types::SessionMode;
use rw_types::ToolCapability;
use rw_types::ToolOutput;
use rw_types::config::PermissionDecision;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
async fn discuss_and_plan_tool_sequences_cannot_mutate_the_workspace() {
    for mode in [SessionMode::Discuss, SessionMode::Plan] {
        let root = TempDir::new().expect("workspace");
        let model = Arc::new(ScriptedModel::new([
            tool_script(
                &[(
                    "write-1",
                    "write",
                    json!({"path": "forbidden.txt", "content": "must not exist"}),
                )],
                &[],
            ),
            stop_script("done", &[]),
        ]));
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(WriteTool::new(ToolLimits::default())))
            .expect("write tool");
        let handle = SessionActor::spawn(config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            HookDispatcher::new(),
        ))
        .expect("actor");
        let mut events = handle.subscribe().expect("subscription");
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("driver", "attach"),
                session_id: SessionId("fixture-session".to_owned()),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach");
        assert_eq!(
            handle
                .dispatch(ClientCommand::SwitchMode {
                    meta: protocol_meta("driver", "mode"),
                    session_id: SessionId("fixture-session".to_owned()),
                    mode: wire_mode(mode),
                })
                .await
                .expect("switch mode"),
            CommandOutcome::Accepted {}
        );
        assert_eq!(
            handle
                .dispatch(ClientCommand::SendMessage {
                    meta: protocol_meta("driver", "turn"),
                    session_id: SessionId("fixture-session".to_owned()),
                    content: "try mutation".to_owned(),
                    attachments: Vec::new(),
                })
                .await
                .expect("turn"),
            CommandOutcome::Accepted {}
        );
        let turn = collect_turn(&mut events).await;
        assert!(turn.iter().any(|event| matches!(
            &event.kind,
            PendingEvent::ToolCallFinished { is_error: true, output: ToolOutput::Text { text }, .. }
                if text.contains("permission denied")
        )));
        assert!(!root.path().join("forbidden.txt").exists());
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn declarative_custom_mode_is_prompted_filtered_enforced_and_replayable() {
    let root = TempDir::new().expect("workspace");
    let model = Arc::new(ScriptedModel::new([stop_script("audited", &[])]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(rw_tools::ReadTool::new(ToolLimits::default())))
        .expect("read tool");
    tools
        .register(Arc::new(WriteTool::new(ToolLimits::default())))
        .expect("write tool");
    let custom = rw_ext::parse_mode_toml(
        "audit.toml",
        r#"
id = "audit"
description = "Read-only audit"
permission = "discuss"
prompt = "CUSTOM AUDIT MODE: inspect evidence and do not mutate."
allowed-tools = ["read"]
"#,
        rw_ext::ModeSource::Embedded {
            name: "test".to_owned(),
        },
    )
    .expect("custom mode");
    let mut mode_registry = ModeRegistry::builtins().expect("built-in modes");
    mode_registry
        .register(custom)
        .expect("register custom mode");
    mode_registry
        .register(
            rw_ext::parse_mode_toml(
                "ship.toml",
                r#"
id = "ship"
description = "Execute through a custom id"
permission = "execute"
prompt = "Execute approved work."
"#,
                rw_ext::ModeSource::Embedded {
                    name: "test".to_owned(),
                },
            )
            .expect("custom execute mode"),
        )
        .expect("register custom execute mode");
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(tools),
        PermissionDecision::Allow,
        HookDispatcher::new(),
    );
    actor_config.modes = Arc::new(mode_registry);
    let sink = Arc::new(RecordingSink::default());
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.ensure_local_driver().await.expect("local driver");
    assert_eq!(
        handle
            .dispatch(ClientCommand::SwitchMode {
                meta: protocol_meta("local", "enter-plan"),
                session_id: SessionId("fixture-session".to_owned()),
                mode: ModeId("plan".to_owned()),
            })
            .await
            .expect("enter plan"),
        CommandOutcome::Accepted {}
    );
    assert!(matches!(
        handle
            .dispatch(ClientCommand::SwitchMode {
                meta: protocol_meta("local", "custom-execute-before-approval"),
                session_id: SessionId("fixture-session".to_owned()),
                mode: ModeId("ship".to_owned()),
            })
            .await
            .expect("custom execute rejected"),
        CommandOutcome::Rejected { error } if error.code == "plan_approval_required"
    ));
    assert_eq!(
        handle
            .dispatch(ClientCommand::SwitchMode {
                meta: protocol_meta("local", "custom-audit"),
                session_id: SessionId("fixture-session".to_owned()),
                mode: ModeId("audit".to_owned()),
            })
            .await
            .expect("switch custom mode"),
        CommandOutcome::Accepted {}
    );
    assert_eq!(
        handle.send_message("inspect").await.expect("turn"),
        MessageDisposition::Started
    );
    collect_turn(&mut events).await;

    let requests = model.requests.lock().expect("requests");
    let request = requests.first().expect("provider request");
    let wire = serde_json::to_string(request).expect("request wire");
    assert!(wire.contains("CUSTOM AUDIT MODE"), "request: {wire}");
    assert!(wire.contains("\"name\":\"read\""));
    assert!(!wire.contains("\"name\":\"write\""));
    drop(requests);

    let durable = sink.events.lock().expect("events");
    assert!(durable.iter().any(|event| matches!(
        &event.wire,
        EngineEvent::ModeChanged { mode, .. } if mode.0 == "audit"
    )));
    let projected = project_session_events(
        &durable
            .iter()
            .map(|event| event.wire.clone())
            .collect::<Vec<_>>(),
    )
    .expect("projection");
    assert_eq!(projected.mode_id, Some(ModeId("audit".to_owned())));
}

#[test]
fn custom_mode_projection_preserves_permission_floor_and_rewind_state() {
    let custom = rw_ext::parse_mode_toml(
        "audit.toml",
        r#"
id = "audit"
description = "Read-only audit"
permission = "discuss"
prompt = "Inspect without mutation."
"#,
        rw_ext::ModeSource::Embedded {
            name: "test".to_owned(),
        },
    )
    .expect("custom mode");
    let mut modes = ModeRegistry::builtins().expect("built-ins");
    modes.register(custom).expect("custom mode registry");
    let kinds = vec![
        PendingEvent::ModeChanged {
            mode: ModeId("audit".to_owned()),
            definition_fingerprint: modes
                .get("audit")
                .expect("audit mode")
                .semantic_fingerprint(),
        },
        PendingEvent::TurnStarted { turn: 1 },
        PendingEvent::TurnFinished {
            turn: 1,
            status: AgentTurnStatus::Completed,
            usage: SessionUsage::default(),
            cost: unavailable_cost(),
        },
        PendingEvent::ModeChanged {
            mode: ModeId("execute".to_owned()),
            definition_fingerprint: modes
                .get("execute")
                .expect("execute mode")
                .semantic_fingerprint(),
        },
        PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "rewind-mode".to_owned(),
            unrestorable_paths: Vec::new(),
        },
    ];
    let events = kinds
        .into_iter()
        .enumerate()
        .map(|(sequence, kind)| wire_event(u64::try_from(sequence).expect("sequence"), kind))
        .collect::<Vec<_>>();
    let recovered =
        project_session_events_with_modes(&events, &modes).expect("registry-aware projection");
    assert_eq!(recovered.mode_id, Some(ModeId("audit".to_owned())));
    assert_eq!(recovered.mode, SessionMode::Discuss);
}

#[test]
fn custom_plan_projection_is_fail_closed_and_requires_registered_definition() {
    let custom = rw_ext::parse_mode_toml(
        "design.toml",
        r#"
id = "design"
description = "Custom planning"
permission = "plan"
prompt = "Produce a plan."
"#,
        rw_ext::ModeSource::Embedded {
            name: "test".to_owned(),
        },
    )
    .expect("custom mode");
    let mut modes = ModeRegistry::builtins().expect("built-ins");
    modes.register(custom).expect("custom mode registry");
    let events = vec![wire_event(
        0,
        PendingEvent::ModeChanged {
            mode: ModeId("design".to_owned()),
            definition_fingerprint: modes
                .get("design")
                .expect("design mode")
                .semantic_fingerprint(),
        },
    )];
    let recovered =
        project_session_events_with_modes(&events, &modes).expect("custom plan projection");
    assert_eq!(recovered.mode, SessionMode::Plan);
    assert!(recovered.plan_gate_active);

    let builtins = ModeRegistry::builtins().expect("built-ins");
    assert_eq!(
        project_session_events_with_modes(&events, &builtins),
        Err(SessionProjectionError::InvalidMode("design".to_owned()))
    );

    let changed = rw_ext::parse_mode_toml(
        "design.toml",
        r#"
id = "design"
description = "Custom planning"
permission = "execute"
prompt = "The file changed after the mode was selected."
"#,
        rw_ext::ModeSource::Embedded {
            name: "changed".to_owned(),
        },
    )
    .expect("changed custom mode");
    let mut changed_modes = ModeRegistry::builtins().expect("built-ins");
    changed_modes.register(changed).expect("changed registry");
    assert_eq!(
        project_session_events_with_modes(&events, &changed_modes),
        Err(SessionProjectionError::ModeDefinitionChanged(
            "design".to_owned()
        ))
    );

    let stale_custom = vec![wire_event(
        0,
        PendingEvent::ModeChanged {
            mode: ModeId("design".to_owned()),
            definition_fingerprint: "stale-fingerprint".to_owned(),
        },
    )];
    assert_eq!(
        project_session_events_with_modes(&stale_custom, &modes),
        Err(SessionProjectionError::ModeDefinitionChanged(
            "design".to_owned()
        ))
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn seeded_plan_mode_property_keeps_complete_workspace_byte_identical() {
    for seed in 0_u64..48 {
        for hook_allow in [false, true] {
            let root = TempDir::new().expect("workspace");
            std::fs::create_dir(root.path().join("nested")).expect("nested fixture");
            std::fs::write(root.path().join("nested/original.bin"), [0, 1, 2, 255])
                .expect("baseline fixture");
            std::fs::create_dir(root.path().join(".git")).expect("git metadata fixture");
            std::fs::write(root.path().join(".git/index"), seed.to_le_bytes())
                .expect("git index fixture");
            let before = workspace_tree_bytes(root.path());

            let mut value = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let count = usize::try_from(value % 12 + 1).expect("bounded count");
            let mut calls = Vec::with_capacity(count);
            let names = ["write", "edit", "multi_edit", "bash", "network_tool"];
            for index in 0..count {
                value = value
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let name = names[usize::try_from(value % 5).expect("bounded choice")];
                let arguments = match name {
                    "bash" => json!({
                        "command": format!("printf mutated > generated-{index}"),
                        "cwd": if value & 1 == 0 { "." } else { "nested" },
                        "env": {"PATH": format!("/seed/{value}")},
                        "network_domains": if value & 2 != 0 {
                            vec![format!("seed-{value}.invalid")]
                        } else {
                            Vec::new()
                        },
                    }),
                    "network_tool" => json!({
                        "url": format!("https://seed-{value}.invalid"),
                        "body": format!("write generated-{index}"),
                    }),
                    _ => json!({
                        "path": format!("nested/generated-{index}.txt"),
                        "content": format!("seed={seed}; value={value}"),
                        "edits": [{"old": "original", "new": "mutated"}],
                    }),
                };
                calls.push((format!("call-{index}"), name.to_owned(), arguments));
            }
            let mut script = vec![Ok(ProviderEvent::MessageStart {
                model: "fixture-model".to_owned(),
            })];
            for (id, name, arguments) in &calls {
                script.push(Ok(ProviderEvent::ToolCallStart {
                    id: id.clone(),
                    name: name.clone(),
                }));
                script.push(Ok(ProviderEvent::ToolCallEnd {
                    id: id.clone(),
                    arguments: arguments.clone(),
                }));
            }
            script.push(Ok(ProviderEvent::Finished {
                reason: FinishReason::ToolCalls,
            }));
            let model = Arc::new(ScriptedModel::new([
                script,
                stop_script("plan sequence denied", &[]),
            ]));
            let mut tools = ToolRegistry::new();
            for (name, capabilities) in [
                ("write", vec![ToolCapability::WriteFilesystem]),
                ("edit", vec![ToolCapability::WriteFilesystem]),
                ("multi_edit", vec![ToolCapability::WriteFilesystem]),
                (
                    "bash",
                    vec![
                        ToolCapability::ReadFilesystem,
                        ToolCapability::WriteFilesystem,
                        ToolCapability::Execute,
                        ToolCapability::Network,
                    ],
                ),
                (
                    "network_tool",
                    vec![ToolCapability::Network, ToolCapability::Execute],
                ),
            ] {
                tools
                    .register(Arc::new(PlanMutationTripwire::new(name, capabilities)))
                    .expect("tripwire tool");
            }
            let mut hooks = builtin_hook_dispatcher().expect("built-in hooks");
            if hook_allow {
                hooks
                    .register(
                        HookRegistration::new("test.allow-permission", HookEvent::PermissionCheck)
                            .with_priority(i32::MAX),
                        PermissionAllowHook,
                    )
                    .expect("permission allow hook");
            }
            let mut actor_config = config(
                root.path(),
                model,
                Arc::new(tools),
                PermissionDecision::Allow,
                hooks,
            );
            actor_config.permissions = Arc::new(if hook_allow {
                PermissionGate::new(PermissionDecision::Ask)
            } else {
                PermissionGate::for_headless_mode(rw_types::PermissionModeDescriptor::Yolo)
            });
            let handle = SessionActor::spawn(actor_config).expect("actor");
            let mut events = handle.subscribe().expect("subscription");
            handle
                .dispatch(ClientCommand::AttachSession {
                    meta: protocol_meta("property", "attach"),
                    session_id: SessionId("fixture-session".to_owned()),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                })
                .await
                .expect("attach");
            handle
                .dispatch(ClientCommand::SwitchMode {
                    meta: protocol_meta("property", "plan-mode"),
                    session_id: SessionId("fixture-session".to_owned()),
                    mode: wire_mode(SessionMode::Plan),
                })
                .await
                .expect("plan mode");
            handle
                .dispatch(ClientCommand::SendMessage {
                    meta: protocol_meta("property", "turn"),
                    session_id: SessionId("fixture-session".to_owned()),
                    content: "exercise arbitrary plan tools".to_owned(),
                    attachments: Vec::new(),
                })
                .await
                .expect("property turn");
            let turn = collect_turn(&mut events).await;
            assert_eq!(
                turn.iter()
                    .filter(|event| matches!(
                        event.kind,
                        PendingEvent::ToolCallFinished { is_error: true, .. }
                    ))
                    .count(),
                calls.len(),
                "seed={seed}, hook_allow={hook_allow}"
            );
            assert_eq!(
                workspace_tree_bytes(root.path()),
                before,
                "Plan mutated workspace for seed={seed}, hook_allow={hook_allow}"
            );
        }
    }
}
