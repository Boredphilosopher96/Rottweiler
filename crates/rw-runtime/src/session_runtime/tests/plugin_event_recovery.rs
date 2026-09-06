//! Native SDK crash recovery uses the same journal transaction as namespace state.
#![allow(clippy::expect_used)]
use super::plugin_command_session::{compose_fixture_session, configure_plugin};
use rw_types::extension_contract::ExtensionStateSnapshot;
use std::time::Duration;

async fn state_with(handle: &rw_core::SessionHandle, key: &str) -> ExtensionStateSnapshot {
    let capability = handle
        .plugin_session_capability("event-recovery")
        .expect("namespace");
    let mut last = None;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = capability
                .read_state()
                .await
                .expect("durable extension state");
            if state.entries.iter().any(|entry| entry.key == key) {
                return state;
            }
            last = Some(state);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("delivery transition deadline waiting for {key}; last namespace: {last:?}")
    })
}
fn value<'a>(state: &'a ExtensionStateSnapshot, key: &str) -> &'a serde_json::Value {
    &state
        .entries
        .iter()
        .find(|entry| entry.key == key)
        .expect("state entry")
        .value
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keeps the production setup and ordered lifecycle assertions in one scenario."
)]
async fn sdk_event_process_crash_replays_only_the_unacknowledged_delivery() {
    let _admission = crate::native_fixture::admit().await;
    let root = tempfile::tempdir().expect("fixture root");
    let storage = root.path().join("storage");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&storage).expect("storage");
    std::fs::create_dir(&workspace).expect("workspace");
    #[cfg(unix)]
    std::fs::set_permissions(
        &storage,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("private storage");
    let workspace = workspace.canonicalize().expect("canonical workspace");
    configure_plugin(root.path(), &storage, &workspace, "event-recovery", &[]).await;
    let first =
        compose_fixture_session(&storage, &workspace, "event-recovery-session", false).await;
    assert!(matches!(
        first
            .handle
            .dispatch(rw_types::ClientCommand::AttachSession {
                meta: rw_types::CommandMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: rw_types::ClientId("local".into()),
                    request_id: rw_types::RequestId("first-attach".into())
                },
                session_id: rw_types::SessionId("event-recovery-session".into()),
                last_seen_sequence: None,
                role: rw_types::ClientRole::Driver,
            })
            .await
            .expect("produce canonical SessionCreated"),
        rw_types::CommandOutcome::Accepted {}
    ));
    let attempted = state_with(&first.handle, "attempt").await;
    assert!(
        attempted.acknowledged.is_none(),
        "handler-side state cannot acknowledge delivery"
    );
    let attempt = value(&attempted, "attempt");
    // A completed failed command proves the owned RPC observed process death;
    // an SDK PID is local to its namespace and cannot be polled on the host.
    let crash = tokio::time::timeout(
        Duration::from_secs(5),
        first.handle.send_message("/crash-event-worker"),
    )
    .await
    .expect("crashed worker settlement deadline");
    assert!(
        crash.is_err(),
        "crashing command cannot complete successfully"
    );
    first
        .handle
        .close()
        .await
        .expect("failed delivery effects settled");
    drop(first);

    let second =
        compose_fixture_session(&storage, &workspace, "event-recovery-session", true).await;
    let recovered = state_with(&second.handle, "delivered").await;
    assert_eq!(value(&recovered, "delivered"), &attempt["sequence"]);
    assert_eq!(value(&recovered, "deliveries"), &serde_json::json!(1));
    let acknowledged = recovered
        .acknowledged
        .as_ref()
        .expect("state and acknowledgement commit together");
    assert_eq!(
        serde_json::to_value(acknowledged.sequence).expect("cursor"),
        attempt["sequence"]
    );
    assert!(recovered.revision > attempted.revision);
    second
        .handle
        .close()
        .await
        .expect("recovered delivery settled");
    drop(second);

    let third = compose_fixture_session(&storage, &workspace, "event-recovery-session", true).await;
    let current = third
        .handle
        .snapshot()
        .await
        .expect("recovered mode")
        .mode_id;
    let next = if current.0 == "plan" {
        "execute"
    } else {
        "plan"
    };
    let mut events = third.handle.subscribe().expect("durable mode events");
    third
        .handle
        .send_message(format!("/mode {next}"))
        .await
        .expect("produce later subscribed event");
    let barrier_sequence = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let rw_types::EngineEvent::ModeChanged { meta, mode, .. } =
                events.recv().await.expect("mode event stream")
            {
                assert_eq!(mode.0, next);
                break meta.sequence_id;
            }
        }
    })
    .await
    .expect("canonical mode transition");
    let after = state_with(&third.handle, "barrier").await;
    assert_eq!(
        value(&after, "barrier"),
        &serde_json::json!(barrier_sequence)
    );
    assert_eq!(value(&after, "deliveries"), &serde_json::json!(1));
    assert!(
        after
            .acknowledged
            .as_ref()
            .expect("barrier acknowledgement")
            .sequence
            > acknowledged.sequence
    );
    third.handle.close().await.expect("final delivery settled");
}
