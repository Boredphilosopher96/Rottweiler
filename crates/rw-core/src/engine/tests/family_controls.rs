#![cfg(test)]
use super::fixtures::{
    controllers::PanicQuestionAsker,
    history,
    models::ScriptedModel,
    support::{config, protocol_meta, stop_script, tool_script},
};
use crate::{SessionHandle, engine::builtin_hook_dispatcher};
use rw_tools::{AskUserTool, ToolLimits, ToolRegistry};
use rw_types::{
    Answer, ClientCommand, ClientId, ClientRole, CommandOutcome, EngineEvent,
    config::PermissionDecision, family_controls::ChildControlResponse,
};
use std::sync::Arc;

async fn actor(path: &std::path::Path, name: &str, ask: bool) -> SessionHandle {
    let script = if ask {
        vec![
            tool_script(
                &[(
                    "question",
                    "ask_user",
                    serde_json::json!({"question":"Proceed?","options":["yes","no"]}),
                )],
                &[],
            ),
            stop_script("done", &[]),
        ]
    } else {
        vec![stop_script("ready", &[])]
    };
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(AskUserTool::new(
            Arc::new(PanicQuestionAsker),
            ToolLimits::default(),
        )))
        .expect("tool");
    let mut config = config(
        path,
        Arc::new(ScriptedModel::new(script)),
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    config.session_id = rw_types::SessionId(name.into());
    let handle = history::spawn(config).await.expect("actor");
    handle
        .dispatch(ClientCommand::AttachSession {
            meta: protocol_meta(&format!("{name}-driver"), "attach"),
            session_id: handle.session_id().clone(),
            role: ClientRole::Driver,
            last_seen_sequence: None,
        })
        .await
        .expect("driver");
    handle
}
#[tokio::test]
async fn explicit_root_driver_answers_exact_child_question_without_changing_child_driver() {
    let dir = tempfile::tempdir().expect("root");
    let root = actor(dir.path(), "root-control", false).await;
    let child = actor(dir.path(), "child-control", true).await;
    let mut events = child.subscribe_live().expect("events");
    child
        .dispatch(ClientCommand::SendMessage {
            meta: protocol_meta("child-control-driver", "ask"),
            session_id: child.session_id().clone(),
            content: "ask".into(),
            attachments: vec![],
        })
        .await
        .expect("send");
    let mut started = None;
    let question_id = loop {
        match events.recv().await.expect("event") {
            EngineEvent::TurnStarted { meta, turn_id, .. } => {
                started = Some((meta.sequence_id, turn_id));
            }
            EngineEvent::QuestionAsked { question_id, .. } => break question_id,
            _ => {}
        }
    };
    let state = child.live_state().await.expect("child state");
    let active = state.active_turn.as_ref().expect("active question turn");
    let (source, turn_id) = started.expect("durable turn start");
    assert_eq!(active.turn_id, turn_id);
    assert_eq!(active.started, Some(source));
    let before = child.child_controls().await.expect("snapshot");
    assert_eq!(before.snapshot.controls.questions.len(), 1);
    assert_eq!(child.control_summary().questions, 1);
    assert!(
        root.family_control_authority(&ClientId("observer".into()))
            .is_err()
    );
    let response = ChildControlResponse::Question {
        question_id: question_id.clone(),
        answers: vec![Answer {
            question_id,
            values: vec!["yes".into()],
        }],
    };
    let authority = root
        .family_control_authority(&ClientId("root-control-driver".into()))
        .expect("root proof");
    let stale = child
        .respond_child_control(
            authority.clone(),
            protocol_meta("root-control-driver", "stale"),
            rw_types::SequenceId(before.revision.0.wrapping_add(1)),
            response.clone(),
        )
        .await
        .expect("rejected");
    assert!(!matches!(stale, CommandOutcome::Accepted {}));
    assert_eq!(
        child
            .child_controls()
            .await
            .expect("still pending")
            .snapshot
            .controls
            .questions
            .len(),
        1
    );
    let outcome = child
        .respond_child_control(
            authority,
            protocol_meta("root-control-driver", "answer-child"),
            before.revision,
            response,
        )
        .await
        .expect("answer");
    assert!(matches!(outcome, CommandOutcome::Accepted {}));
    let after = child.child_controls().await.expect("resolved");
    assert!(after.snapshot.controls.questions.is_empty());
    assert_ne!(after.revision, before.revision);
    // The response capability does not attach or take a lease in the child.
    assert!(
        child
            .family_control_authority(&ClientId("child-control-driver".into()))
            .is_ok()
    );
    child.close().await.expect("close child");
    root.close().await.expect("close root");
}
