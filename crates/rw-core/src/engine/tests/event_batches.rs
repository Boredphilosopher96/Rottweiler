#![cfg(test)]

use crate::engine::AgentLoopError;
use crate::engine::AgentTurnStatus;
use crate::engine::MessageDisposition;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::durability::NoopSessionEventSink;
use crate::engine::pending_event::PendingEvent;
use crate::engine::tests::fixtures::hooks::RewriteUserPromptHook;
use crate::engine::tests::fixtures::models::ContinuousDeltaModel;
use crate::engine::tests::fixtures::models::DelayedFinishModel;
use crate::engine::tests::fixtures::models::PendingModel;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::BlockingBatchSink;
use crate::engine::tests::fixtures::sinks::MalformedBatchMode;
use crate::engine::tests::fixtures::sinks::MalformedBatchSink;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::stop_script;
use rw_ext::HookEvent;
use rw_ext::HookRegistration;
use rw_providers::FinishReason;
use rw_providers::ProviderEvent;
use rw_tools::ToolRegistry;
use rw_types::Block;
use rw_types::ClientId;
use rw_types::CommandOutcome;
use rw_types::EngineEvent;
use rw_types::EventMeta;
use rw_types::RequestId;
use rw_types::Role;
use rw_types::Turn;
use rw_types::config::PermissionDecision;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::time::timeout;

