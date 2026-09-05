//! Startup owns creation, lifecycle receipts and unclaimed-child cleanup.
use super::*;
use futures_util::FutureExt as _;
use std::panic::AssertUnwindSafe;
use tokio::sync::{OwnedSemaphorePermit, oneshot};

pub(super) struct Startups {
    admission: Arc<Semaphore>,
    active: watch::Sender<usize>,
    unproven: Mutex<Vec<UnprovenStartup>>,
}
struct UnprovenStartup {
    _admission: OwnedSemaphorePermit,
    _child: ChildStartup,
}
impl Startups {
    pub(super) fn new(maximum: usize) -> Self {
        Self {
            admission: Arc::new(Semaphore::new(maximum)),
            active: watch::channel(0).0,
            unproven: Mutex::new(Vec::new()),
        }
    }
    pub(super) async fn settle(&self) -> Result<(), OrchestrationError> {
        let mut active = self.active.subscribe();
        loop {
            let count = *active.borrow_and_update();
            if !self
                .unproven
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
            {
                return Err(OrchestrationError::EffectsUnsettled(
                    "child startup effects remain unproven".to_owned(),
                ));
            }
            if count == 0 {
                return Ok(());
            }
            if active.changed().await.is_err() {
                return Err(OrchestrationError::Session(
                    "child startup owner stopped".to_owned(),
                ));
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) enum SpawnPublication {
    #[default]
    Unattempted,
    Uncertain,
    Acknowledged,
}

pub(super) struct ChildStartup {
    pub(super) session: Option<Arc<dyn SubagentSession>>,
    pub(super) recovery: Option<SubagentRecoveryRecord>,
    pub(super) publication: SpawnPublication,
    pub(super) metadata: Arc<dyn SubagentMetadataStore>,
    observer: Arc<dyn SubagentObserver>,
    permit: Option<OwnedSemaphorePermit>,
}
impl ChildStartup {
    pub(super) fn permit(&mut self) -> Result<OwnedSemaphorePermit, OrchestrationError> {
        self.permit.take().ok_or_else(|| {
            OrchestrationError::Session("child startup permit already transferred".to_owned())
        })
    }
    async fn cleanup(
        &mut self,
        limits: SubagentLimits,
        reason: &str,
    ) -> Result<(), OrchestrationError> {
        let Some(session) = &self.session else {
            return Ok(());
        };
        bounded_close(session, None, limits).await?;
        if let Some(record) = &mut self.recovery {
            match self.publication {
                SpawnPublication::Unattempted => {
                    self.metadata
                        .remove(&record.parent_session_id, &record.handle.subagent_id)
                        .await?;
                }
                SpawnPublication::Uncertain | SpawnPublication::Acknowledged => {
                    record.phase = SubagentRecoveryPhase::Closed;
                    self.metadata.save(record.clone()).await?;
                    if matches!(self.publication, SpawnPublication::Acknowledged) {
                        self.observer
                            .finished(&SubagentResult {
                                subagent_id: record.handle.subagent_id.clone(),
                                session_id: record.handle.session_id.clone(),
                                status: SubagentStatus::Failed,
                                final_text: reason.to_owned(),
                                touched_files: Vec::new(),
                                diff_artifact: None,
                                usage: zero_usage(),
                                cost: Cost::Unavailable {
                                    reason: "child never started a turn".to_owned(),
                                },
                                turns: 0,
                                duration_millis: 0,
                            })
                            .await?;
                        self.metadata
                            .remove(&record.parent_session_id, &record.handle.subagent_id)
                            .await?;
                    }
                }
            }
        }
        self.session = None;
        self.permit = None;
        Ok(())
    }
}
pub(super) fn recovery_record(
    launch: &SubagentLaunch,
    session: &dyn SubagentSession,
) -> SubagentRecoveryRecord {
    SubagentRecoveryRecord {
        parent_session_id: launch.parent_session_id.clone(),
        handle: launch.handle.clone(),
        task: launch.request.task.clone(),
        agent: launch.request.agent.clone(),
        depth: launch.depth,
        workspace_root: launch.workspace_root.clone(),
        isolation: launch.request.isolation,
        worktree: session.worktree_record(),
        capabilities: CapabilityManifest::new(
            launch
                .tools
                .descriptors()
                .into_iter()
                .flat_map(|descriptor| descriptor.capabilities.capabilities().to_vec()),
        ),
        tool_names: launch
            .tools
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .chain(
                launch
                    .request
                    .tools
                    .iter()
                    .filter(|name| name.starts_with("mcp:"))
                    .cloned(),
            )
            .collect(),
        policy: SubagentRecoveryPolicy {
            model_alias: launch.request.model.clone(),
            system_prompt: launch.request.system_prompt.clone(),
            permission_mode: launch.request.permission_mode,
            max_turns: launch.max_turns,
        },
        phase: SubagentRecoveryPhase::Pending,
    }
}

struct CallerCancellation(Option<CancellationToken>);
impl Drop for CallerCancellation {
    fn drop(&mut self) {
        if let Some(token) = &self.0 {
            token.cancel();
        }
    }
}
struct StartupReply {
    result: Result<SubagentHandle, OrchestrationError>,
    claim: Option<oneshot::Sender<()>>,
}

impl SubagentOrchestrator {
    /// Wait for creation and cleanup obligations, including abandoned startup callers.
    ///
    /// # Errors
    /// Reports cleanup failures while retaining their resource obligations.
    pub async fn settle_startups(&self) -> Result<(), OrchestrationError> {
        self.inner.startups.settle().await
    }

    /// Starts a child while retaining ownership through cancellation and receipt delivery.
    ///
    /// # Errors
    /// Returns validation, admission, factory, observer or cleanup failures.
    pub async fn start(
        &self,
        parent_session_id: SessionId,
        request: SubagentRequest,
        observer: Arc<dyn SubagentObserver>,
        cancellation: CancellationToken,
    ) -> Result<SubagentHandle, OrchestrationError> {
        let admission = Arc::clone(&self.inner.startups.admission)
            .try_acquire_owned()
            .map_err(|_| OrchestrationError::ConcurrencyExceeded {
                maximum: self.inner.limits.max_concurrency,
            })?;
        let permit = Arc::clone(&self.inner.permits)
            .try_acquire_owned()
            .map_err(|_| OrchestrationError::ConcurrencyExceeded {
                maximum: self.inner.limits.max_concurrency,
            })?;
        let metadata = self
            .inner
            .metadata
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let child = ChildStartup {
            session: None,
            recovery: None,
            publication: SpawnPublication::Unattempted,
            metadata,
            observer,
            permit: Some(permit),
        };
        let owner = self.clone();
        self.inner.startups.active.send_modify(|count| *count += 1);
        let mut caller = CallerCancellation(Some(cancellation.clone()));
        let (send, receive) = oneshot::channel();
        tokio::spawn(async move {
            owner
                .own_startup(
                    parent_session_id,
                    request,
                    cancellation,
                    child,
                    admission,
                    send,
                )
                .await;
        });
        let mut response = receive
            .await
            .map_err(|_| OrchestrationError::Session("child startup owner stopped".to_owned()))?;
        if let Some(claim) = response.claim.take() {
            let _ = claim.send(());
        }
        caller.0 = None;
        response.result
    }

    async fn own_startup(
        &self,
        parent: SessionId,
        request: SubagentRequest,
        cancellation: CancellationToken,
        mut child: ChildStartup,
        admission: OwnedSemaphorePermit,
        send: oneshot::Sender<StartupReply>,
    ) {
        let result = AssertUnwindSafe(self.start_owned(
            parent.clone(),
            request,
            Arc::clone(&child.observer),
            cancellation.clone(),
            &mut child,
        ))
        .catch_unwind()
        .await
        .unwrap_or_else(|_| {
            Err(OrchestrationError::EffectsUnsettled(
                "child startup panicked".to_owned(),
            ))
        });
        let settlement = match result {
            Ok(handle) => {
                let (claim, claimed) = oneshot::channel();
                let _ = send.send(StartupReply {
                    result: Ok(handle.clone()),
                    claim: Some(claim),
                });
                if claimed.await.is_ok() {
                    Ok(())
                } else {
                    cancellation.cancel();
                    AssertUnwindSafe(async {
                        let _ = self.wait(&handle).await;
                        self.close(&parent, &handle.subagent_id).await
                    })
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| {
                        Err(OrchestrationError::Session(
                            "unclaimed child cleanup panicked".to_owned(),
                        ))
                    })
                }
            }
            Err(error) => {
                let settled =
                    AssertUnwindSafe(child.cleanup(self.inner.limits, &error.to_string()))
                        .catch_unwind()
                        .await;
                let mut settled = settled.unwrap_or_else(|_| {
                    Err(OrchestrationError::Session(
                        "child startup cleanup panicked".to_owned(),
                    ))
                });
                if let OrchestrationError::EffectsUnsettled(reason) = &error {
                    settled = Err(OrchestrationError::EffectsUnsettled(reason.clone()));
                }
                let response = match &settled {
                    Ok(()) => error,
                    Err(cleanup) => OrchestrationError::EffectsUnsettled(format!(
                        "{error}; child effects remain unproven: {cleanup}"
                    )),
                };
                let _ = send.send(StartupReply {
                    result: Err(response),
                    claim: None,
                });
                settled
            }
        };
        if settlement.is_err() {
            self.inner
                .startups
                .unproven
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(UnprovenStartup {
                    _admission: admission,
                    _child: child,
                });
            self.inner.startups.active.send_modify(|_| {});
        } else {
            drop(child);
            drop(admission);
            self.inner.startups.active.send_modify(|count| *count -= 1);
        }
    }
}
