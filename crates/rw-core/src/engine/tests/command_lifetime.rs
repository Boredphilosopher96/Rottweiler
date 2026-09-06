use super::fixtures::{
    models::ScriptedModel,
    support::{config, next_matching, protocol_meta},
};
use crate::engine::{
    builtin_hook_dispatcher,
    commands::{SessionCommandAction, SessionCommandContext, SessionCommandOutput},
    pending_event::PendingEvent,
    session::{PluginSessionCapability, SessionHandle},
};
use async_trait::async_trait;
use rw_ext::{
    CommandDescriptor, CommandExecutionError, CommandHandler, CommandInvocation, CommandRegistry,
};
use rw_tools::ToolRegistry;
use rw_types::{
    ClientCommand, CommandOutcome, ModeId, SessionMode,
    config::PermissionDecision,
    extension_control::{ExtensionControl, ExtensionControlOutcome},
};
use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default)]
struct CallbackCommand {
    session: OnceLock<PluginSessionCapability>,
    called_back: Notify,
    release: Notify,
    returned: AtomicBool,
    origin: OnceLock<rw_types::extension_invocation::ExtensionInvocationId>,
}
#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for CallbackCommand {
    async fn execute(
        &self,
        _: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        let session = self.session.get().expect("session bound before invocation");
        session
            .set_status("callback entered")
            .await
            .expect("actor services callback");
        assert!(matches!(
            session
                .control(
                    None,
                    ExtensionControl::SelectMode {
                        mode: ModeId("plan".into())
                    }
                )
                .await
                .expect("policy response"),
            ExtensionControlOutcome::Busy {}
        ));
        let origin = invocation
            .origin()
            .cloned()
            .expect("host invocation identity");
        self.origin
            .set(origin.clone())
            .expect("one admitted fixture invocation");
        assert!(matches!(
            session
                .control(
                    Some(origin),
                    ExtensionControl::SelectMode {
                        mode: ModeId("plan".into())
                    }
                )
                .await
                .expect("same command control"),
            ExtensionControlOutcome::Applied {}
        ));
        self.called_back.notify_one();
        self.release.notified().await;
        session
            .set_status("callback finished")
            .await
            .expect("final callback services");
        self.returned.store(true, Ordering::Release);
        Ok(SessionCommandOutput {
            message: "completed callback".into(),
            action: SessionCommandAction::None,
        })
    }
}

async fn actor(root: &std::path::Path, callback: &Arc<CallbackCommand>) -> SessionHandle {
    let mut configuration = config(
        root,
        Arc::new(ScriptedModel::default()),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    let mut commands = CommandRegistry::new();
    commands
        .register_shared(
            CommandDescriptor::new("callback", "host callback"),
            callback.clone(),
        )
        .expect("command");
    configuration.commands = Arc::new(commands);
    let handle = crate::engine::tests::fixtures::history::spawn(configuration)
        .await
        .expect("actor");
    callback
        .session
        .set(
            handle
                .plugin_session_capability("fixture")
                .expect("capability"),
        )
        .expect("bind once");
    handle
}

#[tokio::test]
async fn command_callback_is_duplex_and_caller_drop_preserves_owned_execution() {
    let root = tempfile::TempDir::new().expect("root");
    let callback = Arc::new(CallbackCommand::default());
    let handle = actor(root.path(), &callback).await;
    let mut events = handle.subscribe().expect("events");
    let caller = tokio::spawn({
        let handle = handle.clone();
        async move { handle.send_message("/callback").await }
    });
    tokio::time::timeout(Duration::from_secs(2), callback.called_back.notified())
        .await
        .expect("real actor callback completes before handler returns");
    assert!(!caller.is_finished());
    assert!(
        handle.send_message("/callback").await.is_err(),
        "only one command can be pending"
    );
    assert_eq!(
        handle.snapshot().await.expect("responsive snapshot").mode,
        SessionMode::Plan
    );
    assert!(
        handle
            .ui_catalog()
            .await
            .expect("responsive catalog")
            .entries
            .is_empty()
    );
    caller.abort();
    assert!(caller.await.expect_err("caller aborted").is_cancelled());
    callback.release.notify_one();
    let event = next_matching(
        &mut events,
        |event| matches!(event, PendingEvent::CommandFinished {name,..} if name == "callback"),
    )
    .await;
    assert!(
        matches!(event.kind, PendingEvent::CommandFinished {message,..} if message == "completed callback")
    );
    assert!(callback.returned.load(Ordering::Acquire));
    assert!(
        callback
            .session
            .get()
            .expect("capability")
            .control(
                callback.origin.get().cloned(),
                ExtensionControl::SelectMode {
                    mode: ModeId("execute".into())
                }
            )
            .await
            .is_err(),
        "retired command origin cannot change state"
    );
    handle.close().await.expect("settled close");
}

#[tokio::test]
async fn driver_takeover_revokes_command_completion_without_dropping_handler() {
    let root = tempfile::TempDir::new().expect("root");
    let callback = Arc::new(CallbackCommand::default());
    let handle = actor(root.path(), &callback).await;
    let caller = tokio::spawn({
        let handle = handle.clone();
        async move { handle.send_message("/callback").await }
    });
    tokio::time::timeout(Duration::from_secs(2), callback.called_back.notified())
        .await
        .expect("callback");
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
    callback.release.notify_one();
    assert!(
        caller.await.expect("caller task").is_err(),
        "retired authority cannot apply an intent"
    );
    assert!(callback.returned.load(Ordering::Acquire));
    handle.close().await.expect("settled close");
}

#[tokio::test]
async fn close_waits_for_command_and_final_host_callback_before_proof() {
    let root = tempfile::TempDir::new().expect("root");
    let callback = Arc::new(CallbackCommand::default());
    let handle = actor(root.path(), &callback).await;
    let caller = tokio::spawn({
        let handle = handle.clone();
        async move { handle.send_message("/callback").await }
    });
    tokio::time::timeout(Duration::from_secs(2), callback.called_back.notified())
        .await
        .expect("callback");
    let mut close = Box::pin(handle.close());
    assert!(futures_util::poll!(close.as_mut()).is_pending());
    assert!(!callback.returned.load(Ordering::Acquire));
    callback.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), close)
        .await
        .expect("close proof deadline")
        .expect("settled close");
    assert!(callback.returned.load(Ordering::Acquire));
    assert!(caller.await.expect("caller task").is_err());
}
