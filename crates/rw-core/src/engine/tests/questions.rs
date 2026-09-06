#![cfg(test)]

use crate::engine::builtin_hook_dispatcher;
use crate::engine::projection::project_session_events;
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
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
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
            question,
            ..
        } = events
            .recv()
            .await
            .expect("question event")
            .as_ref()
            .clone()
        {
            assert_eq!(meta.caused_by, Some(RequestId("send-question".to_owned())));
            assert_eq!(question.prompt, "Continue?");
            break question_id;
        }
    };
    let controls = handle
        .controls()
        .await
        .expect("live controls without replay");
    assert_eq!(controls.controls.questions.len(), 1);
    assert_eq!(controls.controls.questions[0].question_id, question_id);
    assert_eq!(controls.controls.questions[0].question.prompt, "Continue?");
    assert!(controls.through.is_some());
    let asked_prefix = sink
        .events
        .lock()
        .expect("events")
        .iter()
        .map(|event| event.wire.clone())
        .collect::<Vec<_>>();
    let live = handle
        .live_state()
        .await
        .expect("live metadata without replay");
    assert_eq!(live.driver_client_id, Some(ClientId("driver".into())));
    let active = live.active_turn.expect("question owns active turn");
    let started = asked_prefix.iter().find_map(|event| match event {
        EngineEvent::TurnStarted { meta, turn_id } if turn_id == &active.turn_id => {
            Some(meta.sequence_id)
        }
        _ => None,
    });
    assert_eq!(active.started, started);
    assert!(started.is_some());
    assert!(live.through >= started);
    let asked_projection = project_session_events(&asked_prefix).expect("project asked question");
    assert!(
        asked_projection
            .pending_questions
            .contains_key(&question_id.0)
    );
    assert!(matches!(
        handle
            .dispatch(ClientCommand::AnswerQuestion {
                meta: protocol_meta("driver", "unlisted option"),
                session_id: session_id.clone(),
                question_id: question_id.clone(),
                answer: Answer {
                    question_id: question_id.clone(),
                    value: "not a displayed option".into()
                },
            })
            .await
            .expect("rejected option"),
        CommandOutcome::Rejected { .. }
    ));
    assert_eq!(
        handle
            .controls()
            .await
            .expect("still pending")
            .controls
            .questions
            .len(),
        1
    );
    assert_eq!(
        handle
            .dispatch(ClientCommand::AnswerQuestion {
                meta: protocol_meta("driver", "answer"),
                session_id,
                question_id: question_id.clone(),
                answer: Answer {
                    question_id,
                    value: "yes".to_owned(),
                },
            })
            .await
            .expect("answer"),
        CommandOutcome::Accepted {}
    );
    assert!(
        handle
            .controls()
            .await
            .expect("answered controls")
            .controls
            .questions
            .is_empty()
    );
    let mut durable_answer = false;
    loop {
        let event = events
            .recv()
            .await
            .expect("terminal event")
            .as_ref()
            .clone();
        if let EngineEvent::QuestionAnswered { meta, answer, .. } = &event {
            assert_eq!(meta.caused_by, Some(RequestId("answer".to_owned())));
            assert_eq!(answer.value, "yes");
            durable_answer = true;
        }
        if matches!(event, EngineEvent::TurnFinished { .. }) {
            break;
        }
    }
    assert!(durable_answer);
    let settled = handle.live_state().await.expect("settled metadata");
    assert!(settled.active_turn.is_none());
    assert_eq!(settled.completed_turns, 1);
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
    let recovered = project_session_events(&answered_log).expect("answered tool context");
    assert!(recovered.conversation.iter().any(|turn| {
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
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
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
            events.recv().await.expect("question").as_ref().clone()
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
                answer: Answer {
                    question_id,
                    value: "yes".to_owned(),
                },
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
    assert_failed_answer_repaired(&handle, &mut events, &sink).await;
    handle
        .close()
        .await
        .expect("physically settled question actor");
    assert_eq!(model.request_count(), 1);
}

async fn assert_failed_answer_repaired(
    handle: &crate::engine::SessionHandle,
    events: &mut crate::engine::SessionSubscription,
    sink: &ToggleLeaseSink,
) {
    // The rejected acknowledgement precedes physical tool settlement. The
    // durable terminal event is the repair fence that makes state readable.
    timeout(Duration::from_secs(1), async {
        loop {
            let event = events.recv().await.expect("repair event");
            if let EngineEvent::TurnFinished {
                turn_id, status, ..
            } = event.as_ref()
            {
                assert_eq!(turn_id.0, "1");
                assert_eq!(*status, rw_types::TurnStatus::Interrupted);
                break;
            }
        }
    })
    .await
    .expect("cancelled question turn");
    let state = handle.snapshot().await.expect("repaired state");
    assert!(!state.running);
    assert_eq!(state.completed_turns, 1);
    assert_eq!(state.driver_client_id, Some(ClientId("driver".to_owned())));
    assert!(
        handle
            .controls()
            .await
            .expect("repaired controls")
            .controls
            .questions
            .is_empty()
    );
    let source = sink.events.lock().expect("durable source");
    assert!(
        source
            .iter()
            .all(|event| !matches!(event, EngineEvent::QuestionAnswered { .. }))
    );
    let completions: Vec<_> = source
        .iter()
        .filter_map(|event| {
            if let EngineEvent::ToolCallFinished {
                tool_call_id,
                is_error,
                ..
            } = event
            {
                Some((tool_call_id, is_error))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].0.0, "question-call");
    assert!(
        *completions[0].1,
        "the rejected answer cannot complete the tool successfully"
    );
    let recovered = project_session_events(&source).expect("canonical repaired state");
    assert_eq!(recovered.completed_turns, 1);
    assert!(recovered.interrupted_turn.is_none());
    assert!(recovered.pending_questions.is_empty());
    assert!(recovered.interrupted_tool_repairs.is_empty());
}
