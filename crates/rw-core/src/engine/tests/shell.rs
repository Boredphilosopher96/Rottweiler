#![cfg(test)]

use crate::engine::MAX_CAPTURED_SHELL_OUTPUT_BYTES;
use crate::engine::projection::project_session_events;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::models::AliasVisionModel;
use crate::engine::tests::fixtures::support::ShellSecretRedactor;
use crate::engine::tests::fixtures::support::TestEventSinkExt;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::protocol_meta;
use rw_ext::HookDispatcher;
use rw_tools::ToolRegistry;
use rw_types::Answer;
use rw_types::Block;
use rw_types::ClientCommand;
use rw_types::ClientRole;
use rw_types::CommandOutcome;
use rw_types::EngineEvent;
use rw_types::ModelAlias;
use rw_types::ModelContextTransfer;
use rw_types::SessionId;
use rw_types::ShellId;
use rw_types::config::PermissionDecision;
use rw_types::config::ThinkingLevel;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn shell_gate_and_model_alias_are_durable_and_fail_closed() {
    let root = TempDir::new().expect("workspace");
    let mut actor_config = config(
        root.path(),
        Arc::new(AliasVisionModel),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Ask,
        HookDispatcher::new(),
    );
    actor_config.secret_redactor = Arc::new(ShellSecretRedactor);
    let handle = SessionActor::spawn(actor_config).expect("actor");
    assert_eq!(
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("driver", "attach"),
                session_id: SessionId("fixture-session".to_owned()),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach"),
        CommandOutcome::Accepted {}
    );
    assert_eq!(
        handle
            .dispatch(ClientCommand::UserShellStarted {
                meta: protocol_meta("driver", "shell-start"),
                session_id: SessionId("fixture-session".to_owned()),
                command: "python".to_owned(),
            })
            .await
            .expect("shell start"),
        CommandOutcome::Accepted {}
    );
    let active = handle
        .snapshot()
        .await
        .expect("active shell snapshot")
        .active_shell
        .expect("active shell");
    assert!(matches!(
        handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "blocked-turn"),
                session_id: SessionId("fixture-session".to_owned()),
                content: "must wait".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("blocked turn"),
        CommandOutcome::Rejected { error } if error.code == "user_shell_active"
    ));
    assert!(matches!(
        handle
            .dispatch(ClientCommand::UserShellEnded {
                meta: protocol_meta("driver", "wrong-shell-end"),
                session_id: SessionId("fixture-session".to_owned()),
                shell_id: ShellId("wrong".to_owned()),
                status: 0,
                captured_output: None,
            })
            .await
            .expect("wrong shell end"),
        CommandOutcome::Rejected { error } if error.code == "shell_end_rejected"
    ));
    let durable = handle
        .event_sink
        .test_events_after(None)
        .await
        .expect("durable shell start");
    let recovered = project_session_events(&durable).expect("project shell gate");
    assert_eq!(recovered.active_shell.as_ref(), Some(&active));

    assert!(
        handle
            .complete_user_shell(ShellId("stale".to_owned()), 0, None)
            .await
            .is_err()
    );
    handle
        .complete_user_shell(
            active.shell_id,
            130,
            Some(format!(
                "COLLAPSE:{}",
                "SHELL_SECRET".repeat(MAX_CAPTURED_SHELL_OUTPUT_BYTES / 8)
            )),
        )
        .await
        .expect("trusted broker shell end");
    let ended = handle.snapshot().await.expect("ended shell");
    assert!(ended.active_shell.is_none());
    let shell_context = ended.conversation.last().expect("shell model context");
    assert!(matches!(
        shell_context.blocks.as_slice(),
        [Block::Text { text }]
            if text.contains("useful [REDACTED] output")
                && !text.contains("SHELL_SECRET")
    ));
    let durable = handle
        .event_sink
        .test_events_after(None)
        .await
        .expect("durable redacted shell end");
    assert!(durable.iter().any(|event| matches!(
        event,
        EngineEvent::UserShellStateChanged {
            active: false,
            captured_output: Some(output),
            ..
        } if output == "useful [REDACTED] output"
    )));
    let resumed = project_session_events(&durable).expect("project redacted shell output");
    assert_eq!(resumed.conversation.last(), Some(shell_context));
    assert!(matches!(
        handle
            .dispatch(ClientCommand::SwitchModel {
                meta: protocol_meta("driver", "unknown-model"),
                session_id: SessionId("fixture-session".to_owned()),
                model: ModelAlias("missing".to_owned()),
                provider: None,
            })
            .await
            .expect("unknown model"),
        CommandOutcome::Rejected { error } if error.code == "unknown_model_alias"
    ));
    assert_eq!(
        handle
            .dispatch(ClientCommand::SwitchModel {
                meta: protocol_meta("driver", "switch-model"),
                session_id: SessionId("fixture-session".to_owned()),
                model: ModelAlias("slow".to_owned()),
                provider: None,
            })
            .await
            .expect("switch model"),
        CommandOutcome::Accepted {}
    );
    let durable = handle
        .event_sink
        .test_events_after(None)
        .await
        .expect("durable model switch question");
    let (question_id, question) = durable
        .iter()
        .find_map(|event| match event {
            EngineEvent::QuestionAsked {
                question_id,
                questions,
                ..
            } => questions
                .iter()
                .find(|question| {
                    question
                        .model_switch
                        .as_ref()
                        .is_some_and(|target| target.model == ModelAlias("slow".to_owned()))
                })
                .map(|question| (question_id.clone(), question)),
            _ => None,
        })
        .expect("typed model context question");
    assert_eq!(
        question.options[0].model_context_transfer,
        Some(ModelContextTransfer::PassSummary)
    );
    assert_eq!(question.options[0].label, "Pass summary");
    assert_eq!(
        handle
            .dispatch(ClientCommand::AnswerQuestion {
                meta: protocol_meta("driver", "switch-model-context"),
                session_id: SessionId("fixture-session".to_owned()),
                question_id: question_id.clone(),
                answers: vec![Answer {
                    question_id,
                    values: vec!["start_without_context".to_owned()],
                }],
            })
            .await
            .expect("answer model context question"),
        CommandOutcome::Accepted {}
    );
    assert_eq!(
        handle.snapshot().await.expect("model snapshot").model_alias,
        "slow"
    );
    assert_eq!(
        handle.snapshot().await.expect("thinking snapshot").thinking,
        ThinkingLevel::High
    );
    assert!(matches!(
        handle
            .dispatch(ClientCommand::SwitchModel {
                meta: protocol_meta("driver", "unknown-provider"),
                session_id: SessionId("fixture-session".to_owned()),
                model: ModelAlias("slow".to_owned()),
                provider: Some("missing".to_owned()),
            })
            .await
            .expect("unknown provider"),
        CommandOutcome::Rejected { error } if error.code == "unknown_provider_route"
    ));
    assert_eq!(
        handle
            .dispatch(ClientCommand::SwitchModel {
                meta: protocol_meta("driver", "switch-provider"),
                session_id: SessionId("fixture-session".to_owned()),
                model: ModelAlias("slow".to_owned()),
                provider: Some("offline".to_owned()),
            })
            .await
            .expect("switch provider"),
        CommandOutcome::Accepted {}
    );
    assert_eq!(
        handle
            .snapshot()
            .await
            .expect("provider snapshot")
            .provider
            .as_deref(),
        Some("offline")
    );
    assert_eq!(
        handle
            .dispatch(ClientCommand::SwitchModel {
                meta: protocol_meta("driver", "switch-concrete-model"),
                session_id: SessionId("fixture-session".to_owned()),
                model: ModelAlias("openai/live-model".to_owned()),
                provider: None,
            })
            .await
            .expect("switch concrete model"),
        CommandOutcome::Accepted {}
    );
    let concrete = handle.snapshot().await.expect("concrete model snapshot");
    assert_eq!(concrete.model_alias, "openai/live-model");
    assert_eq!(concrete.thinking, ThinkingLevel::High);
    let durable = handle
        .event_sink
        .test_events_after(None)
        .await
        .expect("durable model switch");
    assert_eq!(
        project_session_events(&durable)
            .expect("project model")
            .model_alias
            .as_deref(),
        Some("openai/live-model")
    );
    assert_eq!(
        project_session_events(&durable)
            .expect("project provider")
            .provider
            .as_deref(),
        None
    );
    assert_eq!(
        project_session_events(&durable)
            .expect("project thinking")
            .thinking,
        Some(ThinkingLevel::High)
    );
}
