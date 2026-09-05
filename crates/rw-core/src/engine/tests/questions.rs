#![cfg(test)]

use crate::engine::builtin_hook_dispatcher;
use crate::engine::projection::project_session_events;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::controllers::PanicQuestionAsker;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::sinks::ToggleLeaseSink;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::protocol_meta;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::tool_script;
use rw_tools::AskUserTool;
use rw_tools::ToolLimits;
use rw_tools::ToolRegistry;
use rw_types::Answer;
use rw_types::Block;
use rw_types::ClientCommand;
use rw_types::ClientId;
use rw_types::ClientRole;
use rw_types::CommandOutcome;
use rw_types::EngineEvent;
use rw_types::RequestId;
use rw_types::SessionId;
use rw_types::ToolOutput;
use rw_types::ToolOutputPart;
use rw_types::config::PermissionDecision;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn ask_user_is_persisted_and_answered_only_through_client_command() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[(
                "question-call",
                "ask_user",
                json!({"question": "Continue?", "options": ["yes", "no"]}),
            )],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(AskUserTool::new(
            Arc::new(PanicQuestionAsker),
            ToolLimits::default(),
        )))
        .expect("ask tool");
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle
        .subscribe_client(ClientId("driver".to_owned()), None)
        .expect("subscription");
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
    handle
        .dispatch(ClientCommand::SendMessage {
            meta: protocol_meta("driver", "send-question"),
            session_id: session_id.clone(),
            content: "ask".to_owned(),
            attachments: Vec::new(),
        })
        .await
        .expect("send");
    let question_id = loop {
        if let EngineEvent::QuestionAsked {
            meta,
            question_id,
            questions,
            ..
        } = events.recv().await.expect("question event")
        {
            assert_eq!(meta.caused_by, Some(RequestId("send-question".to_owned())));
            assert_eq!(questions[0].prompt, "Continue?");
            break question_id;
        }
    };
    let asked_prefix = sink
        .events
        .lock()
        .expect("events")
        .iter()
        .map(|event| event.wire.clone())
        .collect::<Vec<_>>();
    let asked_projection = project_session_events(&asked_prefix).expect("project asked question");
    assert!(
        asked_projection
            .pending_questions
            .contains_key(&question_id.0)
    );
    assert_eq!(
        handle
            .dispatch(ClientCommand::AnswerQuestion {
                meta: protocol_meta("driver", "answer"),
                session_id,
                question_id: question_id.clone(),
                answers: vec![Answer {
                    question_id,
                    values: vec!["yes".to_owned()],
                }],
            })
            .await
            .expect("answer"),
        CommandOutcome::Accepted
    );
    let mut durable_answer = false;
    loop {
        let event = events.recv().await.expect("terminal event");
        if let EngineEvent::QuestionAnswered { meta, answers, .. } = &event {
            assert_eq!(meta.caused_by, Some(RequestId("answer".to_owned())));
            assert_eq!(answers[0].values, ["yes"]);
            durable_answer = true;
        }
        if matches!(event, EngineEvent::TurnFinished { .. }) {
            break;
        }
    }
    assert!(durable_answer);
    let answered_log = sink
        .events
        .lock()
        .expect("events")
        .iter()
        .map(|event| event.wire.clone())
        .collect::<Vec<_>>();
    assert!(
        project_session_events(&answered_log)
            .expect("project answered question")
            .pending_questions
            .is_empty()
    );
    let snapshot = handle.snapshot().await.expect("snapshot");
    assert!(snapshot.conversation.iter().any(|turn| {
        turn.blocks.iter().any(|block| {
            matches!(
                block,
                Block::ToolResult {
                    output: ToolOutput::Mixed { parts },
                    ..
                } if parts.iter().any(|part| matches!(
                    part,
                    ToolOutputPart::Text { text } if text == "yes"
                ))
            )
        })
    }));
}

#[tokio::test]
async fn question_answer_persistence_failure_rejects_ack_and_stops_tool_continuation() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([tool_script(
        &[(
            "question-call",
            "ask_user",
            json!({"question": "Continue?", "options": ["yes", "no"]}),
        )],
        &[],
    )]));
    let sink = Arc::new(ToggleLeaseSink::default());
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(AskUserTool::new(
            Arc::new(PanicQuestionAsker),
            ToolLimits::default(),
        )))
        .expect("ask tool");
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let session_id = SessionId("fixture-session".to_owned());
    let mut events = handle
        .subscribe_client(ClientId("driver".to_owned()), None)
        .expect("subscription");
    handle
        .dispatch(ClientCommand::AttachSession {
            meta: protocol_meta("driver", "attach"),
            session_id: session_id.clone(),
            last_seen_sequence: None,
            role: ClientRole::Driver,
        })
        .await
        .expect("attach");
    handle
        .dispatch(ClientCommand::SendMessage {
            meta: protocol_meta("driver", "send"),
            session_id: session_id.clone(),
            content: "ask".to_owned(),
            attachments: Vec::new(),
        })
        .await
        .expect("send");
    let question_id = loop {
        if let EngineEvent::QuestionAsked { question_id, .. } =
            events.recv().await.expect("question")
        {
            break question_id;
        }
    };
    sink.fail_question_answer.store(true, Ordering::SeqCst);
    assert!(matches!(
        handle
            .dispatch(ClientCommand::AnswerQuestion {
                meta: protocol_meta("driver", "failed-answer"),
                session_id,
                question_id: question_id.clone(),
                answers: vec![Answer {
                    question_id,
                    values: vec!["yes".to_owned()],
                }],
            })
            .await
            .expect("answer outcome"),
        CommandOutcome::Rejected { .. }
    ));
    assert!(
        sink.events
            .lock()
            .expect("events")
            .iter()
            .all(|event| !matches!(event, EngineEvent::QuestionAnswered { .. }))
    );
    timeout(Duration::from_secs(1), async {
        loop {
            if !handle.snapshot().await.expect("snapshot").running {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled question turn");
    assert_eq!(model.request_count(), 1);
}
