#![cfg(test)]

use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::ContextSurgeryAction;
use crate::engine::projection::plan_review_context_turn;
use crate::engine::projection::project_session_events;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::protocol_meta;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::tool_script;
use crate::engine::tests::fixtures::support::wire_event;
use crate::engine::tests::fixtures::support::wire_mode;
use rw_ext::HookDispatcher;
use rw_tools::SubmitPlanTool;
use rw_tools::ToolRegistry;
use rw_types::Block;
use rw_types::ClientCommand;
use rw_types::ClientRole;
use rw_types::CommandOutcome;
use rw_types::ContextItemId;
use rw_types::PlanArtifact;
use rw_types::PlanDecision;
use rw_types::SessionId;
use rw_types::SessionMode;
use rw_types::config::PermissionDecision;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn plan_submission_requires_review_and_pins_approved_artifact() {
    let root = TempDir::new().expect("workspace");
    let artifact = PlanArtifact {
        title: "Safe change".to_owned(),
        summary_md: "Implement after approval.".to_owned(),
        steps: vec![rw_types::PlanStep {
            description: "Change one file".to_owned(),
            files_touched: vec!["src/lib.rs".to_owned()],
            verification: "cargo test".to_owned(),
        }],
        open_questions: Vec::new(),
    };
    let artifact_b = PlanArtifact {
        title: "Second plan".to_owned(),
        summary_md: "A new approval cycle.".to_owned(),
        steps: vec![rw_types::PlanStep {
            description: "Change another file".to_owned(),
            files_touched: vec!["src/second.rs".to_owned()],
            verification: "cargo test".to_owned(),
        }],
        open_questions: Vec::new(),
    };
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[(
                "plan-1",
                "submit_plan",
                serde_json::to_value(&artifact).expect("artifact value"),
            )],
            &[],
        ),
        stop_script("awaiting approval", &[]),
        tool_script(
            &[(
                "plan-2",
                "submit_plan",
                serde_json::to_value(&artifact_b).expect("second artifact value"),
            )],
            &[],
        ),
        stop_script("awaiting second approval", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(SubmitPlanTool))
        .expect("submit plan tool");
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        HookDispatcher::new(),
    ))
    .await
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
    handle
        .dispatch(ClientCommand::SwitchMode {
            meta: protocol_meta("driver", "plan-mode"),
            session_id: SessionId("fixture-session".to_owned()),
            mode: wire_mode(SessionMode::Plan),
        })
        .await
        .expect("plan mode");
    assert_eq!(
        handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "turn"),
                session_id: SessionId("fixture-session".to_owned()),
                content: "make a plan".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("turn"),
        CommandOutcome::Accepted {}
    );
    let turn = collect_turn(&mut events).await;
    assert!(turn.iter().any(|event| matches!(
        &event.kind,
        PendingEvent::PlanSubmitted { artifact: submitted } if submitted == &artifact
    )));
    assert!(matches!(
        handle
            .dispatch(ClientCommand::SwitchMode {
                meta: protocol_meta("driver", "execute-too-early"),
                session_id: SessionId("fixture-session".to_owned()),
                mode: wire_mode(SessionMode::Execute),
            })
            .await
            .expect("early execute"),
        CommandOutcome::Rejected { error } if error.code == "plan_approval_required"
    ));
    assert_eq!(
        handle
            .dispatch(ClientCommand::ApprovePlan {
                meta: protocol_meta("driver", "approve-plan"),
                session_id: SessionId("fixture-session".to_owned()),
                decision: PlanDecision::Approve,
                revisions: None,
            })
            .await
            .expect("review"),
        CommandOutcome::Accepted {}
    );
    let snapshot = handle.snapshot().await.expect("snapshot");
    assert_eq!(snapshot.mode, SessionMode::Execute);
    assert_eq!(snapshot.approved_plan, Some(artifact.clone()));
    assert!(snapshot.pending_plan.is_none());
    let context = handle
        .dump_prompt(None)
        .await
        .expect("approved plan context");
    assert!(context.turns.last().is_some_and(|turn| matches!(
        turn.blocks.as_slice(),
        [Block::Text { text }] if text.contains("Approved plan artifact")
    )));

    assert_eq!(
        handle
            .dispatch(ClientCommand::SwitchMode {
                meta: protocol_meta("driver", "second-plan-mode"),
                session_id: SessionId("fixture-session".to_owned()),
                mode: wire_mode(SessionMode::Plan),
            })
            .await
            .expect("second plan mode"),
        CommandOutcome::Accepted {}
    );
    assert!(
        handle
            .snapshot()
            .await
            .expect("second cycle")
            .plan_gate_active
    );
    for (request, intermediate) in [
        ("second-direct-execute", None),
        ("second-discuss-bypass", Some(SessionMode::Discuss)),
    ] {
        if let Some(intermediate) = intermediate {
            assert_eq!(
                handle
                    .dispatch(ClientCommand::SwitchMode {
                        meta: protocol_meta("driver", "second-discuss"),
                        session_id: SessionId("fixture-session".to_owned()),
                        mode: wire_mode(intermediate),
                    })
                    .await
                    .expect("discuss"),
                CommandOutcome::Accepted {}
            );
        }
        assert!(matches!(
            handle
                .dispatch(ClientCommand::SwitchMode {
                    meta: protocol_meta("driver", request),
                    session_id: SessionId("fixture-session".to_owned()),
                    mode: wire_mode(SessionMode::Execute),
                })
                .await
                .expect("blocked execute"),
            CommandOutcome::Rejected { error } if error.code == "plan_approval_required"
        ));
    }
    handle
        .dispatch(ClientCommand::SwitchMode {
            meta: protocol_meta("driver", "return-to-plan"),
            session_id: SessionId("fixture-session".to_owned()),
            mode: wire_mode(SessionMode::Plan),
        })
        .await
        .expect("return plan");
    handle
        .dispatch(ClientCommand::SendMessage {
            meta: protocol_meta("driver", "second-plan-turn"),
            session_id: SessionId("fixture-session".to_owned()),
            content: "make another plan".to_owned(),
            attachments: Vec::new(),
        })
        .await
        .expect("second plan turn");
    collect_turn(&mut events).await;
    assert_eq!(
        handle
            .dispatch(ClientCommand::ApprovePlan {
                meta: protocol_meta("driver", "approve-second-plan"),
                session_id: SessionId("fixture-session".to_owned()),
                decision: PlanDecision::Approve,
                revisions: None,
            })
            .await
            .expect("approve second plan"),
        CommandOutcome::Accepted {}
    );
    let second = handle.snapshot().await.expect("second approved snapshot");
    assert_eq!(second.mode, SessionMode::Execute);
    assert!(!second.plan_gate_active);
    assert_eq!(second.approved_plan, Some(artifact_b));
}

