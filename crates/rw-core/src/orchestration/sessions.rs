use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use rw_tools::{
    CancellationToken, ToolRegistry, WorkspaceBinding, WorktreeIsolation, WorktreeLease,
    WorktreeLeaseRecord,
};
use rw_types::{
    Block, DiffArtifact, EngineEvent, Role, SessionId, SessionMode, SubagentIsolation, Turn,
    TurnMeta,
};

use crate::{AgentLoopError, SessionActor, SessionActorConfig, SessionHandle};

use super::{
    OrchestrationError, SubagentLaunch, SubagentProgressObserver, SubagentRecoveryPolicy,
    SubagentSession, SubagentSessionFactory, SubagentTurnResult, subagent_status,
};

/// Factory for production child actors. The builder supplies a distinct event
/// sink and context; core overwrites security-sensitive launch fields.
pub(super) type ActorConfigBuilder = dyn for<'a> Fn(
        &'a SubagentLaunch,
    )
        -> futures_util::future::BoxFuture<'a, Result<SessionActorConfig, AgentLoopError>>
    + Send
    + Sync;
pub(super) type ActorResumeBuilder = dyn for<'a> Fn(
        &'a SessionId,
        &'a Path,
        &'a SubagentRecoveryPolicy,
    )
        -> futures_util::future::BoxFuture<'a, Result<SessionActorConfig, AgentLoopError>>
    + Send
    + Sync;

pub(super) type DormantControlsReader = dyn for<'a> Fn(
        &'a SessionId,
        &'a Path,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<super::DormantChildControls, AgentLoopError>,
    > + Send
    + Sync;

pub struct ActorSubagentSessionFactory {
    builder: Arc<ActorConfigBuilder>,
    rebuilder: Option<(Arc<ActorResumeBuilder>, Arc<DormantControlsReader>)>,
    recovery_policies: Arc<tokio::sync::Semaphore>,
}

/// Isolation wrapper for the production actor factory. A lease remains bound
/// to the continuable child session; each completed turn refreshes its typed
/// diff artifact without mutating the parent tree.
pub struct WorktreeSubagentSessionFactory {
    inner: Arc<dyn SubagentSessionFactory>,
    isolation: Arc<WorktreeIsolation>,
}

impl WorktreeSubagentSessionFactory {
    #[must_use]
    pub fn new(inner: Arc<dyn SubagentSessionFactory>, isolation: Arc<WorktreeIsolation>) -> Self {
        Self { inner, isolation }
    }
}

#[async_trait]
impl SubagentSessionFactory for WorktreeSubagentSessionFactory {
    async fn create(
        &self,
        mut launch: SubagentLaunch,
    ) -> Result<Arc<dyn SubagentSession>, OrchestrationError> {
        if launch.request.isolation == SubagentIsolation::Shared {
            return self.inner.create(launch).await;
        }
        let allocation = self
            .isolation
            .create(launch.cancellation.clone())
            .await
            .map_err(creation_error)?;
        launch.workspace_root = allocation.lease().path().to_path_buf();
        let inner = match self.inner.create(launch).await {
            Ok(inner) => inner,
            Err(error) => {
                if matches!(error, OrchestrationError::EffectsUnsettled(_)) {
                    return Err(error);
                }
                if let Err(cleanup) = allocation.rollback().await {
                    return Err(OrchestrationError::EffectsUnsettled(format!(
                        "{error}; {cleanup}"
                    )));
                }
                return Err(error);
            }
        };
        Ok(Arc::new(WorktreeSubagentSession {
            inner,
            isolation: Arc::clone(&self.isolation),
            lease: Arc::new(allocation.commit()),
        }))
    }

    async fn rebind(
        &self,
        session_id: &SessionId,
        workspace_root: Option<&Path>,
        worktree: Option<&WorktreeLeaseRecord>,
        allowed_tools: Option<Arc<ToolRegistry>>,
        policy: &SubagentRecoveryPolicy,
    ) -> Result<Option<Arc<dyn SubagentSession>>, OrchestrationError> {
        let Some(record) = worktree else {
            return self
                .inner
                .rebind(session_id, workspace_root, None, allowed_tools, policy)
                .await;
        };
        let lease = Arc::new(
            self.isolation
                .rebind(record, CancellationToken::default())
                .await
                .map_err(|error| OrchestrationError::Session(error.to_string()))?,
        );
        let Some(inner) = self
            .inner
            .rebind(session_id, Some(lease.path()), None, allowed_tools, policy)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(Arc::new(WorktreeSubagentSession {
            inner,
            isolation: Arc::clone(&self.isolation),
            lease,
        })))
    }
}

