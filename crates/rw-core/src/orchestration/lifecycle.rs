use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    time::Instant,
};

use rw_tools::{
    CancellationToken, CapabilityManifest, McpToolPolicy, ToolRegistry, WorktreeLeaseRecord,
    validate_mcp_virtual_tool,
};
use rw_types::{Cost, SessionId, SubagentDescriptor, SubagentId, SubagentResult, SubagentStatus};
use tokio::sync::{Semaphore, watch};

use super::{
    NoopSubagentMetadataStore, ObserverProgress, OrchestrationError, OrchestratorInner,
    SessionRecord, SessionState, SubagentHandle, SubagentLaunch, SubagentLimits,
    SubagentMetadataStore, SubagentObserver, SubagentOrchestrator, SubagentProgressObserver,
    SubagentRecoveryPhase, SubagentRecoveryRecord, SubagentRequest, SubagentSession,
    SubagentSessionFactory, bound_turn_result, bounded_cancel, control_timeout, ensure_child_owner,
    random_id, restricted_registry, session_record_descriptor, validate_request, zero_usage,
};

impl SubagentOrchestrator {
    /// Builds an orchestrator over the same public registry used by the parent actor.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request error when a configured limit is zero.
    pub fn new(
        limits: SubagentLimits,
        factory: Arc<dyn SubagentSessionFactory>,
        tools: Arc<ToolRegistry>,
        artifact_source: Arc<dyn super::SubagentArtifactSource>,
    ) -> Result<Self, OrchestrationError> {
        if limits.max_concurrency == 0 || limits.max_turns == 0 || limits.max_duration.is_zero() {
            return Err(OrchestrationError::InvalidRequest(
                "concurrency, turn, and duration limits must be greater than zero".to_owned(),
            ));
        }
        let weak_tools = Arc::downgrade(&tools);
        Ok(Self {
            inner: Arc::new(OrchestratorInner {
                limits,
                startups: super::startup::Startups::new(limits.max_concurrency),
                factory,
                base_tools: tools,
                tools: RwLock::new(weak_tools),
                permits: Arc::new(Semaphore::new(limits.max_concurrency)),
                retained: Arc::new(Semaphore::new(super::MAX_RETAINED_SUBAGENTS)),
                sequence: std::sync::atomic::AtomicU64::new(0),
                sessions: Mutex::new(HashMap::new()),
                session_depths: Mutex::new(HashMap::new()),
                diff_artifact_authority: artifact_source,
                metadata: RwLock::new(Arc::new(NoopSubagentMetadataStore)),
            }),
        })
    }

    #[must_use]
    pub fn limits(&self) -> SubagentLimits {
        self.inner.limits
    }

    /// Binds the final registry after cyclic orchestration tools are added.
    /// Future children inherit it, allowing safe nested spawning.
    pub fn bind_tools(&self, tools: Arc<ToolRegistry>) {
        let weak_tools = Arc::downgrade(&tools);
        drop(tools);
        *self
            .inner
            .tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = weak_tools;
    }

    /// Installs atomic host-private continuation metadata persistence.
    pub fn bind_metadata_store(&self, store: Arc<dyn SubagentMetadataStore>) {
        *self
            .inner
            .metadata
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = store;
    }

    fn tool_registry(&self) -> Arc<ToolRegistry> {
        self.inner
            .tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .upgrade()
            .unwrap_or_else(|| Arc::clone(&self.inner.base_tools))
    }

    /// Shared provenance authority for the registered `apply_worktree_diff` tool.
    #[must_use]
    pub fn diff_artifact_authority(&self) -> Arc<dyn rw_tools::DiffArtifactAuthority> {
        Arc::clone(&self.inner.diff_artifact_authority) as Arc<dyn rw_tools::DiffArtifactAuthority>
    }

