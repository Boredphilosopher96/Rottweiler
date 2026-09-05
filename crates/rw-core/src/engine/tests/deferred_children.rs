#![cfg(test)]
use super::fixtures::{
    models::ScriptedModel,
    support::{config, stop_script},
};
use crate::engine::builtin_hook_dispatcher;
use crate::{
    ActorSubagentSessionFactory, OrchestrationError, SubagentProgressObserver,
    SubagentRecoveryPolicy, SubagentSession, SubagentSessionFactory,
};
use rw_tools::{CancellationToken, ToolRegistry};
use rw_types::{SessionId, SessionMode, config::PermissionDecision};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

struct Progress;
#[async_trait::async_trait]
impl SubagentProgressObserver for Progress {
    async fn progress(
        &self,
        _: Option<u64>,
        _: serde_json::Value,
    ) -> Result<(), OrchestrationError> {
        Ok(())
    }
}
fn policy() -> SubagentRecoveryPolicy {
    SubagentRecoveryPolicy {
        model_alias: "fast".into(),
        system_prompt: None,
        permission_mode: SessionMode::Execute,
        max_turns: 10,
    }
}
async fn rebind(
    factory: &ActorSubagentSessionFactory,
    root: &std::path::Path,
) -> Arc<dyn SubagentSession> {
    factory
        .rebind(
            &SessionId("child".into()),
            Some(root),
            None,
            Some(Arc::new(ToolRegistry::new())),
            &policy(),
        )
        .await
        .expect("rebind")
        .expect("session")
}
#[tokio::test]
async fn dormant_close_never_prepares_an_actor() {
    let builds = Arc::new(AtomicUsize::new(0));
    let count = builds.clone();
    let factory =
        ActorSubagentSessionFactory::new(|_| unreachable!()).with_rebuilder(move |_, _, _| {
            count.fetch_add(1, Ordering::SeqCst);
            unreachable!("dormant child must not build")
        });
    let root = tempfile::tempdir().expect("root");
    let child = rebind(&factory, root.path()).await;
    child.cancel().await.expect("cancel dormant");
    child.close(None).await.expect("close dormant");
    assert_eq!(builds.load(Ordering::SeqCst), 0);
    assert!(
        child
            .run_turn(
                "closed".into(),
                CancellationToken::default(),
                Arc::new(Progress)
            )
            .await
            .is_err()
    );
}
#[tokio::test]
async fn resumed_followups_observe_only_their_own_turn() {
    let builds = Arc::new(AtomicUsize::new(0));
    let count = builds.clone();
    let model = Arc::new(ScriptedModel::new([
        stop_script("first answer", &[]),
        stop_script("second answer", &[]),
    ]));
    let factory =
        ActorSubagentSessionFactory::new(|_| unreachable!()).with_rebuilder(move |_, root, _| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(config(
                root,
                model.clone(),
                Arc::new(ToolRegistry::new()),
                PermissionDecision::Allow,
                builtin_hook_dispatcher().expect("hooks"),
            ))
        });
    let root = tempfile::tempdir().expect("root");
    let child = rebind(&factory, root.path()).await;
    assert_eq!(builds.load(Ordering::SeqCst), 0);
    for answer in ["first answer", "second answer"] {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            child.run_turn(
                "question".into(),
                CancellationToken::default(),
                Arc::new(Progress),
            ),
        )
        .await
        .expect("turn timeout")
        .expect("turn");
        assert_eq!(result.final_text, answer);
    }
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    child.close(None).await.expect("close");
}
#[tokio::test]
async fn dropped_activation_retains_preparation_and_close_waits_for_it() {
    let (started, entered) = tokio::sync::oneshot::channel();
    let started = std::sync::Mutex::new(Some(started));
    let (release, wait) = std::sync::mpsc::channel();
    let wait = std::sync::Mutex::new(wait);
    let factory =
        ActorSubagentSessionFactory::new(|_| unreachable!()).with_rebuilder(move |_, root, _| {
            started
                .lock()
                .expect("started lock")
                .take()
                .expect("single preparation")
                .send(())
                .expect("notify");
            wait.lock().expect("release lock").recv().expect("release");
            Ok(config(
                root,
                Arc::new(ScriptedModel::default()),
                Arc::new(ToolRegistry::new()),
                PermissionDecision::Allow,
                builtin_hook_dispatcher().expect("hooks"),
            ))
        });
    let root = tempfile::tempdir().expect("root");
    let child = rebind(&factory, root.path()).await;
    let caller_child = child.clone();
    let caller = tokio::spawn(async move {
        caller_child
            .run_turn(
                "question".into(),
                CancellationToken::default(),
                Arc::new(Progress),
            )
            .await
    });
    entered.await.expect("preparation started");
    caller.abort();
    let _ = caller.await;
    let close_child = child.clone();
    let close = tokio::spawn(async move { close_child.close(None).await });
    tokio::task::yield_now().await;
    assert!(!close.is_finished());
    release.send(()).expect("release builder");
    tokio::time::timeout(Duration::from_secs(5), close)
        .await
        .expect("close timeout")
        .expect("close task")
        .expect("proven close");
}

#[tokio::test]
async fn dormant_policy_admission_is_shared_and_released_only_on_close() {
    let factory = ActorSubagentSessionFactory::new(|_| unreachable!())
        .with_rebuilder(|_, _, _| unreachable!("dormant builder"));
    let root = tempfile::tempdir().expect("root");
    let mut policy = policy();
    policy.system_prompt = Some("x".repeat(512 * 1024));
    let tools = Arc::new(ToolRegistry::new());
    let mut children = Vec::new();
    for index in 0..256 {
        match factory
            .rebind(
                &SessionId(format!("child-{index}")),
                Some(root.path()),
                None,
                Some(tools.clone()),
                &policy,
            )
            .await
        {
            Ok(Some(child)) => children.push(child),
            Err(error) => {
                assert!(error.to_string().contains("policy allocation budget"));
                break;
            }
            Ok(None) => panic!("factory must support recovery"),
        }
    }
    assert!(!children.is_empty() && children.len() < 256);
    children
        .pop()
        .expect("admitted child")
        .close(None)
        .await
        .expect("close releases policy");
    let replacement = factory
        .rebind(
            &SessionId("replacement".into()),
            Some(root.path()),
            None,
            Some(tools),
            &policy,
        )
        .await
        .expect("released admission")
        .expect("replacement");
    replacement.close(None).await.expect("close");
    for child in children {
        child.close(None).await.expect("close");
    }
}
