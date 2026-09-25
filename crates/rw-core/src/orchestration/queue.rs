//! Spawns beyond the concurrency limit wait here, in FIFO order, for a running slot.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use rw_tools::CancellationToken;
use rw_types::{Cost, SessionId, SubagentId, SubagentIsolation, SubagentResult, SubagentStatus};
use tokio::sync::watch;

use super::{
    ChildDelivery, DeliveryWaiter, OrchestrationError, SessionState, SubagentHandle,
    SubagentObserver, SubagentOrchestrator, SubagentRequest, ensure_child_owner, zero_usage,
};

/// Result of admitting a child: its stable identity and whether it waits for a slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentTicket {
    pub handle: SubagentHandle,
    pub queued: bool,
}

/// A child admitted by its parent that has not started because every slot is busy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedSubagent {
    pub subagent_id: SubagentId,
    pub child_session_id: SessionId,
    pub task: String,
    pub agent: String,
    pub model: String,
    pub isolation: SubagentIsolation,
}

type StartOutcome = Option<Result<(), String>>;

struct QueuedChild {
    parent: SessionId,
    child: QueuedSubagent,
    cancellation: CancellationToken,
    delivery: Option<Arc<ChildDelivery>>,
    started: watch::Receiver<StartOutcome>,
}

#[derive(Default)]
pub(super) struct Queue {
    entries: Mutex<HashMap<SubagentId, QueuedChild>>,
    changed: watch::Sender<u64>,
}

impl Queue {
    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<SubagentId, QueuedChild>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn remove(&self, subagent_id: &SubagentId) {
        self.entries().remove(subagent_id);
        self.changed
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        crate::engine::control_observation::changed();
    }
}

/// How one child a caller waited on ended its wait.
#[derive(Debug)]
pub enum ChildWait {
    Finished(Box<SubagentResult>),
    /// Cancelled or failed before a session existed; no result will be delivered.
    NeverStarted(String),
}

impl SubagentOrchestrator {
    /// Admits a child and returns immediately after its startup, or at once with
    /// `queued` when every slot is busy. A queued child starts when a slot frees.
    ///
    /// # Errors
    /// Returns validation, depth, retained-capacity, and immediate startup failures.
    pub async fn submit(
        &self,
        parent_session_id: SessionId,
        request: SubagentRequest,
        observer: Arc<dyn SubagentObserver>,
        cancellation: CancellationToken,
    ) -> Result<SubagentTicket, OrchestrationError> {
        let launch = self.prepare_launch(parent_session_id, request, cancellation.clone())?;
        let retained = self.retain()?;
        if let Some(slots) = self.try_slots() {
            return self
                .start_admitted(launch, observer, cancellation, slots, retained)
                .await
                .map(|handle| SubagentTicket {
                    handle,
                    queued: false,
                })
                .map_err(|failure| failure.error);
        }
        let handle = launch.handle.clone();
        let (started_tx, started) = watch::channel(None);
        self.inner.queue.entries().insert(
            handle.subagent_id.clone(),
            QueuedChild {
                parent: launch.parent_session_id.clone(),
                child: QueuedSubagent {
                    subagent_id: handle.subagent_id.clone(),
                    child_session_id: handle.session_id.clone(),
                    task: launch.request.task.clone(),
                    agent: launch.request.agent.clone(),
                    model: launch.request.model.clone(),
                    isolation: launch.request.isolation,
                },
                cancellation: cancellation.clone(),
                delivery: observer.delivery(),
                started,
            },
        );
        crate::engine::control_observation::changed();
        self.inner.activity_changed();
        let owner = self.clone();
        let queued_id = handle.subagent_id.clone();
        tokio::spawn(async move {
            let outcome = match owner.acquire_slots(&cancellation).await {
                None => Err("cancelled before it started".to_owned()),
                Some(slots) => {
                    let task = launch.request.task.clone();
                    let handle = launch.handle.clone();
                    match owner
                        .start_admitted(
                            launch,
                            Arc::clone(&observer),
                            cancellation,
                            slots,
                            retained,
                        )
                        .await
                    {
                        Ok(_) => Ok(()),
                        Err(failure) => {
                            let reason = format!("failed to start: {}", failure.error);
                            if failure.unpublished {
                                publish_start_failure(observer.as_ref(), &handle, &task, &reason)
                                    .await;
                            }
                            Err(reason)
                        }
                    }
                }
            };
            owner.inner.queue.remove(&queued_id);
            owner.inner.activity_changed();
            let _ = started_tx.send(Some(outcome));
        });
        Ok(SubagentTicket {
            handle,
            queued: true,
        })
    }

    /// Children this parent admitted that still wait for a slot, in id order.
    #[must_use]
    pub fn queued_for_parent(&self, parent_session_id: &SessionId) -> Vec<QueuedSubagent> {
        let mut queued = self
            .inner
            .queue
            .entries()
            .values()
            .filter(|entry| entry.parent == *parent_session_id)
            .map(|entry| entry.child.clone())
            .collect::<Vec<_>>();
        queued.sort_by(|left, right| left.subagent_id.0.cmp(&right.subagent_id.0));
        queued
    }

