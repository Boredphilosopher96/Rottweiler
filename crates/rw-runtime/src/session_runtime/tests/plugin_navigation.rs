//! Driver navigation remains deferred while a real SDK command callback is active.
#![allow(clippy::expect_used)]
use rw_types::{
    EngineEvent, SequenceId,
    extension_contract::{
        ExtensionStateCommitOutcome, ExtensionStateMutation, ExtensionStateTransaction,
    },
    extension_control::SessionNavigationTarget,
};
use std::time::Duration;

pub(super) async fn verify_deferred_navigation(handle: &rw_core::SessionHandle) {
    let mut events = handle.subscribe().expect("driver events");
    let mut caller = tokio::spawn({
        let handle = handle.clone();
        async move { handle.send_message("/context-panel navigate").await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = tokio::select! {
                result = &mut caller => panic!("SDK command returned before navigation admission: {result:?}"),
                event = events.recv() => event.expect("command events"),
            };
            match event.as_ref() {
                EngineEvent::SessionNavigationRequested { .. } => {
                    panic!("navigation before callback settlement")
                }
                EngineEvent::UiNotification { title, .. } if title == "Navigation waiting" => break,
                _ => {}
            }
        }
    })
    .await
    .expect("SDK navigation admission marker");
    assert!(
        !caller.is_finished(),
        "SDK is awaiting the explicit release marker"
    );
    let capability = handle
        .plugin_session_capability("command-session")
        .expect("namespace");
    let before = capability.read_state().await.expect("pending state");
    assert!(
        !before
            .entries
            .iter()
            .any(|entry| entry.key == "navigation/completed")
    );
    assert!(matches!(
        capability
            .commit_state(ExtensionStateTransaction {
                expected_revision: before.revision,
                mutations: vec![ExtensionStateMutation::Set {
                    key: "navigation/release".into(),
                    value: serde_json::json!(true)
                }],
                acknowledged: None,
            })
            .await
            .expect("release callback"),
        ExtensionStateCommitOutcome::Committed { .. }
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let EngineEvent::SessionNavigationRequested { meta, target, .. } = events
                .recv()
                .await
                .expect("navigation event")
                .as_ref()
                .clone()
            {
                assert_eq!(meta.client_id.0, "local");
                assert_eq!(
                    target,
                    SessionNavigationTarget::Transcript {
                        sequence: SequenceId(0)
                    }
                );
                break;
            }
        }
    })
    .await
    .expect("settled callback navigation");
    let after = capability.read_state().await.expect("completed state");
    assert!(
        after
            .entries
            .iter()
            .any(|entry| entry.key == "navigation/completed"
                && entry.value == serde_json::json!(true))
    );
    caller
        .await
        .expect("command task")
        .expect("command completion");
}
