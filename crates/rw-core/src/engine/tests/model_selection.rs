#![cfg(test)]

use crate::engine::builtin_hook_dispatcher;
use crate::engine::durability::NoopSessionEventSink;
use crate::engine::model_context_transfer_value;
use crate::engine::model_switch_question;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::project_session_events;
use crate::engine::session::SessionHandle;
use crate::engine::tests::fixtures::models::M3Model;
use crate::engine::tests::fixtures::support::TestEventSinkExt;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::protocol_meta;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::text_turn;
use crate::engine::tests::fixtures::support::wire_event;
use rw_tools::ToolRegistry;
use rw_types::Answer;
use rw_types::Block;
use rw_types::ClientCommand;
use rw_types::ClientRole;
use rw_types::CommandOutcome;
use rw_types::EngineEvent;
use rw_types::ModelAlias;
use rw_types::ModelContextTransfer;
use rw_types::QuestionId;
use rw_types::Role;
use rw_types::SessionId;
use rw_types::config::PermissionDecision;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn model_switch_context_choices_are_explicit_and_reach_the_provider_boundary() {
    async fn attach(handle: &SessionHandle, request: &str) {
        assert_eq!(
            handle
                .dispatch(ClientCommand::AttachSession {
                    meta: protocol_meta("driver", request),
                    session_id: SessionId("fixture-session".to_owned()),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                })
                .await
                .expect("attach"),
            CommandOutcome::Accepted {}
        );
    }

    async fn request_switch(handle: &SessionHandle, request: &str) -> QuestionId {
        assert_eq!(
            handle
                .dispatch(ClientCommand::SwitchModel {
                    meta: protocol_meta("driver", request),
                    session_id: SessionId("fixture-session".to_owned()),
                    model: ModelAlias("slow".to_owned()),
                    provider: None,
                })
                .await
                .expect("switch model"),
            CommandOutcome::Accepted {}
        );
        handle
            .event_sink
            .test_events_after(None)
            .await
            .expect("switch events")
            .into_iter()
            .find_map(|event| match event {
                EngineEvent::QuestionAsked {
                    question_id,
                    question,
                    ..
                } if question
                    .model_switch
                    .as_ref()
                    .is_some_and(|target| target.model.0 == "slow") =>
                {
                    Some(question_id)
                }
                _ => None,
            })
            .expect("model context question")
    }

    async fn answer_switch(
        handle: &SessionHandle,
        question_id: QuestionId,
        strategy: ModelContextTransfer,
        request: &str,
    ) {
        let mut events = handle.subscribe().expect("subscription");
        assert_eq!(
            handle
                .dispatch(ClientCommand::AnswerQuestion {
                    meta: protocol_meta("driver", request),
                    session_id: SessionId("fixture-session".to_owned()),
                    question_id: question_id.clone(),
                    answer: Answer {
                        question_id,
                        value: model_context_transfer_value(strategy).to_owned(),
                    },
                })
                .await
                .expect("answer model context question"),
            CommandOutcome::Accepted {}
        );
        next_matching(&mut events, |event| {
            matches!(
                event,
                PendingEvent::ModelChanged { model, .. } if model.0 == "slow"
            )
        })
        .await;
    }

    let original = vec![
        text_turn(Role::System, "stable system policy"),
        text_turn(Role::User, "original user context"),
        text_turn(Role::Assistant, "original assistant context"),
    ];

    let root = TempDir::new().expect("summary workspace");
    let summary_model = Arc::new(M3Model::new([
        stop_script("durable handoff summary", &[]),
        stop_script("continued after handoff", &[]),
    ]));
    let mut summary_config = config(
        root.path(),
        summary_model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    summary_config.recovered.conversation = original.clone();
    let summary_handle = crate::engine::tests::fixtures::history::spawn(summary_config)
        .await
        .expect("summary actor");
    attach(&summary_handle, "attach-summary").await;
    let summary_question = request_switch(&summary_handle, "switch-summary").await;
    assert_eq!(summary_model.operations(), Vec::<String>::new());
    assert_eq!(
        summary_handle
            .snapshot()
            .await
            .expect("pending summary snapshot")
            .model_alias,
        "fast"
    );
    answer_switch(
        &summary_handle,
        summary_question,
        ModelContextTransfer::PassSummary,
        "answer-summary",
    )
    .await;
    assert_eq!(
        summary_model.operations(),
        ["prepare:fast", "stream:fast", "prepare:slow"]
    );
    let summary_snapshot = summary_handle
        .snapshot()
        .await
        .expect("summary switch snapshot");
    assert_eq!(summary_snapshot.model_alias, "slow");
    let summary_context = summary_handle
        .dump_prompt(None)
        .await
        .expect("summary context");
    assert!(summary_context.turns.iter().any(|turn| {
            turn.meta.summary
                && matches!(turn.blocks.as_slice(), [Block::Text { text }] if text == "durable handoff summary")
        }));
    let compacted =
        serde_json::to_string(&summary_context.turns).expect("serialize compacted conversation");
    assert!(!compacted.contains("original user context"));
    assert!(!compacted.contains("original assistant context"));
    let mut summary_events = summary_handle.subscribe().expect("subscription");
    assert_eq!(
        summary_handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "continue-summary"),
                session_id: SessionId("fixture-session".to_owned()),
                content: "continue on selected model".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("continue after summary"),
        CommandOutcome::Accepted {}
    );
    collect_turn(&mut summary_events).await;
    let summary_requests = summary_model.requests();
    assert_eq!(summary_requests.len(), 2);
    let compaction_prompt =
        serde_json::to_string(&summary_requests[0].turns).expect("compaction prompt");
    assert!(compaction_prompt.contains("original user context"));
    let selected_model_prompt =
        serde_json::to_string(&summary_requests[1].turns).expect("selected model prompt");
    assert!(selected_model_prompt.contains("durable handoff summary"));
    assert!(selected_model_prompt.contains("continue on selected model"));
    assert!(!selected_model_prompt.contains("original user context"));
    assert!(!selected_model_prompt.contains("original assistant context"));

    let root = TempDir::new().expect("full workspace");
    let full_model = Arc::new(M3Model::new([stop_script("full context received", &[])]));
    let mut full_config = config(
        root.path(),
        full_model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    full_config.recovered.conversation = original.clone();
    let full_handle = crate::engine::tests::fixtures::history::spawn(full_config)
        .await
        .expect("full actor");
    attach(&full_handle, "attach-full").await;
    let full_question = request_switch(&full_handle, "switch-full").await;
    assert_eq!(full_model.operations(), Vec::<String>::new());
    answer_switch(
        &full_handle,
        full_question,
        ModelContextTransfer::PassFullContext,
        "answer-full",
    )
    .await;
    assert_eq!(full_model.operations(), ["prepare:slow"]);
    assert_eq!(
        full_handle
            .dump_prompt(None)
            .await
            .expect("full context")
            .turns,
        original
    );
    let mut full_events = full_handle.subscribe().expect("subscription");
    assert_eq!(
        full_handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "continue-full"),
                session_id: SessionId("fixture-session".to_owned()),
                content: "continue with full context".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("continue with full context"),
        CommandOutcome::Accepted {}
    );
    collect_turn(&mut full_events).await;
    let full_prompt =
        serde_json::to_string(&full_model.requests()[0].turns).expect("serialize full prompt");
    assert!(full_prompt.contains("original user context"));
    assert!(full_prompt.contains("original assistant context"));

    let root = TempDir::new().expect("fresh workspace");
    let fresh_model = Arc::new(M3Model::new([stop_script("fresh context received", &[])]));
    let mut fresh_config = config(
        root.path(),
        fresh_model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    fresh_config.recovered.conversation = original.clone();
    let fresh_handle = crate::engine::tests::fixtures::history::spawn(fresh_config)
        .await
        .expect("fresh actor");
    attach(&fresh_handle, "attach-fresh").await;
    let fresh_question = request_switch(&fresh_handle, "switch-fresh").await;
    assert_eq!(fresh_model.operations(), Vec::<String>::new());
    answer_switch(
        &fresh_handle,
        fresh_question,
        ModelContextTransfer::StartWithoutContext,
        "answer-fresh",
    )
    .await;
    assert_eq!(fresh_model.operations(), ["prepare:slow"]);
    assert_eq!(
        fresh_handle
            .dump_prompt(None)
            .await
            .expect("fresh context")
            .turns,
        vec![original[0].clone()]
    );
    let mut fresh_events = fresh_handle.subscribe().expect("subscription");
    assert_eq!(
        fresh_handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "continue-fresh"),
                session_id: SessionId("fixture-session".to_owned()),
                content: "continue without inherited context".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("continue without context"),
        CommandOutcome::Accepted {}
    );
    collect_turn(&mut fresh_events).await;
    let fresh_prompt =
        serde_json::to_string(&fresh_model.requests()[0].turns).expect("serialize fresh prompt");
    assert!(fresh_prompt.contains("stable system policy"));
    assert!(fresh_prompt.contains("continue without inherited context"));
    assert!(!fresh_prompt.contains("original user context"));
    assert!(!fresh_prompt.contains("original assistant context"));
}

