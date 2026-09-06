use super::fixtures::{
    models::ScriptedModel,
    support::{config, protocol_meta},
};
use crate::engine::{
    builtin_hook_dispatcher,
    commands::{
        SessionCommandAction, SessionCommandContext, SessionCommandOutput, builtin_command_registry,
    },
    session::{PluginSessionCapability, SessionHandle},
};
use async_trait::async_trait;
use rw_ext::{CommandDescriptor, CommandExecutionError, CommandHandler, CommandInvocation};
use rw_tools::ToolRegistry;
use rw_types::{
    ClientCommand, CommandOutcome, EngineEvent, SequenceId,
    config::PermissionDecision,
    extension_control::{ExtensionControl, ExtensionControlOutcome, SessionNavigationTarget},
};
use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default)]
struct NavigateCallback {
    capability: OnceLock<PluginSessionCapability>,
    admitted: Notify,
    release: Notify,
}
#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for NavigateCallback {
    async fn execute(
        &self,
        _: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        let outcome = self
            .capability
            .get()
            .expect("bound")
            .control(
                invocation.origin().cloned(),
                ExtensionControl::Navigate {
                    target: SessionNavigationTarget::Transcript {
                        sequence: SequenceId(0),
                    },
                },
            )
            .await
            .expect("owned callback");
        assert!(matches!(outcome, ExtensionControlOutcome::Applied {}));
        self.admitted.notify_one();
        self.release.notified().await;
        Ok(SessionCommandOutput {
            message: "navigation complete".into(),
            action: SessionCommandAction::None,
        })
    }
}
async fn actor(root: &std::path::Path, callback: &Arc<NavigateCallback>) -> SessionHandle {
    let mut configuration = config(
        root,
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    let mut commands = builtin_command_registry().expect("builtins");
    commands
        .register_shared(
            CommandDescriptor::new("navigate-fixture", "Navigate through host control"),
            callback.clone(),
        )
        .expect("registry");
    configuration.commands = Arc::new(commands);
    let handle = crate::engine::tests::fixtures::history::spawn(configuration)
        .await
        .expect("actor");
    callback
        .capability
        .set(
            handle
                .plugin_session_capability("navigator")
                .expect("capability"),
        )
        .expect("bind");
    handle
}

#[tokio::test]
async fn navigation_waits_for_command_settlement_and_is_revoked_by_driver_takeover() {
    for takeover in [false, true] {
        let root = tempfile::tempdir().expect("root");
        let callback = Arc::new(NavigateCallback::default());
        let handle = actor(root.path(), &callback).await;
        let mut events = handle.subscribe().expect("events");
        let caller = tokio::spawn({
            let handle = handle.clone();
            async move { handle.send_message("/navigate-fixture").await }
        });
        tokio::time::timeout(Duration::from_secs(2), callback.admitted.notified())
            .await
            .expect("callback");
        while let Ok(event) = events.receiver.try_recv() {
            assert!(
                !matches!(
                    event.as_ref().clone(),
                    EngineEvent::SessionNavigationRequested { .. }
                ),
                "navigation must wait for handler settlement"
            );
        }
        if takeover {
            assert_eq!(
                handle
                    .dispatch(ClientCommand::TakeDriver {
                        meta: protocol_meta("replacement", "take-driver"),
                        session_id: handle.session_id().clone()
                    })
                    .await
                    .expect("takeover"),
                CommandOutcome::Accepted {}
            );
        }
        callback.release.notify_one();
        let completion = caller.await.expect("task");
        assert_eq!(completion.is_ok(), !takeover);
        let mut navigations = 0;
        while let Ok(event) = events.receiver.try_recv() {
            if let EngineEvent::SessionNavigationRequested { meta, target, .. } =
                event.as_ref().clone()
            {
                assert_eq!(meta.client_id.0, "local");
                assert_eq!(
                    target,
                    SessionNavigationTarget::Transcript {
                        sequence: SequenceId(0)
                    }
                );
                navigations += 1;
            }
        }
        assert_eq!(navigations, usize::from(!takeover));
        handle.close().await.expect("close");
    }
}

#[tokio::test]
async fn builtin_navigation_uses_the_control_contract_and_rejects_unowned_or_future_requests() {
    let root = tempfile::tempdir().expect("root");
    let callback = Arc::new(NavigateCallback::default());
    let handle = actor(root.path(), &callback).await;
    let mut events = handle.subscribe().expect("events");
    assert!(
        callback
            .capability
            .get()
            .expect("capability")
            .control(
                None,
                ExtensionControl::Navigate {
                    target: SessionNavigationTarget::Transcript {
                        sequence: SequenceId(0)
                    }
                }
            )
            .await
            .is_err()
    );
    handle
        .send_message("/goto session selected-session")
        .await
        .expect("builtin session");
    let mut found = false;
    while let Ok(event) = events.receiver.try_recv() {
        if let EngineEvent::SessionNavigationRequested { target, .. } = event.as_ref().clone() {
            assert_eq!(
                target,
                SessionNavigationTarget::Session {
                    session_id: rw_types::SessionId("selected-session".into())
                }
            );
            found = true;
        }
    }
    assert!(found);
    for input in [
        "/goto sequence 18446744073709551615",
        "/goto sequence 00",
        "/goto session ../foreign",
    ] {
        assert!(handle.send_message(input).await.is_err(), "reject {input}");
    }
    handle.close().await.expect("close");
}