pub(super) struct WorktreeSubagentSession {
    inner: Arc<dyn SubagentSession>,
    isolation: Arc<WorktreeIsolation>,
    lease: Arc<WorktreeLease>,
}

#[async_trait]
impl SubagentSession for WorktreeSubagentSession {
    fn control_summary(&self) -> rw_types::family_controls::ChildControlSummary {
        self.inner.control_summary()
    }
    async fn child_state(
        &self,
    ) -> Result<rw_types::session_state::SessionStateSnapshot, OrchestrationError> {
        self.inner.child_state().await
    }
    async fn child_controls(
        &self,
    ) -> Result<rw_types::family_controls::ChildControlsSnapshot, OrchestrationError> {
        self.inner.child_controls().await
    }
    async fn respond_control(
        &self,
        authority: crate::FamilyControlAuthority,
        meta: rw_types::CommandMeta,
        revision: rw_types::SequenceId,
        response: rw_types::family_controls::ChildControlResponse,
    ) -> Result<rw_types::CommandOutcome, OrchestrationError> {
        self.inner
            .respond_control(authority, meta, revision, response)
            .await
    }

    fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }

    async fn run_turn(
        &self,
        prompt: String,
        cancellation: CancellationToken,
        progress: Arc<dyn SubagentProgressObserver>,
    ) -> Result<SubagentTurnResult, OrchestrationError> {
        let mut result = self
            .inner
            .run_turn(prompt, cancellation.clone(), progress)
            .await?;
        let artifact = self
            .isolation
            .collect(
                &self.lease,
                &result.final_text,
                result.usage.clone(),
                result.cost.clone(),
                cancellation,
            )
            .await
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        result.final_text = artifact.final_text;
        result.touched_files = artifact
            .touched_files
            .iter()
            .map(|file| file.path.clone())
            .collect();
        result.diff_artifact = artifact.diff;
        Ok(result)
    }

    async fn cancel(&self) -> Result<(), OrchestrationError> {
        self.inner.cancel().await
    }

    fn worktree_record(&self) -> Option<WorktreeLeaseRecord> {
        Some(self.lease.durable_record())
    }

    async fn close(
        &self,
        durable_artifact: Option<&DiffArtifact>,
    ) -> Result<(), OrchestrationError> {
        self.inner.close(None).await?;
        let removed = if let Some(artifact) = durable_artifact {
            self.isolation
                .finalize_captured(&self.lease, artifact, CancellationToken::default())
                .await
                .map_err(|error| OrchestrationError::Session(error.to_string()))?
        } else {
            self.isolation
                .cleanup_if_untouched(&self.lease, CancellationToken::default())
                .await
                .map_err(|error| OrchestrationError::Session(error.to_string()))?
        };
        if !removed {
            return Err(OrchestrationError::Session(
                "worktree changed after its latest durable artifact; child was not closed"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

impl ActorSubagentSessionFactory {
    #[must_use]
    pub fn new(
        builder: impl for<'a> Fn(
            &'a SubagentLaunch,
        ) -> futures_util::future::BoxFuture<
            'a,
            Result<SessionActorConfig, AgentLoopError>,
        > + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            builder: Arc::new(builder),
            rebuilder: None,
            recovery_policies: Arc::new(tokio::sync::Semaphore::new(
                super::deferred_actor::POLICY_BUDGET,
            )),
        }
    }

    /// Adds the host-specific recovery builder. It must reopen the child log
    /// and rebuild every dependency bound to the supplied root.
    #[must_use]
    pub fn with_rebuilder(
        mut self,
        rebuilder: impl for<'a> Fn(
            &'a SessionId,
            &'a Path,
            &'a SubagentRecoveryPolicy,
        ) -> futures_util::future::BoxFuture<
            'a,
            Result<SessionActorConfig, AgentLoopError>,
        > + Send
        + Sync
        + 'static,
        controls: impl for<'a> Fn(
            &'a SessionId,
            &'a Path,
        ) -> futures_util::future::BoxFuture<
            'a,
            Result<super::DormantChildControls, AgentLoopError>,
        > + Send
        + Sync
        + 'static,
    ) -> Self {
        self.rebuilder = Some((Arc::new(rebuilder), Arc::new(controls)));
        self
    }
}