#[tokio::test]
async fn pending_model_switch_question_recovers_and_can_be_answered() {
    let original = vec![
        text_turn(Role::System, "system policy"),
        text_turn(Role::User, "durable prior context"),
    ];
    let question_id = QuestionId("model-switch-recovered".to_owned());
    let mut events = prior_model_context(&original[0]);
    events.push(wire_event(
        3,
        PendingEvent::QuestionAsked {
            turn: 1,
            question_id: question_id.clone(),
            question: model_switch_question(
                question_id.clone(),
                ModelAlias("slow".to_owned()),
                None,
            ),
        },
    ));
    let recovered = project_session_events(&events).expect("project pending model question");
    assert!(recovered.pending_questions.contains_key(&question_id.0));
    let sink = Arc::new(NoopSessionEventSink::default());
    for event in events {
        crate::commit_session_events(Arc::clone(&sink), vec![event])
            .await
            .expect("seed recovered journal");
    }

    let root = TempDir::new().expect("recovery workspace");
    let mut actor_config = config(
        root.path(),
        Arc::new(M3Model::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.recovered = recovered;
    actor_config.event_sink = sink;
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("recovered actor");
    assert_eq!(
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("driver", "attach-recovered"),
                session_id: SessionId("fixture-session".to_owned()),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach recovered"),
        CommandOutcome::Accepted {}
    );
    let mut subscription = handle.subscribe().expect("subscription");
    assert_eq!(
        handle
            .dispatch(ClientCommand::AnswerQuestion {
                meta: protocol_meta("driver", "answer-recovered"),
                session_id: SessionId("fixture-session".to_owned()),
                question_id: question_id.clone(),
                answer: Answer {
                    question_id,
                    value: "pass_full_context".to_owned(),
                },
            })
            .await
            .expect("answer recovered question"),
        CommandOutcome::Accepted {}
    );
    next_matching(
        &mut subscription,
        |event| matches!(event, PendingEvent::ModelChanged { model, .. } if model.0 == "slow"),
    )
    .await;
    let snapshot = handle.snapshot().await.expect("recovered switch snapshot");
    assert_eq!(snapshot.model_alias, "slow");
    assert_eq!(
        handle
            .dump_prompt(None)
            .await
            .expect("preserved context")
            .turns,
        original
    );
}

fn prior_model_context(system: &rw_types::Turn) -> Vec<EngineEvent> {
    vec![
        wire_event(
            0,
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: system.clone(),
            },
        ),
        wire_event(
            1,
            PendingEvent::UserMessageAccepted {
                turn: 1,
                content: "durable prior context".into(),
                attachments: vec![],
            },
        ),
        wire_event(
            2,
            PendingEvent::ConversationInputCommitted {
                agent_turn: 1,
                accepted_source: rw_types::SequenceId(1),
                selection: rw_types::conversation_input::InputSelection::Accepted {},
            },
        ),
    ]
}
