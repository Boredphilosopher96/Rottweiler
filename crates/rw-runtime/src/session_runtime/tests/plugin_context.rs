//! SDK context surgery targets a real canonical conversation source across reopen.
#![allow(clippy::expect_used)]
use super::plugin_command_session::{
    compose_fixture_session, compose_fixture_with_provider, configure_plugin,
};
use rw_providers::{FinishReason, ProviderEvent};
use rw_types::EngineEvent;
use std::time::Duration;

#[tokio::test]
async fn sdk_context_workflow_pins_and_evicts_committed_conversation() {
    let _admission = crate::native_fixture::admit().await;
    let root = tempfile::tempdir().expect("root");
    let storage = root.path().join("storage");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&storage).expect("storage");
    std::fs::create_dir(&workspace).expect("workspace");
    let workspace = workspace
        .canonicalize()
        .expect("canonical workspace identity");
    #[cfg(unix)]
    std::fs::set_permissions(
        &storage,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("private storage");
    let seeded = compose_fixture_with_provider(
        &storage,
        &workspace,
        "context-workflow",
        false,
        super::HostedProviderMode::DeterministicReplay {
            provider_name: "fixture".into(),
            scripts: vec![vec![
                ProviderEvent::TextDelta {
                    text: "Committed context input".into(),
                },
                ProviderEvent::Finished {
                    reason: FinishReason::Stop,
                },
            ]],
            event_delay_ms: 0,
        },
    )
    .await;
    let mut events = seeded.handle.subscribe_live().expect("seed events");
    seeded
        .handle
        .send_message("Keep this source addressable")
        .await
        .expect("user input");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            events.recv().await.expect("seed event").as_ref().clone(),
            EngineEvent::TurnFinished { .. }
        ) {}
    })
    .await
    .expect("canonical seed turn");
    seeded.handle.close().await.expect("seed effects settled");
    drop(events);
    drop(seeded);
    configure_plugin(root.path(), &storage, &workspace, "context-workflow", &[]).await;
    let runtime = compose_fixture_session(&storage, &workspace, "context-workflow", true).await;
    tokio::time::timeout(
        Duration::from_secs(10),
        runtime.handle.send_message("/manage-context"),
    )
    .await
    .expect("duplex context workflow deadline")
    .expect("native SDK context controls");
    let receipt = runtime
        .handle
        .plugin_session_capability("context-workflow")
        .expect("namespace")
        .read_state()
        .await
        .expect("context receipt");
    let managed = receipt
        .entries
        .iter()
        .find(|entry| entry.key == "managed")
        .expect("source receipt");
    let source = managed.value.as_str().expect("canonical source ID");
    assert!(source.starts_with("conversation:"));
    let context = runtime
        .handle
        .context_snapshot()
        .await
        .expect("canonical context");
    assert!(
        context
            .items
            .iter()
            .any(|item| item.item_id.0 == source && item.state.evicted)
    );
    runtime
        .handle
        .close()
        .await
        .expect("native generation settled");
}
