use super::fixtures::{
    models::ScriptedModel,
    sinks::BlockingBatchSink,
    support::{collect_turn, config, stop_script},
};
use crate::engine::{AgentTurnStatus, PendingEvent};
use rw_tools::ToolRegistry;
use rw_types::{EngineEvent, config::PermissionDecision};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn provider_dispatch_waits_for_the_committed_prompt_source() {
    let root = tempfile::tempdir().expect("workspace");
    let sink = Arc::new(BlockingBatchSink {
        should_block: |events| {
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::ContextUsageUpdated { .. }))
        },
        persisted: std::sync::Mutex::default(),
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
    let handle = super::fixtures::history::spawn(configuration)
        .await
        .expect("actor");
    handle.ensure_local_driver().await.expect("driver");
    let mut events = handle.subscribe().expect("events");
    handle
        .send_message("record the exact source")
        .await
        .expect("message");
    tokio::time::timeout(Duration::from_secs(3), sink.entered.notified())
        .await
        .expect("context commit reached sink");
    // Give the provider task a scheduling window while storage deliberately has
    // no commit proof. An asynchronous metric send would already start inference.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(model.request_count(), 0);
    sink.release.notify_one();
    let completed = collect_turn(&mut events).await;
    assert_eq!(model.request_count(), 1);
    assert!(completed.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Completed,
            ..
        }
    )));
    handle.close().await.expect("close");
}
