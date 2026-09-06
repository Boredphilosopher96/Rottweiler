use super::fixtures::{
    history,
    models::ScriptedModel,
    sinks::BlockingBatchSink,
    support::{config, stop_script, wire_event},
};
use crate::engine::{PendingEvent, SessionActor};
use rw_tools::ToolRegistry;
use rw_types::{
    AttachmentData, EngineEvent, SequenceId, StoredAttachment, config::PermissionDecision,
};
use std::{sync::Arc, time::Duration};

fn seed() -> Vec<EngineEvent> {
    let body = "attachment survives restart".repeat(1024);
    vec![
        wire_event(0, PendingEvent::TurnStarted { turn: 1 }),
        wire_event(
            1,
            PendingEvent::UserMessageAccepted {
                turn: 1,
                content: "accepted once".into(),
                attachments: vec![StoredAttachment {
                    name: "input.txt".into(),
                    source_path: None,
                    media_type: "text/plain".into(),
                    content_hash: blake3::hash(body.as_bytes()).to_hex().to_string(),
                    byte_len: u64::try_from(body.len()).expect("fixture bytes"),
                    data: AttachmentData::Text { content: body },
                }],
            },
        ),
    ]
}

#[tokio::test]
async fn explicit_continuation_keeps_accepted_source_after_response_waiter_loss() {
    let root = tempfile::tempdir().expect("workspace");
    let sink = Arc::new(BlockingBatchSink {
        should_block: |events| {
            events.iter().any(|event| {
                matches!(event,
            EngineEvent::TurnStarted { turn_id, .. } if turn_id.0 == "2")
            })
        },
        persisted: std::sync::Mutex::new(seed()),
        blocked_once: std::sync::atomic::AtomicBool::default(),
        entered: tokio::sync::Notify::default(),
        release: tokio::sync::Notify::default(),
    });
    let model = Arc::new(ScriptedModel::new([stop_script("done", &[])]));
    let mut configuration = config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        rw_ext::HookDispatcher::new(),
    );
    configuration.event_sink = sink.clone();
    let handle =
        SessionActor::spawn_for_controls(history::bind(configuration).await.expect("source"))
            .expect("actor");
    handle.ensure_local_driver().await.expect("driver");
    assert_eq!(model.request_count(), 0);
    let mut events = handle.subscribe().expect("events");
    let caller = handle.clone();
    let waiter = tokio::spawn(async move { caller.send_message("explicit continuation").await });
    tokio::time::timeout(Duration::from_secs(3), sink.entered.notified())
        .await
        .expect("admitted opening");
    waiter.abort();
    assert!(waiter.await.expect_err("dropped waiter").is_cancelled());
    sink.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let EngineEvent::TurnFinished {
                turn_id, status, ..
            } = events.recv().await.expect("event").as_ref().clone()
            {
                if turn_id.0 == "2" {
                    assert_eq!(status, rw_types::TurnStatus::Completed);
                    break;
                }
                assert_eq!(turn_id.0, "1", "only the repair precedes the explicit turn");
            }
        }
    })
    .await
    .expect("explicit resumed turn completion");
    assert_eq!(model.request_count(), 1);
    handle.close().await.expect("settled actor");
    let persisted = sink.persisted.lock().expect("events");
    assert_eq!(
        persisted
            .iter()
            .filter(|event| matches!(event,
        EngineEvent::UserMessageAccepted { content, .. } if content == "accepted once"))
            .count(),
        1
    );
    assert_eq!(
        persisted
            .iter()
            .filter(|event| matches!(
                event,
                EngineEvent::ConversationInputCommitted {
                    agent_turn: 2,
                    accepted_source: SequenceId(1),
                    ..
                }
            ))
            .count(),
        1
    );
    let selected = crate::engine::project_session_events(&persisted).expect("exact source audit");
    let text = serde_json::to_string(&selected.conversation).expect("selected IR");
    assert_eq!(text.matches("accepted once").count(), 1);
    assert_eq!(text.matches("attachment survives restart").count(), 1024);
}
