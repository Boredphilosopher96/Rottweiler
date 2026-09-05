#![cfg(test)]

use crate::engine::SESSION_TITLE_MAX_CHARS;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::project_session_events;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::models::DelayedFinishModel;
use crate::engine::tests::fixtures::models::PendingModel;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::stop_script;
use rw_providers::TokenUsage;
use rw_providers::ToolChoice;
use rw_tools::ToolRegistry;
use rw_types::ClientCommand;
use rw_types::ClientId;
use rw_types::CommandMeta;
use rw_types::CommandOutcome;
use rw_types::EngineEvent;
use rw_types::PROTOCOL_VERSION;
use rw_types::RequestId;
use rw_types::config::PermissionDecision;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[tokio::test]
async fn first_successful_turn_generates_and_replays_a_bounded_fast_model_title() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(RecordingSink::default());
    let model = Arc::new(
        ScriptedModel::new([
            stop_script("The project is a Rust workspace.", &[]),
            stop_script(
                "Rust Workspace Structure",
                &[TokenUsage {
                    input_tokens: 18,
                    output_tokens: 3,
                    ..TokenUsage::default()
                }],
            ),
        ])
        .with_title_alias(),
    );
    let mut actor_config = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle
        .send_message("explain the project structure")
        .await
        .expect("message");

    let titled = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::SessionTitleUpdated { .. })
    })
    .await;
    assert!(matches!(
        titled.kind,
        PendingEvent::SessionTitleUpdated { ref title, usage: Some(_), cost: Some(_)}
            if title == "Rust Workspace Structure"
    ));
    assert_eq!(model.request_count(), 2);
    assert_eq!(model.aliases(), ["fast", "fast"]);
    let requests = model.requests.lock().expect("requests");
    let title_request = requests.last().expect("title request");
    assert_eq!(title_request.max_output_tokens, 32);
    assert_eq!(title_request.tool_choice, ToolChoice::None {});
    assert!(title_request.tools.is_empty());
    drop(requests);

    let durable = sink
        .events
        .lock()
        .expect("events")
        .iter()
        .map(|event| event.wire.clone())
        .collect::<Vec<_>>();
    let recovered = project_session_events(&durable).expect("replay title");
    assert_eq!(recovered.title.as_deref(), Some("Rust Workspace Structure"));
    assert!(recovered.accounting.usage.output_tokens > 0);
    assert!(durable.iter().any(|event| matches!(event,
        EngineEvent::SessionTitleUpdated { usage: Some(usage), .. } if usage.output_tokens > 0
    )));
}

#[tokio::test]
async fn manual_rename_before_first_turn_completion_prevents_auto_title_overwrite() {
    let root = TempDir::new().expect("tempdir");
    let sink = Arc::new(RecordingSink::default());
    let mut actor_config = config(
        root.path(),
        Arc::new(DelayedFinishModel {
            delay: Duration::from_millis(100),
        }),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.event_sink = sink.clone();
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle
        .send_message("explain the project structure")
        .await
        .expect("message");
    assert!(handle.snapshot().await.expect("running snapshot").running);

    handle
        .dispatch_durably(ClientCommand::RenameSession {
            meta: CommandMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId("local".to_owned()),
                request_id: RequestId("manual-title".to_owned()),
            },
            session_id: handle.session_id().clone(),
            title: "  Manual auth refactor  ".to_owned(),
        })
        .await
        .expect("manual rename");

    timeout(Duration::from_secs(1), async {
        loop {
            if matches!(
                events.recv().await.expect("turn event"),
                EngineEvent::TurnFinished { .. }
            ) {
                break;
            }
        }
    })
    .await
    .expect("first turn completion");
    tokio::time::sleep(Duration::from_millis(25)).await;

    let durable = sink
        .events
        .lock()
        .expect("events")
        .iter()
        .map(|event| event.wire.clone())
        .collect::<Vec<_>>();
    let titles = durable
        .iter()
        .filter_map(|event| match event {
            EngineEvent::SessionTitleUpdated { title, .. } => Some(title.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(titles, ["Manual auth refactor"]);
    assert_eq!(
        project_session_events(&durable)
            .expect("replay manual title")
            .title
            .as_deref(),
        Some("Manual auth refactor")
    );
}

#[tokio::test]
async fn manual_session_title_validation_rejects_empty_long_and_control_text() {
    let root = TempDir::new().expect("tempdir");
    let handle = SessionActor::spawn(config(
        root.path(),
        Arc::new(PendingModel),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    for (request, title) in [
        ("empty-title", "   ".to_owned()),
        (
            "long-title",
            "x".repeat(SESSION_TITLE_MAX_CHARS.saturating_add(1)),
        ),
        ("control-title", "auth\nrefactor".to_owned()),
    ] {
        let outcome = handle
            .dispatch(ClientCommand::RenameSession {
                meta: CommandMeta {
                    protocol_version: PROTOCOL_VERSION,
                    client_id: ClientId("picker".to_owned()),
                    request_id: RequestId(request.to_owned()),
                },
                session_id: handle.session_id().clone(),
                title,
            })
            .await
            .expect("validation outcome");
        assert!(matches!(
            outcome,
            CommandOutcome::Rejected { error } if error.code == "invalid_session_title"
        ));
    }
}

#[tokio::test]
async fn unavailable_title_model_persists_first_prompt_fallback_after_success_only() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([stop_script("Done.", &[])]));
    let handle = SessionActor::spawn(config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle
        .send_message("fix reconnect recovery without blocking input")
        .await
        .expect("message");

    let mut saw_finished = false;
    loop {
        let event = timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("title timeout")
            .expect("title event");
        if matches!(event, EngineEvent::TurnFinished { .. }) {
            saw_finished = true;
        }
        if let EngineEvent::SessionTitleUpdated { title, .. } = event {
            assert!(saw_finished, "fallback must wait for a successful turn");
            assert_eq!(title, "fix reconnect recovery without blocking input");
            break;
        }
    }
    assert_eq!(
        model.request_count(),
        1,
        "fallback must not make a model call"
    );
}