#[async_trait]
impl SubagentSessionFactory for ActorSubagentSessionFactory {
    async fn create(
        &self,
        launch: SubagentLaunch,
    ) -> Result<Arc<dyn SubagentSession>, OrchestrationError> {
        let mut config = (self.builder)(&launch)
            .await
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        config.session_id = launch.handle.session_id.clone();
        config.workspace_root.clone_from(&launch.workspace_root);
        config.additional_workspace_roots.clear();
        config.model_alias.clone_from(&launch.request.model);
        config.tools = Arc::new(bind_child_tools(&config.tools, &launch.tools)?);
        apply_child_policy(
            &mut config,
            &launch.request.model,
            launch.request.system_prompt.as_deref(),
            launch.request.permission_mode,
            launch.max_turns,
        );
        let handle = SessionActor::spawn(config)
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        Ok(Arc::new(ActorSubagentSession { handle }))
    }

    async fn rebind(
        &self,
        session_id: &SessionId,
        workspace_root: Option<&Path>,
        _worktree: Option<&WorktreeLeaseRecord>,
        allowed_tools: Option<Arc<ToolRegistry>>,
        policy: &SubagentRecoveryPolicy,
    ) -> Result<Option<Arc<dyn SubagentSession>>, OrchestrationError> {
        let (Some((rebuilder, controls)), Some(workspace_root)) = (&self.rebuilder, workspace_root)
        else {
            return Ok(None);
        };
        let policy_permit = super::deferred_actor::admit_policy(
            &self.recovery_policies,
            session_id,
            workspace_root,
            policy,
        )?;
        let controls = controls(session_id, workspace_root)
            .await
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        if controls.session_id != *session_id {
            return Err(OrchestrationError::Session(
                "child control source belongs to another session".into(),
            ));
        }
        Ok(Some(Arc::new(
            super::deferred_actor::DeferredActorSession::new(
                Arc::clone(rebuilder),
                session_id.clone(),
                workspace_root.to_path_buf(),
                allowed_tools,
                policy.clone(),
                policy_permit,
                controls,
            ),
        )))
    }
}

pub(super) fn apply_child_policy(
    config: &mut SessionActorConfig,
    model_alias: &str,
    system_prompt: Option<&str>,
    permission_mode: SessionMode,
    max_turns: usize,
) {
    model_alias.clone_into(&mut config.model_alias);
    config.max_turns = config.max_turns.min(max_turns).max(1);
    config.recovered.mode = permission_mode;
    config.recovered.plan_gate_active = permission_mode == SessionMode::Plan;
    let mode_prompt = match permission_mode {
        SessionMode::Discuss => {
            "Child permission mode: discuss. Use only read-only tools and do not mutate the workspace."
        }
        SessionMode::Plan => {
            "Child permission mode: plan. Use only read-only tools and return a structured plan."
        }
        SessionMode::Execute => {
            "Child permission mode: execute. Use the exact tool grant selected by the parent and the parent session's effective permission policy."
        }
    };
    if let Some(system_prompt) = system_prompt.filter(|prompt| !prompt.trim().is_empty()) {
        if let Some(system) = config
            .initial_session_context
            .iter_mut()
            .find(|turn| turn.role == Role::System)
        {
            system.blocks.push(Block::Text {
                text: system_prompt.to_owned(),
            });
        } else {
            config.initial_session_context.insert(
                0,
                Turn {
                    role: Role::System,
                    blocks: vec![Block::Text {
                        text: system_prompt.to_owned(),
                    }],
                    meta: TurnMeta::default(),
                },
            );
        }
    }
    if let Some(system) = config
        .initial_session_context
        .iter_mut()
        .find(|turn| turn.role == Role::System)
    {
        system.blocks.push(Block::Text {
            text: mode_prompt.to_owned(),
        });
    }
}