    fn prepare_launch(
        &self,
        parent_session_id: SessionId,
        request: SubagentRequest,
        cancellation: CancellationToken,
    ) -> Result<SubagentLaunch, OrchestrationError> {
        validate_request(&request)?;
        let parent_depth = self
            .inner
            .session_depths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&parent_session_id)
            .copied()
            .unwrap_or(0);
        let depth = parent_depth.saturating_add(1);
        if depth > self.inner.limits.max_depth {
            return Err(OrchestrationError::DepthExceeded {
                requested: depth,
                maximum: self.inner.limits.max_depth,
            });
        }
        let tools = restricted_registry(
            &self.tool_registry(),
            &request.tools,
            request.permission_mode,
        )?;
        let ordinal = self
            .inner
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let random = random_id()?;
        let handle = SubagentHandle {
            subagent_id: SubagentId(format!("agent-{ordinal}-{random}")),
            session_id: SessionId(format!("child-{random}")),
        };
        let resolved_max_turns = request
            .max_turns
            .unwrap_or(self.inner.limits.max_turns)
            .min(self.inner.limits.max_turns);
        let workspace_root = request.workspace_root.clone();
        Ok(SubagentLaunch {
            handle,
            parent_session_id,
            depth,
            request,
            tools,
            max_turns: resolved_max_turns,
            workspace_root,
            cancellation,
        })
    }

    pub(super) async fn start_owned(
        &self,
        parent_session_id: SessionId,
        request: SubagentRequest,
        observer: Arc<dyn SubagentObserver>,
        cancellation: CancellationToken,
        startup: &mut super::startup::ChildStartup,
    ) -> Result<SubagentHandle, OrchestrationError> {
        let launch = self.prepare_launch(
            parent_session_id.clone(),
            request.clone(),
            cancellation.clone(),
        )?;
        let handle = launch.handle.clone();
        let depth = launch.depth;
        let session = self.inner.factory.create(launch.clone()).await?;
        startup.session = Some(Arc::clone(&session));
        if cancellation.is_cancelled() {
            return Err(OrchestrationError::Session(
                "child startup cancelled".to_owned(),
            ));
        }
        if session.session_id() != &handle.session_id {
            return Err(OrchestrationError::Session(
                "child factory returned a different session id".to_owned(),
            ));
        }
        let mut recovery_record = super::startup::recovery_record(&launch, session.as_ref());
        let metadata = Arc::clone(&startup.metadata);
        startup.recovery = Some(recovery_record.clone());
        metadata.save(recovery_record.clone()).await?;
        startup.publication = super::startup::SpawnPublication::Uncertain;
        observer.spawned(&handle, &request.task).await?;
        startup.publication = super::startup::SpawnPublication::Acknowledged;
        recovery_record.phase = SubagentRecoveryPhase::Active;
        metadata.save(recovery_record).await?;
        let permit = startup.permit()?;
        let retained = startup.retained()?;
        let (result_tx, result_rx) = watch::channel(None);
        self.inner
            .session_depths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(handle.session_id.clone(), depth);
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                handle.subagent_id.clone(),
                SessionRecord {
                    _retained: retained,
                    handle: handle.clone(),
                    task: request.task.clone(),
                    agent: request.agent.clone(),
                    model: request.model.clone(),
                    session: Arc::clone(&session),
                    state: SessionState::Active,
                    result: Some(result_rx),
                    isolation: request.isolation,
                    parent_session_id: parent_session_id.clone(),
                    latest_durable_artifact_id: None,
                    closing_artifact: None,
                    close_completed: false,
                    close_gate: Arc::new(tokio::sync::Mutex::new(())),
                },
            );
        crate::engine::control_observation::changed();
        self.spawn_turn(
            handle.clone(),
            parent_session_id,
            session,
            request.task,
            observer,
            cancellation,
            result_tx,
            permit,
        );
        startup.session = None;
        startup.recovery = None;
        Ok(handle)
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn spawn_turn(
        &self,
        handle: SubagentHandle,
        parent_session_id: SessionId,
        session: Arc<dyn SubagentSession>,
        prompt: String,
        observer: Arc<dyn SubagentObserver>,
        cancellation: CancellationToken,
        result_tx: watch::Sender<Option<Result<SubagentResult, String>>>,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let started = Instant::now();
            let progress: Arc<dyn SubagentProgressObserver> = Arc::new(ObserverProgress {
                observer: Arc::clone(&observer),
                handle: handle.clone(),
            });
            let turn = tokio::select! {
                () = cancellation.cancelled() => {
                    let _ = bounded_cancel(&session, inner.limits).await;
                    Err(OrchestrationError::Session("cancelled".to_owned()))
                },
                result = tokio::time::timeout(
                    inner.limits.max_duration,
                    session.run_turn(prompt, cancellation.clone(), progress),
                ) => if let Ok(result) = result {
                    result
                } else {
                        let _ = bounded_cancel(&session, inner.limits).await;
                        Err(OrchestrationError::Session("timed out".to_owned()))
                },
            };
            if turn.is_err() {
                let _ = bounded_cancel(&session, inner.limits).await;
            }
            let mut result = match turn {
                Ok(mut turn) => {
                    bound_turn_result(&mut turn);
                    SubagentResult {
                        subagent_id: handle.subagent_id.clone(),
                        session_id: handle.session_id.clone(),
                        status: turn.status,
                        final_text: turn.final_text,
                        touched_files: turn.touched_files,
                        diff_artifact: turn.diff_artifact,
                        usage: turn.usage,
                        cost: turn.cost,
                        turns: turn.turns,
                        duration_millis: u64::try_from(started.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                    }
                }
                Err(error) => SubagentResult {
                    subagent_id: handle.subagent_id.clone(),
                    session_id: handle.session_id.clone(),
                    status: if cancellation.is_cancelled() {
                        SubagentStatus::Cancelled
                    } else if started.elapsed() >= inner.limits.max_duration {
                        SubagentStatus::TimedOut
                    } else {
                        SubagentStatus::Failed
                    },
                    final_text: error.to_string(),
                    touched_files: Vec::new(),
                    diff_artifact: None,
                    usage: zero_usage(),
                    cost: Cost::Unavailable {
                        reason: error.to_string(),
                    },
                    turns: 0,
                    duration_millis: u64::try_from(started.elapsed().as_millis())
                        .unwrap_or(u64::MAX),
                },
            };
            if let Some(artifact) = result.diff_artifact.as_ref()
                && let Err(error) = rw_tools::validate_diff_artifact(artifact)
            {
                result.status = SubagentStatus::Failed;
                result.final_text = format!("isolated child returned an invalid diff: {error}");
                result.diff_artifact = None;
            }
            let durable_result = match observer.finished(&result).await {
                Ok(()) => inner
                    .diff_artifact_authority
                    .verify_result(&parent_session_id, &result)
                    .await
                    .map(|()| result)
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            {
                let mut sessions = inner
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(record) = sessions.get_mut(&handle.subagent_id) {
                    if let Ok(durable) = &durable_result {
                        record.latest_durable_artifact_id = durable
                            .diff_artifact
                            .as_ref()
                            .map(|artifact| artifact.id.clone());
                    }
                    record.state = SessionState::Inactive;
                }
            }
            let _ = result_tx.send(Some(durable_result));
            drop(permit);
        });
    }

    /// Waits for the currently running turn associated with a handle.
    ///
    /// # Errors
    ///
    /// Returns when the handle has no pending result or its child failed.
    pub async fn wait(
        &self,
        handle: &SubagentHandle,
    ) -> Result<SubagentResult, OrchestrationError> {
        let mut receiver = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&handle.subagent_id)
            .and_then(|record| record.result.clone())
            .ok_or_else(|| OrchestrationError::NoPendingResult(handle.subagent_id.0.clone()))?;
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result.map_err(OrchestrationError::Session);
            }
            receiver.changed().await.map_err(|_| {
                OrchestrationError::Session("child result channel closed".to_owned())
            })?;
        }
    }

    /// Convenience start-and-wait operation used by the public tool.
    ///
    /// # Errors
    ///
    /// Returns any start, child-session, or durable-observer failure.
    pub async fn spawn(
        &self,
        parent_session_id: SessionId,
        request: SubagentRequest,
        observer: Arc<dyn SubagentObserver>,
        cancellation: CancellationToken,
    ) -> Result<SubagentResult, OrchestrationError> {
        let handle = self
            .start(parent_session_id, request, observer, cancellation)
            .await?;
        self.wait(&handle).await
    }

    /// Sends a follow-up to a completed child while retaining its context/log.
    ///
    /// # Errors
    ///
    /// Returns for unknown/running children, invalid prompts, exhausted concurrency, or failures.
    pub async fn follow_up(
        &self,
        caller_parent_session_id: &SessionId,
        subagent_id: &SubagentId,
        prompt: String,
        observer: Arc<dyn SubagentObserver>,
        cancellation: CancellationToken,
    ) -> Result<SubagentHandle, OrchestrationError> {
        if prompt.trim().is_empty() {
            return Err(OrchestrationError::InvalidRequest(
                "follow-up prompt must not be empty".to_owned(),
            ));
        }
        let (handle, parent_session_id, session, permit) = {
            let mut sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let record = sessions
                .get_mut(subagent_id)
                .ok_or_else(|| OrchestrationError::UnknownSubagent(subagent_id.0.clone()))?;
            ensure_child_owner(caller_parent_session_id, subagent_id, record)?;
            if record.state != SessionState::Inactive {
                return Err(OrchestrationError::AlreadyRunning(subagent_id.0.clone()));
            }
            let permit = Arc::clone(&self.inner.permits)
                .try_acquire_owned()
                .map_err(|_| OrchestrationError::ConcurrencyExceeded {
                    maximum: self.inner.limits.max_concurrency,
                })?;
            record.state = SessionState::Active;
            (
                record.handle.clone(),
                record.parent_session_id.clone(),
                Arc::clone(&record.session),
                permit,
            )
        };
        let (result_tx, result_rx) = watch::channel(None);
        if let Some(record) = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(subagent_id)
        {
            record.result = Some(result_rx);
        }
        if let Err(error) = observer.spawned(&handle, &prompt).await {
            let _ = bounded_cancel(&session, self.inner.limits).await;
            if let Some(record) = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(subagent_id)
            {
                record.state = SessionState::Inactive;
            }
            return Err(error);
        }
        self.spawn_turn(
            handle.clone(),
            parent_session_id,
            session,
            prompt,
            observer,
            cancellation,
            result_tx,
            permit,
        );
        Ok(handle)
    }

    /// Cooperatively cancels one active child.
    ///
    /// # Errors
    ///
    /// Returns when the child is unknown or cancellation fails.
    pub async fn cancel(
        &self,
        caller_parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<(), OrchestrationError> {
        let session = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(subagent_id)
            .ok_or_else(|| OrchestrationError::UnknownSubagent(subagent_id.0.clone()))
            .and_then(|record| {
                ensure_child_owner(caller_parent_session_id, subagent_id, record)?;
                Ok(Arc::clone(&record.session))
            })?;
        bounded_cancel(&session, self.inner.limits).await
    }

    /// Permanently closes a completed child and removes its private recovery metadata.
    ///
    /// # Errors
    ///
    /// Returns for unknown/active children, unsafe worktree finalization, or metadata failure.
    #[allow(clippy::too_many_lines)]
    pub async fn close(
        &self,
        caller_parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<(), OrchestrationError> {
        let close_gate = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(subagent_id)
            .ok_or_else(|| OrchestrationError::UnknownSubagent(subagent_id.0.clone()))
            .and_then(|record| {
                ensure_child_owner(caller_parent_session_id, subagent_id, record)?;
                Ok(Arc::clone(&record.close_gate))
            })?;
        let _close_guard = close_gate.lock().await;
        let (parent_session_id, session, artifact_id, already_finalized) = {
            let mut sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let record = sessions
                .get_mut(subagent_id)
                .ok_or_else(|| OrchestrationError::UnknownSubagent(subagent_id.0.clone()))?;
            ensure_child_owner(caller_parent_session_id, subagent_id, record)?;
            match record.state {
                SessionState::Inactive => record.state = SessionState::Closing,
                SessionState::Closing if record.close_completed => {}
                SessionState::Active | SessionState::Closing => {
                    return Err(OrchestrationError::AlreadyRunning(subagent_id.0.clone()));
                }
            }
            (
                record.parent_session_id.clone(),
                Arc::clone(&record.session),
                record.latest_durable_artifact_id.clone(),
                record.close_completed,
            )
        };
        let durable_artifact = match artifact_id.as_deref() {
            Some(id) => self
                .inner
                .diff_artifact_authority
                .resolve(&parent_session_id, id)
                .await
                .map_err(|error| OrchestrationError::Session(error.to_string()))
                .and_then(|artifact| {
                    artifact.ok_or_else(|| {
                        OrchestrationError::Session(
                            "durable child artifact authority is unavailable".into(),
                        )
                    })
                })
                .map(Some),
            None => Ok(None),
        };
        let durable_artifact = match durable_artifact {
            Ok(artifact) => artifact.map(Arc::new),
            Err(error) => {
                if !already_finalized
                    && let Some(record) = self
                        .inner
                        .sessions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get_mut(subagent_id)
                {
                    record.state = SessionState::Inactive;
                }
                return Err(error);
            }
        };
        if !already_finalized {
            if let Some(record) = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(subagent_id)
            {
                record.closing_artifact.clone_from(&durable_artifact);
            }
            // Failure retains both the child and its source admission.
            tokio::time::timeout(
                control_timeout(self.inner.limits),
                session.close(
                    durable_artifact
                        .as_ref()
                        .map(|artifact| artifact.artifact()),
                ),
            )
            .await
            .map_err(|_| OrchestrationError::EffectsUnsettled("child close timed out".to_owned()))
            .and_then(std::convert::identity)?;
            if let Some(record) = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(subagent_id)
            {
                record.close_completed = true;
                record.closing_artifact = None;
            }
        }
        let metadata = self
            .inner
            .metadata
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        metadata.remove(&parent_session_id, subagent_id).await?;
        let removed = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(subagent_id);
        crate::engine::control_observation::changed();
        if let Some(record) = removed {
            self.inner
                .session_depths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&record.handle.session_id);
        }
        Ok(())
    }

    /// Lists retained children owned directly by one parent session.
    #[must_use]
    pub fn list_for_parent(&self, parent_session_id: &SessionId) -> Vec<SubagentDescriptor> {
        let mut descriptors = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|record| record.parent_session_id == *parent_session_id)
            .map(session_record_descriptor)
            .collect::<Vec<_>>();
        descriptors.sort_by(|left, right| left.subagent_id.0.cmp(&right.subagent_id.0));
        descriptors
    }

    /// Resolves one retained child only when it belongs directly to the caller parent.
    ///
    /// # Errors
    ///
    /// Returns the same opaque unknown-child error for missing and cross-parent ids.
    pub fn descriptor_for_parent(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<SubagentDescriptor, OrchestrationError> {
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = sessions
            .get(subagent_id)
            .ok_or_else(|| OrchestrationError::UnknownSubagent(subagent_id.0.clone()))?;
        ensure_child_owner(parent_session_id, subagent_id, record)?;
        Ok(session_record_descriptor(record))
    }

    /// Restores one child solely from validated host-private metadata.
    ///
    /// # Errors
    ///
    /// Returns when depth, lease identity, or child-log recovery fails.
    #[allow(clippy::too_many_lines)]
    pub async fn recover_record(
        &self,
        record: SubagentRecoveryRecord,
    ) -> Result<(), OrchestrationError> {
        if record.depth == 0 || record.depth > self.inner.limits.max_depth {
            return Err(OrchestrationError::DepthExceeded {
                requested: record.depth,
                maximum: self.inner.limits.max_depth,
            });
        }
        self.ensure_recovery_identity_available(&record.handle)?;
        let retained = Arc::clone(&self.inner.retained)
            .try_acquire_owned()
            .map_err(|_| OrchestrationError::RetainedCapacityExceeded {
                maximum: super::MAX_RETAINED_SUBAGENTS,
            })?;
        let unique_tool_names = record
            .tool_names
            .iter()
            .collect::<std::collections::HashSet<_>>();
        if unique_tool_names.len() != record.tool_names.len() {
            return Err(OrchestrationError::InvalidRequest(
                "recovery tool allowlist contains duplicates".to_owned(),
            ));
        }
        let mut registered_names = Vec::new();
        let mut mcp_grants = Vec::new();
        for name in &record.tool_names {
            if name.starts_with("mcp:") {
                validate_mcp_virtual_tool(name)
                    .map_err(|error| OrchestrationError::InvalidRequest(error.to_string()))?;
                mcp_grants.push(name.clone());
            } else {
                registered_names.push(name.as_str());
            }
        }
        let mcp_policy = McpToolPolicy::restricted(mcp_grants)
            .map_err(|error| OrchestrationError::InvalidRequest(error.to_string()))?;
        let allowed_tools = Arc::new(
            self.tool_registry()
                .subset(registered_names)
                .map_err(|error| OrchestrationError::InvalidRequest(error.to_string()))?
                .with_mcp_tool_policy(mcp_policy),
        );
        let current_capabilities = CapabilityManifest::new(
            allowed_tools
                .descriptors()
                .into_iter()
                .flat_map(|descriptor| descriptor.capabilities.capabilities().to_vec()),
        );
        if current_capabilities != record.capabilities {
            return Err(OrchestrationError::InvalidRequest(
                "recovery capabilities differ from the current tool descriptors".to_owned(),
            ));
        }
        let session = self
            .inner
            .factory
            .rebind(
                &record.handle.session_id,
                Some(&record.workspace_root),
                record.worktree.as_ref(),
                Some(allowed_tools),
                &record.policy,
            )
            .await?
            .ok_or_else(|| {
                OrchestrationError::UnknownSubagent(record.handle.subagent_id.0.clone())
            })?;
        let latest_durable_artifact_id = self
            .inner
            .diff_artifact_authority
            .latest(&record.parent_session_id, &record.handle.subagent_id)
            .await?;
        self.inner
            .session_depths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(record.handle.session_id.clone(), record.depth);
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                record.handle.subagent_id.clone(),
                SessionRecord {
                    _retained: retained,
                    handle: record.handle,
                    task: record.task,
                    agent: record.agent,
                    model: record.policy.model_alias.clone(),
                    session,
                    state: SessionState::Inactive,
                    result: None,
                    isolation: record.isolation,
                    parent_session_id: record.parent_session_id,
                    latest_durable_artifact_id,
                    closing_artifact: None,
                    close_completed: false,
                    close_gate: Arc::new(tokio::sync::Mutex::new(())),
                },
            );
        crate::engine::control_observation::changed();
        Ok(())
    }

    fn ensure_recovery_identity_available(
        &self,
        handle: &SubagentHandle,
    ) -> Result<(), OrchestrationError> {
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if sessions.contains_key(&handle.subagent_id)
            || sessions
                .values()
                .any(|record| record.handle.session_id == handle.session_id)
        {
            return Err(OrchestrationError::InvalidRequest(
                "duplicate recovered child identity".to_owned(),
            ));
        }
        Ok(())
    }

    /// Returns host-private recovery metadata; it must never enter model or parent logs.
    ///
    /// # Errors
    ///
    /// Returns when the child id is unknown.
    pub fn worktree_recovery_record(
        &self,
        subagent_id: &SubagentId,
    ) -> Result<Option<WorktreeLeaseRecord>, OrchestrationError> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(subagent_id)
            .map(|record| record.session.worktree_record())
            .ok_or_else(|| OrchestrationError::UnknownSubagent(subagent_id.0.clone()))
    }
}
