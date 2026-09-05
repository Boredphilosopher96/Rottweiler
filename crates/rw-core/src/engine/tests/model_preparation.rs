#![cfg(test)]
use super::fixtures::support::{config, protocol_meta};
use crate::PluginSessionCapability;
use crate::engine::{AgentLoopError, ModelDriver, SessionHandle, builtin_hook_dispatcher};
use async_trait::async_trait;
use rw_providers::{BoxEventStream, ProviderRequest};
use rw_tools::ToolRegistry;
use rw_types::extension_control::{ExtensionControl, ExtensionControlOutcome};
use rw_types::{
    ClientCommand, ClientRole, CommandOutcome, ModelAlias, SessionId, config::PermissionDecision,
};
use std::future::Future;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Default)]
struct CallbackModel {
    session: OnceLock<PluginSessionCapability>,
    entered: Notify,
    release: Notify,
    prepared: AtomicUsize,
    committed: AtomicUsize,
    discarded: AtomicUsize,
}
#[async_trait]
impl ModelDriver for CallbackModel {
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }
    fn stream(
        &self,
        _: &str,
        _: ProviderRequest,
        _: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        Err(AgentLoopError::Provider(
            "selection fixture has no provider stream".into(),
        ))
    }
    async fn prepare_model(&self, _: &str) -> Result<(), AgentLoopError> {
        let session = self.session.get().expect("bound capability");
        session.query().await.expect("preparation may call actor");
        session
            .set_status("resolving catalog")
            .await
            .expect("duplex status");
        self.prepared.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        session.query().await.expect("final callback");
        Ok(())
    }
    fn commit_prepared_model(&self, _: &str) {
        self.committed.fetch_add(1, Ordering::SeqCst);
    }
    fn discard_prepared_model(&self, _: &str) {
        self.discarded.fetch_add(1, Ordering::SeqCst);
    }
}

async fn actor(root: &std::path::Path, model: &Arc<CallbackModel>) -> SessionHandle {
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root,
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .await
    .expect("actor");
    model
        .session
        .set(
            handle
                .plugin_session_capability("catalog")
                .expect("capability"),
        )
        .expect("bind once");
    assert_eq!(
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("driver", "attach"),
                session_id: SessionId("fixture-session".into()),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach"),
        CommandOutcome::Accepted {}
    );
    handle
}

async fn select(handle: SessionHandle, plugin: bool) -> Result<(), AgentLoopError> {
    if plugin {
        let outcome = handle
            .plugin_session_capability("catalog")
            .expect("capability")
            .control(
                None,
                ExtensionControl::SelectModel {
                    model: ModelAlias("slow".into()),
                    provider: None,
                },
            )
            .await?;
        assert!(matches!(outcome, ExtensionControlOutcome::Applied {}));
    } else {
        assert_eq!(
            handle
                .dispatch(ClientCommand::SwitchModel {
                    meta: protocol_meta("driver", "select"),
                    session_id: SessionId("fixture-session".into()),
                    model: ModelAlias("slow".into()),
                    provider: None,
                })
                .await?,
            CommandOutcome::Accepted {}
        );
    }
    Ok(())
}

#[tokio::test]
async fn model_preparation_services_callbacks_for_client_and_plugin_selection() {
    for plugin in [false, true] {
        let root = tempfile::tempdir().expect("root");
        let model = Arc::new(CallbackModel::default());
        let handle = actor(root.path(), &model).await;
        let caller = tokio::spawn(select(handle.clone(), plugin));
        tokio::time::timeout(Duration::from_secs(2), model.entered.notified())
            .await
            .expect("provider catalog callback is serviced while selection waits");
        let snapshot = tokio::time::timeout(Duration::from_secs(2), handle.snapshot())
            .await
            .expect("responsive actor")
            .expect("snapshot");
        assert_eq!(snapshot.model_alias, "fast");
        assert!(!caller.is_finished());
        assert!(matches!(
            model
                .session
                .get()
                .expect("session")
                .control(
                    None,
                    ExtensionControl::SelectModel {
                        model: ModelAlias("another".into()),
                        provider: None
                    }
                )
                .await
                .expect("busy reply"),
            ExtensionControlOutcome::Busy {}
        ));
        model.release.notify_one();
        caller.await.expect("selection task").expect("selected");
        assert_eq!(
            handle.snapshot().await.expect("snapshot").model_alias,
            "slow"
        );
        assert_eq!(model.prepared.load(Ordering::SeqCst), 1);
        assert_eq!(model.committed.load(Ordering::SeqCst), 1);
        handle.close().await.expect("close");
    }
}

#[tokio::test]
async fn abandoned_model_selection_retains_preparation_until_shutdown_proof() {
    let root = tempfile::tempdir().expect("root");
    let model = Arc::new(CallbackModel::default());
    let handle = actor(root.path(), &model).await;
    let caller = tokio::spawn(select(handle.clone(), true));
    tokio::time::timeout(Duration::from_secs(2), model.entered.notified())
        .await
        .expect("entered");
    caller.abort();
    assert!(caller.await.expect_err("aborted caller").is_cancelled());
    let mut close = std::pin::pin!(handle.close());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(close.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    // Closing still services this query, proving the close request is not a task abort.
    model
        .session
        .get()
        .expect("session")
        .query()
        .await
        .expect("owned callback");

    model.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), close)
        .await
        .expect("settled shutdown")
        .expect("close");
    assert_eq!(model.committed.load(Ordering::SeqCst), 0);
    assert_eq!(model.discarded.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn model_preparation_rechecks_driver_before_selection_publication() {
    let root = tempfile::tempdir().expect("root");
    let model = Arc::new(CallbackModel::default());
    let handle = actor(root.path(), &model).await;
    let caller = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .dispatch(ClientCommand::SwitchModel {
                    meta: protocol_meta("driver", "select"),
                    session_id: SessionId("fixture-session".into()),
                    model: ModelAlias("slow".into()),
                    provider: None,
                })
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), model.entered.notified())
        .await
        .expect("entered");
    assert_eq!(
        handle
            .dispatch(ClientCommand::TakeDriver {
                meta: protocol_meta("replacement", "takeover"),
                session_id: SessionId("fixture-session".into()),
            })
            .await
            .expect("takeover"),
        CommandOutcome::Accepted {}
    );
    model.release.notify_one();
    assert!(matches!(
        caller.await.expect("task").expect("reply"),
        CommandOutcome::Rejected { .. }
    ));
    assert_eq!(
        handle.snapshot().await.expect("snapshot").model_alias,
        "fast"
    );
    assert_eq!(model.committed.load(Ordering::SeqCst), 0);
    assert_eq!(model.discarded.load(Ordering::SeqCst), 1);
    handle.close().await.expect("close");
}