#[test]
fn mode_and_approved_plan_project_durably_with_conversation_pin() {
    let artifact = PlanArtifact {
        title: "Durable plan".to_owned(),
        summary_md: "Survives restart and compaction.".to_owned(),
        steps: vec![rw_types::PlanStep {
            description: "Implement".to_owned(),
            files_touched: Vec::new(),
            verification: "test".to_owned(),
        }],
        open_questions: Vec::new(),
    };
    let context =
        plan_review_context_turn(&artifact, PlanDecision::Approve, None).expect("approved context");
    let item_id = ContextItemId("conversation:0".to_owned());
    let kinds = vec![
        PendingEvent::ModeChanged {
            mode: wire_mode(SessionMode::Plan),
            definition_fingerprint: "fixture".to_owned(),
        },
        PendingEvent::PlanSubmitted {
            artifact: artifact.clone(),
        },
        PendingEvent::PlanReviewed {
            artifact: artifact.clone(),
            decision: PlanDecision::Approve,
            revisions: None,
        },
        PendingEvent::ConversationTurnCommitted {
            agent_turn: 0,
            turn: context.clone(),
        },
        PendingEvent::ContextItemPinned {
            item_id: item_id.clone(),
            effective_after_agent_turn: 0,
        },
        PendingEvent::ModeChanged {
            mode: wire_mode(SessionMode::Execute),
            definition_fingerprint: "fixture".to_owned(),
        },
    ];
    let events = kinds
        .into_iter()
        .enumerate()
        .map(|(sequence, kind)| wire_event(u64::try_from(sequence).expect("sequence"), kind))
        .collect::<Vec<_>>();
    let recovered = project_session_events(&events).expect("project mode and plan");
    assert_eq!(recovered.mode, SessionMode::Execute);
    assert!(!recovered.plan_gate_active);
    assert_eq!(recovered.pending_plan, None);
    assert_eq!(recovered.approved_plan, Some(artifact));
    assert_eq!(recovered.conversation, vec![context]);
    assert_eq!(
        recovered.context_surgery,
        vec![ContextSurgeryAction {
            item_id,
            pinned: true,
            effective_after_agent_turn: 0,
        }]
    );
    let mut next_cycle = events;
    next_cycle.push(wire_event(
        6,
        PendingEvent::ModeChanged {
            mode: wire_mode(SessionMode::Plan),
            definition_fingerprint: "fixture".to_owned(),
        },
    ));
    next_cycle.push(wire_event(
        7,
        PendingEvent::ModeChanged {
            mode: wire_mode(SessionMode::Discuss),
            definition_fingerprint: "fixture".to_owned(),
        },
    ));
    let resumed = project_session_events(&next_cycle).expect("resume second plan cycle");
    assert_eq!(resumed.mode, SessionMode::Discuss);
    assert!(resumed.plan_gate_active);
    assert!(resumed.approved_plan.is_none());
}

#[tokio::test]
async fn permission_mode_projects_and_is_reapplied_when_a_session_resumes() {
    let durable = vec![wire_event(
        0,
        PendingEvent::PermissionModeChanged {
            mode: Some(rw_types::PermissionModeDescriptor::Yolo),
        },
    )];
    let recovered = project_session_events(&durable).expect("project permission mode");
    assert_eq!(
        recovered.permission_mode,
        Some(rw_types::PermissionModeDescriptor::Yolo)
    );

    let root = TempDir::new().expect("workspace");
    let actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::new([])),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Ask,
        HookDispatcher::new(),
    );
    crate::commit_session_events(Arc::clone(&actor_config.event_sink), durable)
        .await
        .expect("seed permission source");
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("resume actor");
    assert_eq!(
        handle.snapshot().await.expect("snapshot").permission_mode,
        Some(rw_types::PermissionModeDescriptor::Yolo)
    );
}