#[tokio::test]
async fn opening_batch_is_fully_persisted_before_any_event_is_broadcast() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(BlockingBatchSink {
        should_block: |events| events.len() > 1,
        persisted: Mutex::new(Vec::new()),
        blocked_once: AtomicBool::new(false),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let mut actor_config = config(
        root.path(),
        Arc::new(PendingModel),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    handle.ensure_local_driver().await.expect("local driver");
    let mut events = handle
        .subscribe_client(ClientId("local".to_owned()), Some(0.into()))
        .expect("subscription");
    let sender = handle.clone();
    let send = tokio::spawn(async move { sender.send_message("persist together").await });

    timeout(Duration::from_secs(1), sink.entered.notified())
        .await
        .expect("opening batch reached sink");
    assert_eq!(sink.persisted.lock().expect("persisted events").len(), 1);
    assert!(matches!(
        events.recv().await.expect("command ack").as_ref().clone(),
        EngineEvent::CommandAcknowledged {
            outcome: CommandOutcome::Accepted {},
            ..
        }
    ));
    assert!(
        timeout(Duration::from_millis(20), events.recv())
            .await
            .is_err()
    );

    sink.release.notify_one();
    assert_eq!(
        send.await.expect("send task").expect("send message"),
        MessageDisposition::Started
    );
    let persisted = sink.persisted.lock().expect("persisted events").clone();
    assert!(persisted.len() >= 3);
    assert_eq!(
        persisted[1].meta().expect("event meta").sequence_id,
        1.into()
    );
    assert_eq!(
        persisted[2].meta().expect("event meta").sequence_id,
        2.into()
    );
    assert!(matches!(persisted[1], EngineEvent::TurnStarted { .. }));
    assert!(matches!(
        &persisted[2],
        EngineEvent::UserMessageAccepted { agent_turn: 1, content, .. }
            if content == "persist together"
    ));
    assert_eq!(
        events.recv().await.expect("started event").as_ref().clone(),
        persisted[1]
    );
    assert_eq!(
        events
            .recv()
            .await
            .expect("accepted event")
            .as_ref()
            .clone(),
        persisted[2]
    );

    assert!(handle.interrupt().await.expect("cleanup interrupt"));
    collect_turn(&mut events).await;
}

#[tokio::test]
async fn malformed_batch_payload_or_sequence_is_rejected_before_broadcast_or_model_work() {
    for mode in [MalformedBatchMode::Payload, MalformedBatchMode::Sequence] {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([stop_script("unused", &[])]));
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = Arc::new(MalformedBatchSink {
            mode,
            inner: Arc::new(NoopSessionEventSink::default()),
        });
        let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
            .await
            .expect("actor");
        handle.ensure_local_driver().await.expect("local driver");
        let mut events = handle
            .subscribe_client(ClientId("local".to_owned()), Some(0.into()))
            .expect("subscription");

        assert!(matches!(
            handle.send_message("reject malformed batch").await,
            Err(AgentLoopError::Persistence(_))
        ));
        assert_eq!(model.request_count(), 0);
        assert!(matches!(
            events.recv().await.expect("command ack").as_ref().clone(),
            EngineEvent::CommandAcknowledged {
                outcome: CommandOutcome::Accepted {},
                ..
            }
        ));
        let failure = events
            .recv()
            .await
            .expect("caused-by failure event")
            .as_ref()
            .clone();
        assert!(matches!(
            failure,
            EngineEvent::Error {
                meta: EventMeta {
                    caused_by: Some(RequestId(ref request)),
                    ..
                },
                ..
            } if request == "local-1"
        ));
        assert!(
            timeout(Duration::from_millis(20), events.recv())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn successful_single_delta_batches_delta_commit_and_finish() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::new([stop_script("terminal", &[])])),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.recovered.title = Some("batch fixture".to_owned());
    actor_config.event_sink = sink.clone();
    let actor_config = crate::engine::tests::fixtures::history::bind(actor_config)
        .await
        .expect("seed canonical source");
    let initial_events = sink.events.lock().expect("seed events").len();
    let initial_batches = sink.batch_sizes.lock().expect("seed batches").len();
    let handle = crate::engine::SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");

    handle.send_message("run").await.expect("message");
    let observed = collect_turn(&mut events).await;
    let snapshot = handle.snapshot().await.expect("snapshot");
    assert_eq!(snapshot.conversation_turns, 2);
    assert!(matches!(
        &handle.dump_prompt(None).await.expect("committed user context").turns[0],
        Turn {
            role: Role::User,
            blocks,
            ..
        } if matches!(blocks.as_slice(), [Block::Text { text }] if text == "run")
    ));
    let deltas = observed
        .iter()
        .filter_map(|event| match &event.kind {
            PendingEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deltas, ["terminal"]);
    assert_eq!(
        &sink.batch_sizes.lock().expect("batch sizes")[initial_batches..],
        &[1, 3, 1, 1, 3]
    );
    let persisted_guard = sink.events.lock().expect("event sink lock");
    let persisted = &persisted_guard[initial_events..];
    assert!(matches!(
        persisted[1].kind,
        PendingEvent::TurnStarted { turn: 1 }
    ));
    assert!(matches!(
        &persisted[2].kind,
        PendingEvent::UserMessageAccepted { turn: 1, content, .. } if content == "run"
    ));
    assert!(matches!(
        &persisted[3].kind,
        PendingEvent::ConversationInputCommitted {
            agent_turn: 1,
            accepted_source,
            selection: rw_types::conversation_input::InputSelection::Accepted {},
        } if *accepted_source == persisted[2].sequence
    ));
    let terminal = &persisted[persisted.len() - 3..];
    assert!(matches!(
        &terminal[0].kind,
        PendingEvent::TextDelta { turn: 1, text } if text == "terminal"
    ));
    assert!(matches!(
        terminal[1].kind,
        PendingEvent::ConversationTurnCommitted { agent_turn: 1, .. }
    ));
    assert!(matches!(
        terminal[2].kind,
        PendingEvent::TurnFinished {
            turn: 1,
            status: AgentTurnStatus::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn no_hook_multi_message_opening_batch_preserves_accept_and_commit_order() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(RecordingSink::default());
    let model = Arc::new(ScriptedModel::new([stop_script("done", &[])]));
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.recovered.title = Some("batch fixture".to_owned());
    actor_config.event_sink = sink.clone();
    actor_config.recovered.queued_messages =
        vec!["first queued".to_owned(), "second queued".to_owned()];
    let actor_config = crate::engine::tests::fixtures::history::bind(actor_config)
        .await
        .expect("seed canonical source");
    let initial_events = sink.events.lock().expect("seed events").len();
    let initial_batches = sink.batch_sizes.lock().expect("seed batches").len();
    let handle = crate::engine::SessionActor::spawn(actor_config).expect("actor");

    timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = handle.snapshot().await.expect("snapshot");
            if !snapshot.running && snapshot.completed_turns == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("queued turn completion");

    assert_eq!(
        &sink.batch_sizes.lock().expect("batch sizes")[initial_batches..],
        &[5, 1, 1, 3]
    );
    let persisted_guard = sink.events.lock().expect("event sink lock");
    let persisted = &persisted_guard[initial_events..];
    assert!(matches!(
        persisted[0].kind,
        PendingEvent::TurnStarted { turn: 1 }
    ));
    for (event, expected) in persisted[1..3]
        .iter()
        .zip(["first queued", "second queued"])
    {
        assert!(matches!(
            &event.kind,
            PendingEvent::UserMessageAccepted { turn: 1, content, .. }
                if content == expected
        ));
    }
    for (event, expected) in persisted[3..5].iter().zip(&persisted[1..3]) {
        assert!(matches!(
            &event.kind,
            PendingEvent::ConversationInputCommitted {
                agent_turn: 1,
                accepted_source,
                selection: rw_types::conversation_input::InputSelection::Accepted {},
            } if *accepted_source == expected.sequence
        ));
    }
    drop(persisted_guard);
    let requests = model.requests.lock().expect("request lock");
    let user_text = requests[0]
        .turns
        .iter()
        .filter(|turn| turn.role == Role::User)
        .filter_map(|turn| match turn.blocks.as_slice() {
            [Block::Text { text }] => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(user_text, ["first queued", "second queued"]);
}

#[tokio::test]
async fn registered_user_prompt_hook_keeps_rewrite_on_the_separate_commit_path() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(RecordingSink::default());
    let model = Arc::new(ScriptedModel::new([stop_script("done", &[])]));
    let mut hooks = builtin_hook_dispatcher().expect("hooks");
    hooks
        .register(
            HookRegistration::new(
                "fixture.rewrite-user",
                HookEvent::UserPromptSubmit,
                rw_types::hook_contract::HookClass::Transform,
            ),
            RewriteUserPromptHook("rewritten by hook"),
        )
        .expect("user prompt hook");
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        hooks,
    );
    actor_config.recovered.title = Some("batch fixture".to_owned());
    actor_config.event_sink = sink.clone();
    let actor_config = crate::engine::tests::fixtures::history::bind(actor_config)
        .await
        .expect("seed canonical source");
    let initial_events = sink.events.lock().expect("seed events").len();
    let initial_batches = sink.batch_sizes.lock().expect("seed batches").len();
    let handle = crate::engine::SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");

    handle.send_message("raw input").await.expect("message");
    collect_turn(&mut events).await;

    assert_eq!(
        &sink.batch_sizes.lock().expect("batch sizes")[initial_batches..],
        &[1, 2, 1, 1, 1, 3]
    );
    let persisted_guard = sink.events.lock().expect("event sink lock");
    let persisted = &persisted_guard[initial_events..];
    assert!(matches!(
        &persisted[2].kind,
        PendingEvent::UserMessageAccepted { content, .. } if content == "raw input"
    ));
    assert!(matches!(
        &persisted[3].kind,
        PendingEvent::ConversationInputCommitted {
            accepted_source,
            selection: rw_types::conversation_input::InputSelection::Transformed { text },
            ..
        } if *accepted_source == persisted[2].sequence && text == "rewritten by hook"
    ));
    drop(persisted_guard);
    let requests = model.requests.lock().expect("request lock");
    assert!(requests[0].turns.iter().any(|turn| matches!(
        turn.blocks.as_slice(),
        [Block::Text { text }] if turn.role == Role::User && text == "rewritten by hook"
    )));
}

#[tokio::test]
async fn multiple_immediate_deltas_coalesce_without_losing_order() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(RecordingSink::default());
    let script = vec![
        Ok(ProviderEvent::MessageStart {
            model: "fixture-model".to_owned(),
        }),
        Ok(ProviderEvent::TextDelta {
            text: "first".to_owned(),
        }),
        Ok(ProviderEvent::TextDelta {
            text: "second".to_owned(),
        }),
        Ok(ProviderEvent::Finished {
            reason: FinishReason::Stop,
        }),
    ];
    let mut actor_config = config(
        root.path(),
        Arc::new(ScriptedModel::new([script])),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.recovered.title = Some("batch fixture".to_owned());
    actor_config.event_sink = sink.clone();
    let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("subscription");

    handle.send_message("run").await.expect("message");
    let observed = collect_turn(&mut events).await;
    let deltas = observed
        .iter()
        .filter_map(|event| match &event.kind {
            PendingEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deltas, ["firstsecond"]);
    assert_eq!(
        sink.events
            .lock()
            .expect("event sink")
            .iter()
            .filter(|event| matches!(event.kind, PendingEvent::TextDelta { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn delayed_finish_never_holds_a_lone_delta_beyond_the_coalescing_window() {
    let root = TempDir::new().expect("tempdir");
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        Arc::new(DelayedFinishModel {
            delay: Duration::from_millis(50),
        }),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");

    handle.send_message("run").await.expect("message");
    let delta = timeout(
        Duration::from_millis(30),
        next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::TextDelta { .. })
        }),
    )
    .await
    .expect("delta must be visible promptly");
    assert!(matches!(
        delta.kind,
        PendingEvent::TextDelta { turn: 1, ref text }
            if text == "visible promptly"
    ));
    let finished = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::TurnFinished { .. })
    })
    .await;
    assert!(matches!(
        finished.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn continuous_deltas_flush_on_the_anchored_coalescing_deadline() {
    let root = TempDir::new().expect("tempdir");
    let first_delta = Arc::new(Notify::new());
    let allow_finish = Arc::new(Notify::new());
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        Arc::new(ContinuousDeltaModel {
            count: 50,
            first_delta: Arc::clone(&first_delta),
            allow_finish: Arc::clone(&allow_finish),
            delay: Duration::from_micros(100),
        }),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");

    handle.send_message("run").await.expect("message");
    timeout(Duration::from_secs(3), first_delta.notified())
        .await
        .expect("provider started after owned context preparation");
    let delta = timeout(
        Duration::from_millis(30),
        next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::TextDelta { .. })
        }),
    )
    .await
    .expect("continuous provider output must be visible before stream completion");
    assert!(matches!(
        delta.kind,
        PendingEvent::TextDelta { turn: 1, ref text } if !text.is_empty()
    ));
    allow_finish.notify_one();
    let finished = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::TurnFinished { .. })
    })
    .await;
    assert!(matches!(
        finished.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Completed,
            ..
        }
    ));
}