pub(super) fn bind_child_tools(
    root_bound: &ToolRegistry,
    allowed: &ToolRegistry,
) -> Result<ToolRegistry, OrchestrationError> {
    let mut child = ToolRegistry::new();
    for approved in allowed.descriptors() {
        let tool = if let Some(tool) = root_bound.resolve(&approved.name) {
            tool
        } else {
            let fallback = allowed.resolve(&approved.name).ok_or_else(|| {
                OrchestrationError::InvalidRequest(format!(
                    "approved child tool `{}` disappeared during binding",
                    approved.name
                ))
            })?;
            if fallback.workspace_binding() != WorkspaceBinding::RootIndependent {
                return Err(OrchestrationError::InvalidRequest(format!(
                    "root-bound child tool `{}` was not rebuilt for the child workspace",
                    approved.name
                )));
            }
            fallback
        };
        let actual = tool.descriptor();
        if actual.capabilities != approved.capabilities {
            return Err(OrchestrationError::InvalidRequest(format!(
                "child tool `{}` capability manifest changed during root binding",
                approved.name
            )));
        }
        child
            .register(tool)
            .map_err(|error| OrchestrationError::InvalidRequest(error.to_string()))?;
    }
    Ok(child.with_mcp_tool_policy(allowed.mcp_tool_policy().clone()))
}

pub(super) struct ActorSubagentSession {
    pub(super) handle: SessionHandle,
}

#[async_trait]
impl SubagentSession for ActorSubagentSession {
    fn control_summary(&self) -> rw_types::family_controls::ChildControlSummary {
        self.handle.control_summary()
    }
    async fn child_state(
        &self,
    ) -> Result<rw_types::session_state::SessionStateSnapshot, OrchestrationError> {
        self.handle
            .live_state()
            .await
            .map_err(|error| OrchestrationError::Session(error.to_string()))
    }
    async fn child_controls(
        &self,
    ) -> Result<rw_types::family_controls::ChildControlsSnapshot, OrchestrationError> {
        self.handle
            .child_controls()
            .await
            .map_err(|error| OrchestrationError::Session(error.to_string()))
    }
    async fn respond_control(
        &self,
        authority: crate::FamilyControlAuthority,
        meta: rw_types::CommandMeta,
        revision: rw_types::SequenceId,
        response: rw_types::family_controls::ChildControlResponse,
    ) -> Result<rw_types::CommandOutcome, OrchestrationError> {
        self.handle
            .respond_child_control(authority, meta, revision, response)
            .await
            .map_err(|error| OrchestrationError::Session(error.to_string()))
    }

    fn session_id(&self) -> &SessionId {
        self.handle.session_id()
    }

    async fn run_turn(
        &self,
        prompt: String,
        cancellation: CancellationToken,
        progress: Arc<dyn SubagentProgressObserver>,
    ) -> Result<SubagentTurnResult, OrchestrationError> {
        let mut subscription = self
            .handle
            .subscribe_live()
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        self.handle
            .send_message(prompt)
            .await
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
        let mut final_text = String::new();
        loop {
            let event = tokio::select! {
                () = cancellation.cancelled() => {
                    let _ = self.handle.interrupt().await;
                    return Err(OrchestrationError::Session("cancelled".to_owned()));
                }
                event = subscription.recv() => event
                    .map_err(|error| OrchestrationError::Session(error.to_string()))?,
            };
            let sequence = event.meta().map(|meta| meta.sequence_id.0);
            if let EngineEvent::TextDelta { text, .. } = event.as_ref() {
                let remaining =
                    super::MAX_SUBAGENT_FINAL_TEXT_BYTES.saturating_sub(final_text.len());
                let mut end = text.len().min(remaining);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                final_text.push_str(&text[..end]);
            }
            let encoded = super::progress::encode(sequence, &event)?;
            progress.progress(sequence, encoded).await?;
            if let EngineEvent::TurnFinished {
                status,
                usage,
                cost,
                ..
            } = event.as_ref()
            {
                return Ok(SubagentTurnResult {
                    status: subagent_status(status),
                    final_text,
                    touched_files: Vec::new(),
                    diff_artifact: None,
                    usage: usage.clone(),
                    cost: cost.clone(),
                    turns: 1,
                });
            }
        }
    }

    async fn close(
        &self,
        _durable_artifact: Option<&DiffArtifact>,
    ) -> Result<(), OrchestrationError> {
        self.handle
            .close()
            .await
            .map_err(|error| OrchestrationError::Session(error.to_string()))
    }

    async fn cancel(&self) -> Result<(), OrchestrationError> {
        self.handle
            .interrupt()
            .await
            .map(|_| ())
            .map_err(|error| OrchestrationError::Session(error.to_string()))
    }
}

fn creation_error(error: rw_tools::ToolError) -> OrchestrationError {
    match error {
        rw_tools::ToolError::EffectsUnsettled(reason) => {
            OrchestrationError::EffectsUnsettled(reason)
        }
        error => OrchestrationError::Session(error.to_string()),
    }
}
