//! Explicit actor closure retains failed generations instead of releasing their resources.
use rw_types::hook_contract::{HookInput, HookSessionInput};

use std::{future::pending, sync::Arc, time::Duration};

use futures_util::FutureExt;
use rw_ext::HookEvent;
use rw_tools::CancellationToken;
use tokio::sync::{mpsc, watch};

use super::session::ActorCommand;
use super::{
    AgentLoopError, PendingEvent, SessionActorConfig, TurnSignal, hook_event_name, persist_event,
};

const SHUTDOWN_PROOF_TIMEOUT: Duration = Duration::from_secs(30);
type Proof = Result<(), Arc<str>>;
pub(super) struct ActorControl {
    pub(super) active_turn: Arc<std::sync::atomic::AtomicU64>,
    pub(super) command_descriptors: Arc<std::sync::RwLock<Arc<[rw_ext::CommandDescriptor]>>>,
    pub(super) mode_registry: Arc<std::sync::RwLock<Arc<rw_ext::ModeRegistry>>>,
    pub(super) shutdown: ActorShutdown,
}

pub(super) type Cleanup = tokio::task::JoinHandle<Result<(), String>>;

#[derive(Clone)]
pub(super) struct ActorShutdown {
    cancellation: CancellationToken,
    completion: watch::Sender<Option<Proof>>,
}

impl Default for ActorShutdown {
    fn default() -> Self {
        Self {
            cancellation: CancellationToken::default(),
            completion: watch::channel(None).0,
        }
    }
}

impl ActorShutdown {
    pub(super) fn requested(&self) -> bool {
        self.cancellation.is_cancelled()
    }
    pub(super) async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }
    pub(super) fn complete(&self, result: Result<(), String>) {
        self.completion.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(result.map_err(Arc::from));
            true
        });
    }
    pub(super) async fn close(&self) -> Result<(), AgentLoopError> {
        let mut completion = self.completion.subscribe();
        self.cancellation.cancel();
        let deadline = tokio::time::Instant::now() + SHUTDOWN_PROOF_TIMEOUT;
        loop {
            if let Some(result) = completion.borrow_and_update().clone() {
                return result
                    .map_err(|message| AgentLoopError::EffectsUnsettled(message.to_string()));
            }
            tokio::select! {
                changed = completion.changed() => changed.map_err(|_| AgentLoopError::EffectsUnsettled("actor close proof owner disappeared".to_owned()))?,
                () = tokio::time::sleep_until(deadline) => self.complete(Err("session actor did not acknowledge shutdown before proof deadline".to_owned())),
            }
        }
    }
}

pub(super) async fn retain_unproven<T: Send>(owners: T) {
    pending::<()>().await;
    drop(owners);
}

pub(super) async fn deadline(started: Option<tokio::time::Instant>) {
    if let Some(started) = started {
        tokio::time::sleep_until(started + SHUTDOWN_PROOF_TIMEOUT).await;
    } else {
        pending::<()>().await;
    }
}

pub(super) async fn cleanup_result(cleanup: &mut Option<Cleanup>) -> Result<(), String> {
    match cleanup {
        Some(task) => task
            .await
            .map_err(|error| format!("session cleanup owner failed: {error}"))?,
        None => pending().await,
    }
}

pub(super) fn admit_internal(command: ActorCommand) -> Option<ActorCommand> {
    match command {
        ActorCommand::SendMessage { respond, .. }
        | ActorCommand::PluginInjectMessage { respond, .. } => {
            let _ = respond.send(Err(AgentLoopError::InvalidConfiguration(
                "session is closing".to_owned(),
            )));
            None
        }
        command => Some(command),
    }
}

pub(super) fn start_cleanup(
    config: Arc<SessionActorConfig>,
    signals: mpsc::UnboundedSender<TurnSignal>,
    initial_failure: Option<String>,
) -> Cleanup {
    tokio::spawn(async move {
        let retained = Arc::clone(&config);
        let result = std::panic::AssertUnwindSafe(cleanup(&config, &signals, initial_failure))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err("session cleanup panicked before proof".to_owned()));
        if result.is_err() {
            tokio::spawn(retain_unproven(retained));
        }
        result
    })
}

async fn cleanup(
    config: &SessionActorConfig,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    initial_failure: Option<String>,
) -> Result<(), String> {
    let mut failure = initial_failure;
    if let Err(error) = config.model.settle_effects().await {
        failure = Some(error.to_string());
    }
    for descriptor in config.tools.descriptors() {
        if let Some(tool) = config.tools.resolve(&descriptor.name)
            && let Err(error) = tool.settle_effects().await
        {
            failure.get_or_insert_with(|| error.to_string());
        }
    }
    for event in [
        HookEvent::SessionStart,
        HookEvent::SessionEnd,
        HookEvent::UserPromptSubmit,
        HookEvent::PreTool,
        HookEvent::PostTool,
        HookEvent::PreCompact,
        HookEvent::TurnEnd,
        HookEvent::PermissionCheck,
    ] {
        if let Err(error) = config.hooks.settle_effects(event).await {
            failure.get_or_insert_with(|| error.to_string());
        }
    }
    if let Err(error) = config.event_sink.settle_effects().await {
        failure.get_or_insert_with(|| error.to_string());
    }
    if let Err(error) = config.checkpoints.settle_effects().await {
        failure.get_or_insert_with(|| error.to_string());
    }
    if failure.is_some() {
        return failure.map_or(Ok(()), Err);
    }
    let hooks = config
        .hooks
        .dispatch(HookInput::SessionEnd(HookSessionInput {
            session_id: config.session_id.0.clone(),
            workspace: config.workspace_root.to_string_lossy().into_owned(),
        }))
        .await
        .map_err(|error| error.to_string())?;
    for hook in hooks.failures() {
        if let Err(error) = persist_event(
            signals,
            PendingEvent::HookFailure {
                event: hook_event_name(HookEvent::SessionEnd).to_owned(),
                hook_id: hook.hook_id().to_owned(),
                fail_closed: hook.policy() == rw_ext::HookFailurePolicy::FailClosed,
                message: config.secret_redactor.redact(&hook.error().to_string()),
            },
        )
        .await
        {
            failure.get_or_insert_with(|| error.to_string());
        }
    }
    if let Err(error) = config.hooks.settle_effects(HookEvent::SessionEnd).await {
        failure.get_or_insert_with(|| error.to_string());
    }
    if let Err(error) = config.tools.end_session(&config.session_id).await {
        failure.get_or_insert_with(|| error.to_string());
    }
    if let Err(error) = config.event_sink.settle_effects().await {
        failure.get_or_insert_with(|| error.to_string());
    }
    if let Err(error) = config.checkpoints.settle_effects().await {
        failure.get_or_insert_with(|| error.to_string());
    }
    if let Some(failure) = failure {
        return Err(failure);
    }
    config
        .extension_development
        .shutdown()
        .await
        .map_err(|error| error.to_string())?;
    if let Err(error) = config.resources.shutdown().await {
        failure.get_or_insert_with(|| error.to_string());
    }
    failure.map_or(Ok(()), Err)
}
