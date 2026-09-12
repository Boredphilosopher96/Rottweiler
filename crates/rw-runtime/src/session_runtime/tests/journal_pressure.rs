//! Actual journal admission under injected storage delay, with independent actor control.
use super::{child_plugin_sessions::controller, dormant_controls};
use rw_core::SessionActor;
use rw_types::SessionId;

async fn run(rounds: usize) {
    let private = tempfile::tempdir().expect("private storage");
    #[cfg(unix)]
    std::fs::set_permissions(
        private.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("private permissions");
    let workspace = tempfile::tempdir().expect("workspace");
    let owner = controller(private.path(), workspace.path());
    let actor = SessionActor::spawn(
        dormant_controls::config(
            &owner,
            private.path(),
            &SessionId("pressure-control".into()),
            workspace.path(),
            std::sync::Arc::default(),
        )
        .await
        .expect("independent actor config"),
    )
    .expect("control actor");
    assert_eq!(
        actor
            .dispatch(rw_types::ClientCommand::AttachSession {
                meta: rw_types::CommandMeta {
                    protocol_version: rw_types::PROTOCOL_VERSION,
                    client_id: rw_types::ClientId("local".into()),
                    request_id: rw_types::RequestId("pressure-attach".into()),
                },
                session_id: actor.session_id().clone(),
                last_seen_sequence: None,
                role: rw_types::ClientRole::Driver,
            })
            .await
            .expect("driver attached before pressure"),
        rw_types::CommandOutcome::Accepted {}
    );
    for round in 0..rounds {
        owner
            .journal_service
            .commits
            .measure_storage_pressure(&actor, round)
            .await;
    }
    actor.close().await.expect("actor physically closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_pressure_preserves_queue_bounds_and_independent_control() {
    run(1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "optimized acceptance workload; invoke prebuilt binary with --ignored --exact --nocapture"]
async fn measure_storage_pressure_queue_age_throughput_and_control() {
    run(10).await;
}