    /// Waits for one child owned by the parent to start (if queued) and finish.
    ///
    /// # Errors
    /// Returns for unknown children and failed result delivery.
    pub async fn wait_for_parent(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<ChildWait, OrchestrationError> {
        let queued = self.inner.queue.entries().get(subagent_id).map(|entry| {
            (
                entry.parent.clone(),
                entry.child.child_session_id.clone(),
                entry.started.clone(),
            )
        });
        let handle = if let Some((parent, session_id, mut started)) = queued {
            if parent != *parent_session_id {
                return Err(OrchestrationError::UnknownSubagent(subagent_id.0.clone()));
            }
            let outcome = loop {
                if let Some(outcome) = started.borrow_and_update().clone() {
                    break outcome;
                }
                if started.changed().await.is_err() {
                    break Err("queued child owner stopped".to_owned());
                }
            };
            if let Err(reason) = outcome {
                return Ok(ChildWait::NeverStarted(reason));
            }
            SubagentHandle {
                subagent_id: subagent_id.clone(),
                session_id,
            }
        } else {
            let child = self.descriptor_for_parent(parent_session_id, subagent_id)?;
            SubagentHandle {
                subagent_id: child.subagent_id,
                session_id: child.child_session_id,
            }
        };
        self.wait(&handle)
            .await
            .map(|result| ChildWait::Finished(Box::new(result)))
    }

    /// Registers a foreground waiter when the child's invocation is tool-owned.
    ///
    /// # Errors
    /// Returns the opaque unknown-child error for missing and cross-parent ids.
    pub fn foreground_waiter(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<Option<DeliveryWaiter>, OrchestrationError> {
        Ok(self
            .delivery(parent_session_id, subagent_id)?
            .map(|delivery| delivery.waiter()))
    }

    /// Releases every tool call waiting on a running or queued child; its result
    /// is then delivered to the parent like any background completion.
    ///
    /// # Errors
    /// Returns for unknown children and children nobody waits on.
    pub fn move_to_background(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<(), OrchestrationError> {
        let running = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(subagent_id)
            .map(|record| record.state == SessionState::Active);
        if running == Some(false) {
            return Err(OrchestrationError::NotInForeground(subagent_id.0.clone()));
        }
        let detached = self
            .delivery(parent_session_id, subagent_id)?
            .is_some_and(|delivery| delivery.detach());
        if detached {
            crate::engine::control_observation::changed();
            Ok(())
        } else {
            Err(OrchestrationError::NotInForeground(subagent_id.0.clone()))
        }
    }

    fn delivery(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<Option<Arc<ChildDelivery>>, OrchestrationError> {
        if let Some(record) = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(subagent_id)
        {
            ensure_child_owner(parent_session_id, subagent_id, record)?;
            return Ok(record.delivery.clone());
        }
        let entries = self.inner.queue.entries();
        let entry = entries
            .get(subagent_id)
            .filter(|entry| entry.parent == *parent_session_id)
            .ok_or_else(|| OrchestrationError::UnknownSubagent(subagent_id.0.clone()))?;
        Ok(entry.delivery.clone())
    }

    /// Cancels a queued child before it starts. Returns false when it is not queued.
    ///
    /// # Errors
    /// Returns the opaque unknown-child error for another parent's queued child.
    pub(super) fn cancel_queued(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &SubagentId,
    ) -> Result<bool, OrchestrationError> {
        let entries = self.inner.queue.entries();
        let Some(entry) = entries.get(subagent_id) else {
            return Ok(false);
        };
        if entry.parent != *parent_session_id {
            return Err(OrchestrationError::UnknownSubagent(subagent_id.0.clone()));
        }
        entry.cancellation.cancel();
        Ok(true)
    }

    /// Whether a child is still waiting for a slot.
    pub(super) fn is_queued(&self, subagent_id: &SubagentId) -> bool {
        self.inner.queue.entries().contains_key(subagent_id)
    }

    /// Cancels this parent's queued children and waits until their owners exit.
    pub(super) async fn drain_queue(&self, parent_session_id: &SessionId) {
        let mut changed = self.inner.queue.changed.subscribe();
        loop {
            let pending = {
                let entries = self.inner.queue.entries();
                let mut pending = false;
                for entry in entries
                    .values()
                    .filter(|entry| entry.parent == *parent_session_id)
                {
                    entry.cancellation.cancel();
                    pending = true;
                }
                pending
            };
            if !pending {
                return;
            }
            let _ = changed.changed().await;
        }
    }
}

/// Publishes a durable spawn and failed result for a child that never started,
/// so a parent that was told it is queued learns the outcome.
async fn publish_start_failure(
    observer: &dyn SubagentObserver,
    handle: &SubagentHandle,
    task: &str,
    reason: &str,
) {
    if observer.spawned(handle, task).await.is_err() {
        return;
    }
    let _ = observer
        .finished(&SubagentResult {
            subagent_id: handle.subagent_id.clone(),
            session_id: handle.session_id.clone(),
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
        .await;
}
