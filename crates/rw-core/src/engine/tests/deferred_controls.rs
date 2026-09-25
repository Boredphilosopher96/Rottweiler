use super::fixtures::{
    models::PendingModel,
    support::{config, next_matching, protocol_meta},
};
use crate::engine::{builtin_hook_dispatcher, pending_event::PendingEvent};
use rw_tools::ToolRegistry;
use rw_types::{
    ClientCommand, ClientId, ClientRole, CommandOutcome, ModeId, SessionControlOutcome, SessionId,
    config::PermissionDecision,
};
use std::sync::Arc;

#[tokio::test]
async fn queued_mode_applies_after_interrupted_turn_and_reports_original_request() {
    let root = tempfile::tempdir().expect("root");
    let handle = super::fixtures::history::spawn(config(
        root.path(),
        Arc::new(PendingModel),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let session_id = SessionId("fixture-session".into());
    handle
        .dispatch(ClientCommand::AttachSession {
            meta: protocol_meta("local", "attach"),
            session_id: session_id.clone(),
            last_seen_sequence: None,
            role: ClientRole::Driver,
        })
        .await
        .expect("driver");
    let mut events = handle
        .subscribe_client(ClientId("local".into()), None)
        .expect("events");
    handle.send_message("keep running").await.expect("turn");
    assert_eq!(
        handle
            .dispatch(ClientCommand::SwitchMode {
                meta: protocol_meta("local", "queued-plan"),
                session_id,
                mode: ModeId("plan".into())
            })
            .await
            .expect("queued"),
        CommandOutcome::Accepted {}
    );
    let queued = next_matching(&mut events, |event| matches!(event, PendingEvent::SessionControlQueueChanged { controls, settlement: None } if controls.len() == 1)).await;
    assert!(
        matches!(queued.kind, PendingEvent::SessionControlQueueChanged { controls, .. } if controls[0].request.request_id.0 == "queued-plan")
    );
    assert_ne!(
        handle.snapshot().await.expect("busy snapshot").mode_id.0,
        "plan"
    );
    handle.interrupt().await.expect("interrupt");
    let settled = next_matching(&mut events, |event| {
        matches!(
            event,
            PendingEvent::SessionControlQueueChanged {
                settlement: Some(_),
                ..
            }
        )
    })
    .await;
    assert!(
        matches!(settled.kind, PendingEvent::SessionControlQueueChanged { controls, settlement: Some(settled) } if controls.is_empty() && settled.request.request_id.0 == "queued-plan" && settled.outcome == SessionControlOutcome::Applied)
    );
    assert_eq!(
        handle.snapshot().await.expect("applied snapshot").mode_id.0,
        "plan"
    );
    handle.close().await.expect("close");
}

#[tokio::test]
async fn control_queue_is_bounded_and_driver_takeover_cancels_owned_requests() {
    let root = tempfile::tempdir().expect("root");
    let handle = super::fixtures::history::spawn(config(
        root.path(),
        Arc::new(PendingModel),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let session_id = SessionId("fixture-session".into());
    handle
        .dispatch(ClientCommand::AttachSession {
            meta: protocol_meta("local", "attach"),
            session_id: session_id.clone(),
            last_seen_sequence: None,
            role: ClientRole::Driver,
        })
        .await
        .expect("driver");
    let mut events = handle.subscribe().expect("events");
    handle.send_message("keep running").await.expect("turn");
    for index in 0..=rw_types::MAX_QUEUED_SESSION_CONTROLS {
        let outcome = handle
            .dispatch(ClientCommand::SwitchMode {
                meta: protocol_meta("local", &format!("queued-{index}")),
                session_id: session_id.clone(),
                mode: ModeId("plan".into()),
            })
            .await
            .expect("queue reply");
        if index < rw_types::MAX_QUEUED_SESSION_CONTROLS {
            assert_eq!(outcome, CommandOutcome::Accepted {});
        } else {
            assert!(matches!(outcome, CommandOutcome::Rejected { .. }));
        }
    }
    handle
        .dispatch(ClientCommand::TakeDriver {
            meta: protocol_meta("replacement", "take"),
            session_id,
        })
        .await
        .expect("takeover");
    for _ in 0..rw_types::MAX_QUEUED_SESSION_CONTROLS {
        let settled = next_matching(&mut events, |event| {
            matches!(
                event,
                PendingEvent::SessionControlQueueChanged {
                    settlement: Some(_),
                    ..
                }
            )
        })
        .await;
        assert!(
            matches!(settled.kind, PendingEvent::SessionControlQueueChanged { settlement: Some(settled), .. } if settled.outcome == SessionControlOutcome::Cancelled)
        );
    }
    assert_ne!(handle.snapshot().await.expect("snapshot").mode_id.0, "plan");
    handle.close().await.expect("close");
}

struct RecordingPreferences {
    fail: bool,
    saved: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl crate::engine::ModelSelectionPreferences for RecordingPreferences {
    async fn persist(&self, model: &str) -> Result<(), crate::AgentLoopError> {
        self.saved.lock().expect("saved models").push(model.into());
        if self.fail {
            Err(crate::AgentLoopError::Persistence(
                "fixture save failure".into(),
            ))
        } else {
            Ok(())
        }
    }
}

async fn queued_model_at_context_question(
    root: &std::path::Path,
    preferences: Arc<RecordingPreferences>,
) -> (
    crate::SessionHandle,
    crate::engine::SessionSubscription,
    rw_types::QuestionId,
) {
    let mut actor_config = config(
        root,
        Arc::new(PendingModel),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.model_preferences = Some(preferences);
    let handle = super::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    handle
        .dispatch(ClientCommand::AttachSession {
            meta: protocol_meta("local", "attach"),
            session_id: SessionId("fixture-session".into()),
            last_seen_sequence: None,
            role: ClientRole::Driver,
        })
        .await
        .expect("driver");
    let mut events = handle.subscribe().expect("events");
    handle.send_message("keep running").await.expect("turn");
    assert!(
        !handle
            .dispatch_model_control(ClientCommand::SwitchModel {
                meta: protocol_meta("local", "queued-model"),
                session_id: SessionId("fixture-session".into()),
                model: rw_types::ModelAlias("next-model".into()),
                provider: None,
            })
            .await
            .expect("queued model")
    );
    handle.interrupt().await.expect("interrupt");
    let question = next_matching(&mut events, |event| matches!(event, PendingEvent::QuestionAsked { question, .. } if question.model_switch.is_some())).await;
    let PendingEvent::QuestionAsked { question_id, .. } = question.kind else {
        unreachable!()
    };
    (handle, events, question_id)
}

#[tokio::test]
async fn queued_model_waits_for_context_answer_and_preference_save_before_settlement() {
    for fail in [false, true] {
        let root = tempfile::tempdir().expect("root");
        let preferences = Arc::new(RecordingPreferences {
            fail,
            saved: std::sync::Mutex::new(Vec::new()),
        });
        let (handle, mut events, question_id) =
            queued_model_at_context_question(root.path(), preferences.clone()).await;
        assert!(preferences.saved.lock().expect("saved models").is_empty());
        assert_ne!(
            handle
                .snapshot()
                .await
                .expect("pending snapshot")
                .model_alias,
            "next-model"
        );
        assert!(
            !handle
                .dispatch_model_control(ClientCommand::AnswerQuestion {
                    meta: protocol_meta("local", "answer-model"),
                    session_id: SessionId("fixture-session".into()),
                    question_id: question_id.clone(),
                    answer: rw_types::Answer {
                        question_id,
                        value: "pass_full_context".into()
                    },
                })
                .await
                .expect("answer")
        );
        let settlement = next_matching(&mut events, |event| {
            matches!(
                event,
                PendingEvent::SessionControlQueueChanged {
                    settlement: Some(_),
                    ..
                }
            )
        })
        .await;
        let PendingEvent::SessionControlQueueChanged {
            controls,
            settlement: Some(settled),
        } = settlement.kind
        else {
            unreachable!()
        };
        assert!(controls.is_empty());
        assert_eq!(settled.request.request_id.0, "queued-model");
        assert_eq!(
            settled.outcome,
            if fail {
                SessionControlOutcome::Failed
            } else {
                SessionControlOutcome::Applied
            }
        );
        if fail {
            assert!(settled.message.contains("saving its default failed"));
        }
        assert_eq!(
            *preferences.saved.lock().expect("saved models"),
            vec!["next-model"]
        );
        assert_eq!(
            handle
                .snapshot()
                .await
                .expect("applied snapshot")
                .model_alias,
            "next-model"
        );
        handle.close().await.expect("close");
    }
}

#[tokio::test]
async fn driver_loss_cancels_queued_model_context_question_without_saving_default() {
    let root = tempfile::tempdir().expect("root");
    let preferences = Arc::new(RecordingPreferences {
        fail: false,
        saved: std::sync::Mutex::new(Vec::new()),
    });
    let (handle, mut events, question_id) =
        queued_model_at_context_question(root.path(), preferences.clone()).await;
    handle
        .dispatch(ClientCommand::TakeDriver {
            meta: protocol_meta("replacement", "take"),
            session_id: SessionId("fixture-session".into()),
        })
        .await
        .expect("takeover");
    let settlement = next_matching(&mut events, |event| {
        matches!(
            event,
            PendingEvent::SessionControlQueueChanged {
                settlement: Some(_),
                ..
            }
        )
    })
    .await;
    let PendingEvent::SessionControlQueueChanged {
        controls,
        settlement: Some(settled),
    } = settlement.kind
    else {
        unreachable!()
    };
    assert!(controls.is_empty());
    assert_eq!(settled.outcome, SessionControlOutcome::Cancelled);
    assert_eq!(settled.cancelled_question.as_ref(), Some(&question_id));
    assert!(preferences.saved.lock().expect("saved models").is_empty());
    assert_ne!(
        handle.snapshot().await.expect("snapshot").model_alias,
        "next-model"
    );
    assert!(matches!(
        handle
            .dispatch(ClientCommand::AnswerQuestion {
                meta: protocol_meta("replacement", "answer-retired"),
                session_id: SessionId("fixture-session".into()),
                question_id: question_id.clone(),
                answer: rw_types::Answer {
                    question_id,
                    value: "pass_full_context".into()
                },
            })
            .await
            .expect("retired answer"),
        CommandOutcome::Rejected { .. }
    ));
    handle.close().await.expect("close");
}
