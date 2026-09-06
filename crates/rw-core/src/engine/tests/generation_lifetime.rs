use super::fixtures::{
    models::ScriptedModel,
    support::{config, protocol_meta},
};
use crate::{
    AgentLoopError, PreparedRuntimePublication, RuntimePublication, SessionExtensionController,
    SessionExtensionSnapshot,
    engine::{
        builtin_hook_dispatcher,
        session::{PluginSessionCapability, SessionHandle},
    },
};
use async_trait::async_trait;
use rw_tools::ToolRegistry;
use rw_types::{ClientCommand, ClientRole, config::PermissionDecision};
use std::{
    path::Path,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default)]
struct Publication(AtomicBool);
impl PreparedRuntimePublication for Publication {
    fn publish(&self) -> Result<(), AgentLoopError> {
        assert!(
            !self.0.swap(true, Ordering::AcqRel),
            "exactly one publication"
        );
        Ok(())
    }
}
#[derive(Default)]
struct CallbackGeneration {
    capability: OnceLock<PluginSessionCapability>,
    entered: Notify,
    release: Notify,
    finished: AtomicBool,
    publication: Arc<Publication>,
}
#[async_trait]
impl SessionExtensionController for CallbackGeneration {
    async fn attach(
        &self,
        _: &Path,
        mut current: SessionExtensionSnapshot,
    ) -> Result<SessionExtensionSnapshot, AgentLoopError> {
        self.capability
            .get()
            .expect("bound")
            .set_status("retirement callback")
            .await?;
        self.entered.notify_one();
        self.release.notified().await;
        self.capability
            .get()
            .expect("bound")
            .set_status("retirement settled")
            .await?;
        self.finished.store(true, Ordering::Release);
        current.publication = RuntimePublication::Prepared(self.publication.clone());
        Ok(current)
    }
    async fn detach(
        &self,
        _: SessionExtensionSnapshot,
    ) -> Result<SessionExtensionSnapshot, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "fixture has no detach".into(),
        ))
    }
    async fn shutdown(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }
}
async fn fixture(controller: &Arc<CallbackGeneration>, root: &Path) -> SessionHandle {
    let mut configuration = config(
        root,
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    configuration.extension_development = controller.clone();
    let handle = crate::engine::tests::fixtures::history::spawn(configuration)
        .await
        .expect("actor");
    controller
        .capability
        .set(
            handle
                .plugin_session_capability("generation")
                .expect("capability"),
        )
        .expect("bind");
    handle
        .dispatch(ClientCommand::AttachSession {
            meta: protocol_meta("driver", "driver-attach"),
            session_id: handle.session_id().clone(),
            last_seen_sequence: None,
            role: ClientRole::Driver,
        })
        .await
        .expect("explicit driver before development admission");
    handle
}
fn attach(handle: &SessionHandle) -> ClientCommand {
    ClientCommand::AttachDevelopmentPlugin {
        meta: protocol_meta("plugin-dev", "attach"),
        session_id: handle.session_id().clone(),
        source: "/fixture".into(),
    }
}
async fn entered(controller: &CallbackGeneration) {
    tokio::time::timeout(Duration::from_secs(2), controller.entered.notified())
        .await
        .expect("actor remains responsive to retirement callback");
}

#[tokio::test]
async fn generation_preparation_services_callbacks_and_publishes_after_settlement() {
    let root = tempfile::TempDir::new().expect("root");
    let controller = Arc::new(CallbackGeneration::default());
    let handle = fixture(&controller, root.path()).await;
    let caller = tokio::spawn({
        let handle = handle.clone();
        async move { handle.dispatch_durably(attach(&handle)).await }
    });
    entered(&controller).await;
    assert!(!caller.is_finished());
    assert!(!controller.publication.0.load(Ordering::Acquire));
    handle
        .snapshot()
        .await
        .expect("actor snapshot while preparation waits");
    controller.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), caller)
        .await
        .expect("completion")
        .expect("task")
        .expect("publication");
    assert!(controller.finished.load(Ordering::Acquire));
    assert!(controller.publication.0.load(Ordering::Acquire));
    handle.close().await.expect("settled");
}

#[tokio::test]
async fn dropped_generation_waiter_does_not_revoke_owned_preparation() {
    let root = tempfile::TempDir::new().expect("root");
    let controller = Arc::new(CallbackGeneration::default());
    let handle = fixture(&controller, root.path()).await;
    let caller = tokio::spawn({
        let handle = handle.clone();
        async move { handle.dispatch_durably(attach(&handle)).await }
    });
    entered(&controller).await;
    caller.abort();
    assert!(caller.await.expect_err("aborted").is_cancelled());
    controller.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !controller.publication.0.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actor owns publication independently of caller");
    handle.close().await.expect("settled");
}

#[tokio::test]
async fn takeover_rejects_prepared_generation_and_keeps_publication_closed() {
    let root = tempfile::TempDir::new().expect("root");
    let controller = Arc::new(CallbackGeneration::default());
    let handle = fixture(&controller, root.path()).await;
    let caller = tokio::spawn({
        let handle = handle.clone();
        async move { handle.dispatch_durably(attach(&handle)).await }
    });
    entered(&controller).await;
    handle
        .dispatch(ClientCommand::TakeDriver {
            meta: protocol_meta("replacement", "takeover"),
            session_id: handle.session_id().clone(),
        })
        .await
        .expect("takeover remains responsive");
    controller.release.notify_one();
    assert!(caller.await.expect("caller").is_err());
    assert!(controller.finished.load(Ordering::Acquire));
    assert!(!controller.publication.0.load(Ordering::Acquire));
    assert!(
        handle.close().await.is_err(),
        "retired unpublished generation cannot silently reopen"
    );
}
