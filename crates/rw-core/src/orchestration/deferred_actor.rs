//! Inactive child records own a resume recipe; actor work starts only with a turn.
use super::sessions::{
    ActorResumeBuilder, ActorSubagentSession, apply_child_policy, bind_child_tools,
};
use super::{
    OrchestrationError, SubagentProgressObserver, SubagentRecoveryPolicy, SubagentSession,
    SubagentTurnResult,
};
use async_trait::async_trait;
use rw_tools::{CancellationToken, ToolRegistry};
use rw_types::{DiffArtifact, SessionId};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

pub(super) const POLICY_BUDGET: usize = 32 * 1024 * 1024;
pub(super) fn admit_policy(
    budget: &Arc<Semaphore>,
    session: &SessionId,
    workspace: &std::path::Path,
    policy: &SubagentRecoveryPolicy,
) -> Result<OwnedSemaphorePermit, OrchestrationError> {
    // The recipe's strings are cloned only after admission. Reserve both the
    // recipe and the policy copies used while its blocking builder runs.
    let bytes = std::mem::size_of::<Recipe>()
        .saturating_add(session.0.len())
        .saturating_add(workspace.as_os_str().len())
        .saturating_add(policy.model_alias.len())
        .saturating_add(policy.system_prompt.as_ref().map_or(0, String::len))
        .saturating_mul(3);
    let charge = u32::try_from(bytes).map_err(|_| policy_exhausted())?;
    budget
        .clone()
        .try_acquire_many_owned(charge)
        .map_err(|_| policy_exhausted())
}
fn policy_exhausted() -> OrchestrationError {
    OrchestrationError::Session("child recovery policy allocation budget exhausted".into())
}

pub(super) struct DeferredActorSession {
    id: SessionId,
    state: Arc<Mutex<State>>,
}
struct State {
    closing: bool,
    phase: Phase,
}
enum Phase {
    Dormant(Arc<Recipe>),
    Starting {
        _recipe: Arc<Recipe>,
        completion: watch::Receiver<bool>,
    },
    Live(Arc<ActorSubagentSession>),
    Failed {
        _recipe: Arc<Recipe>,
        reason: String,
    },
    Closed,
}
struct Recipe {
    builder: Arc<ActorResumeBuilder>,
    session: SessionId,
    workspace: PathBuf,
    tools: Option<Arc<ToolRegistry>>,
    policy: SubagentRecoveryPolicy,
    _policy_permit: OwnedSemaphorePermit,
}
impl Recipe {
    async fn prepare(&self) -> Result<crate::SessionActorConfig, OrchestrationError> {
        let mut config = (self.builder)(&self.session, &self.workspace, &self.policy)
            .await
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        config.session_id.clone_from(&self.session);
        config.workspace_root.clone_from(&self.workspace);
        config.additional_workspace_roots.clear();
        if let Some(tools) = &self.tools {
            config.tools = Arc::new(bind_child_tools(&config.tools, tools)?);
        }
        apply_child_policy(
            &mut config,
            &self.policy.model_alias,
            self.policy.system_prompt.as_deref(),
            self.policy.permission_mode,
            self.policy.max_turns,
        );
        Ok(config)
    }
}
impl DeferredActorSession {
    pub(super) fn new(
        builder: Arc<ActorResumeBuilder>,
        session: SessionId,
        workspace: PathBuf,
        tools: Option<Arc<ToolRegistry>>,
        policy: SubagentRecoveryPolicy,
        policy_permit: OwnedSemaphorePermit,
    ) -> Self {
        Self {
            id: session.clone(),
            state: Arc::new(Mutex::new(State {
                closing: false,
                phase: Phase::Dormant(Arc::new(Recipe {
                    builder,
                    session,
                    workspace,
                    tools,
                    policy,
                    _policy_permit: policy_permit,
                })),
            })),
        }
    }
    async fn live(
        &self,
        start: bool,
    ) -> Result<Option<Arc<ActorSubagentSession>>, OrchestrationError> {
        loop {
            let mut completion = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if start && state.closing {
                    return Err(closed());
                }
                match &state.phase {
                    Phase::Live(session) => return Ok(Some(session.clone())),
                    Phase::Dormant(_) if !start => return Ok(None),
                    Phase::Dormant(recipe) => {
                        let recipe = recipe.clone();
                        let (done, completion) = watch::channel(false);
                        state.phase = Phase::Starting {
                            _recipe: recipe.clone(),
                            completion: completion.clone(),
                        };
                        spawn_preparation(self.state.clone(), recipe, done);
                        completion
                    }
                    Phase::Starting { completion, .. } => completion.clone(),
                    Phase::Failed { reason, .. } => {
                        return Err(OrchestrationError::EffectsUnsettled(reason.clone()));
                    }
                    Phase::Closed => return if start { Err(closed()) } else { Ok(None) },
                }
            };
            while !*completion.borrow_and_update() {
                completion.changed().await.map_err(|_| {
                    OrchestrationError::EffectsUnsettled(
                        "child preparation owner exited without settlement".into(),
                    )
                })?;
            }
        }
    }
}
fn spawn_preparation(state: Arc<Mutex<State>>, recipe: Arc<Recipe>, done: watch::Sender<bool>) {
    tokio::spawn(async move {
        // The task owns preparation even when its initiating run/cancel/close caller drops.
        let builder = recipe.clone();
        let result = rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            tokio::runtime::Handle::current().block_on(builder.prepare())
        })
        .await
        .map_err(|error| {
            OrchestrationError::EffectsUnsettled(format!("child preparation failed: {error}"))
        })
        .and_then(std::convert::identity)
        .and_then(|config| {
            crate::SessionActor::spawn(config)
                .map_err(|error| OrchestrationError::Session(error.to_string()))
        });
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.phase = match result {
            Ok(handle) => Phase::Live(Arc::new(ActorSubagentSession { handle })),
            Err(error) => Phase::Failed {
                _recipe: recipe,
                reason: error.to_string(),
            },
        };
        done.send_replace(true);
    });
}
fn closed() -> OrchestrationError {
    OrchestrationError::Session("child session is closed".into())
}
#[async_trait]
impl SubagentSession for DeferredActorSession {
    fn session_id(&self) -> &SessionId {
        &self.id
    }
    async fn run_turn(
        &self,
        prompt: String,
        cancellation: CancellationToken,
        progress: Arc<dyn SubagentProgressObserver>,
    ) -> Result<SubagentTurnResult, OrchestrationError> {
        let session = self.live(true).await?.ok_or_else(closed)?;
        if cancellation.is_cancelled() {
            return Err(OrchestrationError::Session(
                "child turn cancelled before activation".into(),
            ));
        }
        session.run_turn(prompt, cancellation, progress).await
    }
    async fn cancel(&self) -> Result<(), OrchestrationError> {
        if let Some(session) = self.live(false).await? {
            session.cancel().await?;
        }
        Ok(())
    }
    async fn close(&self, artifact: Option<&DiffArtifact>) -> Result<(), OrchestrationError> {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closing = true;
            if matches!(state.phase, Phase::Dormant(_) | Phase::Closed) {
                state.phase = Phase::Closed;
                return Ok(());
            }
        }
        if let Some(session) = self.live(false).await? {
            session.close(artifact).await?;
        }
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .phase = Phase::Closed;
        Ok(())
    }
}
