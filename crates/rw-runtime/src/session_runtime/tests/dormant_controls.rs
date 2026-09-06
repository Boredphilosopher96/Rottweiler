#![cfg(test)]
use super::{CapturingModel, child_plugin_sessions::controller, test_provider_admission};
use crate::session_runtime::{
    native_model_generations::ChildNativeModel, workspace_roots::RuntimeWorkspaceRootController,
};
use rw_core::{
    ActorSubagentSessionFactory, AgentLoopError, PermissionGate, SessionActor, SessionActorConfig,
    SubagentRecoveryPolicy, SubagentSessionFactory,
};
use rw_store::session::SessionEventLog;
use rw_types::{
    ClientCommand, ClientId, ClientRole, CommandMeta, CommandOutcome, EngineEvent, EventMeta,
    ModeId, PlanArtifact, PlanDecision, RequestId, SequenceId, SessionId, SessionMode,
    family_controls::ChildControlResponse,
};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

pub(super) type RequestCapture = Arc<Mutex<Option<rw_providers::ProviderRequest>>>;
fn command(request: &str) -> CommandMeta {
    CommandMeta {
        protocol_version: rw_types::PROTOCOL_VERSION,
        client_id: ClientId("root-driver".into()),
        request_id: RequestId(request.into()),
    }
}
pub(super) fn artifact() -> PlanArtifact {
    PlanArtifact {
        title: "Review saved plan".into(),
        summary_md: "Restore without inference".into(),
        steps: vec![],
        open_questions: vec![],
    }
}
fn seed(storage: &Path, child: &SessionId) {
    let modes = rw_ext::ModeRegistry::builtins().expect("modes");
    let meta = |sequence| EventMeta {
        protocol_version: rw_types::PROTOCOL_VERSION,
        session_id: child.clone(),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-09-05T00:00:00.000Z".into(),
        caused_by: None,
    };
    let mut journal = SessionEventLog::open(storage, &child.0).expect("source");
    journal
        .append_batch([
            EngineEvent::ModeChanged {
                meta: meta(0),
                mode: ModeId("plan".into()),
                definition_fingerprint: modes.get("plan").expect("plan").semantic_fingerprint(),
            },
            EngineEvent::PlanSubmitted {
                meta: meta(1),
                artifact: artifact(),
            },
            EngineEvent::MessageQueued {
                meta: meta(2),
                position: 0,
                content: "durable queued input".into(),
                attachments: vec![],
            },
        ])
        .expect("persist controls");
    // Drop the writer: discovery and activation must reopen the actual source.
}
async fn config(
    owner: &RuntimeWorkspaceRootController,
    storage: &Path,
    session: &SessionId,
    workspace: &Path,
    request: RequestCapture,
) -> Result<SessionActorConfig, AgentLoopError> {
    owner
        .child_config(
            storage,
            &SessionId("root".into()),
            session,
            workspace,
            "fast",
            ChildNativeModel {
                compose: Arc::new(move |_| {
                    Arc::new(CapturingModel {
                        request: request.clone(),
                    })
                }),
                redactor: rw_providers::FixtureRedactor::default(),
                resources: Arc::new(rw_core::NoopSessionResources),
            },
            Arc::new(rw_core::NoopSecretRedactor),
            &PermissionGate::from_config(rw_core::PermissionConfig::default())
                .with_workspace_roots([workspace]),
            4,
            test_provider_admission(),
        )
        .await
}
pub(super) fn factory(
    owner: Arc<RuntimeWorkspaceRootController>,
    storage: &Path,
    request: RequestCapture,
    builds: Arc<AtomicUsize>,
) -> ActorSubagentSessionFactory {
    let storage = storage.to_path_buf();
    let reader = owner.clone();
    ActorSubagentSessionFactory::new(|_| unreachable!("recovery fixture")).with_rebuilder(
        move |session, workspace, _| {
            let owner = owner.clone();
            let storage = storage.clone();
            let request = request.clone();
            let builds = builds.clone();
            Box::pin(async move {
                builds.fetch_add(1, Ordering::SeqCst);
                config(&owner, &storage, session, workspace, request).await
            })
        },
        move |session, workspace| {
            let owner = reader.clone();
            Box::pin(async move { owner.dormant_controls(session, workspace).await })
        },
    )
}
#[tokio::test]
async fn reopened_dormant_plan_is_discovered_selected_and_answered_without_inference() {
    let private = tempfile::tempdir().expect("private");
    #[cfg(unix)]
    std::fs::set_permissions(
        private.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("private storage permissions");
    let workspace = tempfile::tempdir().expect("workspace");
    let session = SessionId("saved-child".into());
    seed(private.path(), &session);
    let owner = Arc::new(controller(private.path(), workspace.path()));
    let request: RequestCapture = Arc::default();
    let builds = Arc::new(AtomicUsize::new(0));
    let factory = factory(
        owner.clone(),
        private.path(),
        request.clone(),
        builds.clone(),
    );
    let child = factory
        .rebind(
            &session,
            Some(workspace.path()),
            None,
            None,
            &SubagentRecoveryPolicy {
                model_alias: "fast".into(),
                system_prompt: None,
                permission_mode: SessionMode::Plan,
                max_turns: 4,
            },
        )
        .await
        .expect("rebind")
        .expect("child");
    let summary = child.control_summary();
    assert!(summary.available && summary.pending_plan);
    assert_eq!(summary.through, Some(SequenceId(2)));
    assert_eq!(builds.load(Ordering::SeqCst), 0, "discovery stays inert");
    let selected = child.child_controls().await.expect("selected snapshot");
    assert_eq!(selected.snapshot.controls.pending_plan, Some(artifact()));
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    assert!(
        request.lock().expect("requests").is_none(),
        "selection cannot resume queued inference"
    );
    let state = child.child_state().await.expect("state");
    assert!(state.active_turn.is_none());
    assert_eq!(state.queued_messages.len(), 1);
    let root = SessionActor::spawn(
        config(
            &owner,
            private.path(),
            &SessionId("root".into()),
            workspace.path(),
            Arc::default(),
        )
        .await
        .expect("root config"),
    )
    .expect("root actor");
    root.dispatch(ClientCommand::AttachSession {
        meta: command("attach"),
        session_id: root.session_id().clone(),
        role: ClientRole::Driver,
        last_seen_sequence: None,
    })
    .await
    .expect("root driver");
    let outcome = child
        .respond_control(
            root.family_control_authority(&ClientId("root-driver".into()))
                .expect("authority"),
            command("reject saved plan"),
            selected.revision,
            ChildControlResponse::Plan {
                decision: PlanDecision::Reject,
                revisions: None,
            },
        )
        .await
        .expect("answer");
    assert!(matches!(outcome, CommandOutcome::Accepted {}));
    assert!(!child.control_summary().pending_plan);
    assert!(
        request.lock().expect("requests").is_none(),
        "review cannot release queued work"
    );
    child.close(None).await.expect("child settled");
    root.close().await.expect("root settled");
}
