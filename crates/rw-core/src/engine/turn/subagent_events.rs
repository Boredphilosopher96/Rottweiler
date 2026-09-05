use crate::engine::pending_event::PendingEvent;
use crate::engine::turn::provider_messages::persist_event;
use crate::engine::turn::signals::TurnSignal;
use async_trait::async_trait;
use rw_tools::SubagentEventSink;
use rw_tools::SubagentLifecycleEvent;
use rw_tools::SubagentProgressEvent;
use rw_tools::ToolError;
use rw_types::SessionId;
use rw_types::SubagentId;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;
use tokio::sync::mpsc;

/// Serializes durable child lifecycle records by provider tool-call index.
/// Child progress bypasses this gate because it is display-only and absent
/// from the parent log.
pub(in crate::engine) struct OrderedSubagentCoordinator {
    pub(super) positions: BTreeMap<usize, usize>,
    pub(super) multi_producer_calls: BTreeSet<usize>,
    pub(super) next_spawn: AtomicUsize,
    pub(super) allowed_finish: AtomicUsize,
    pub(super) spawned: Notify,
    pub(super) finished: Notify,
    pub(super) signals: mpsc::UnboundedSender<TurnSignal>,
}

impl OrderedSubagentCoordinator {
    #[cfg(test)]
    pub(in crate::engine) fn new(
        indices: impl IntoIterator<Item = usize>,
        signals: mpsc::UnboundedSender<TurnSignal>,
    ) -> Self {
        Self::new_with_multi(indices.into_iter().map(|index| (index, false)), signals)
    }

    pub(in crate::engine) fn new_with_multi(
        calls: impl IntoIterator<Item = (usize, bool)>,
        signals: mpsc::UnboundedSender<TurnSignal>,
    ) -> Self {
        let calls = calls.into_iter().collect::<Vec<_>>();
        Self {
            positions: calls
                .iter()
                .map(|(index, _)| *index)
                .enumerate()
                .map(|(position, index)| (index, position))
                .collect(),
            multi_producer_calls: calls
                .into_iter()
                .filter_map(|(index, multi)| multi.then_some(index))
                .collect(),
            next_spawn: AtomicUsize::new(0),
            allowed_finish: AtomicUsize::new(0),
            spawned: Notify::new(),
            finished: Notify::new(),
            signals,
        }
    }

    pub(super) async fn wait_for(&self, counter: &AtomicUsize, notify: &Notify, position: usize) {
        loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if counter.load(Ordering::Acquire) == position {
                return;
            }
            notified.await;
        }
    }

    pub(super) fn position(&self, index: usize) -> Result<usize, ToolError> {
        self.positions.get(&index).copied().ok_or_else(|| {
            ToolError::Output("subagent lifecycle came from an unregistered tool call".to_owned())
        })
    }

    pub(in crate::engine) fn advance_after_tool(&self, index: usize) {
        let Some(position) = self.positions.get(&index).copied() else {
            return;
        };
        if self.next_spawn.load(Ordering::Acquire) == position {
            self.next_spawn
                .store(position.saturating_add(1), Ordering::Release);
            self.spawned.notify_waiters();
        }
        if self.allowed_finish.load(Ordering::Acquire) == position {
            self.allowed_finish
                .store(position.saturating_add(1), Ordering::Release);
            self.finished.notify_waiters();
        }
    }
}

pub(in crate::engine) struct ActorSubagentEventSink {
    pub(in crate::engine) index: usize,
    pub(in crate::engine) coordinator: Arc<OrderedSubagentCoordinator>,
    pub(in crate::engine) state: Mutex<ActorSubagentLifecycleState>,
}

#[derive(Default)]
pub(in crate::engine) struct ActorSubagentLifecycleState {
    pub(super) single_spawned: bool,
    pub(super) active: HashMap<SubagentId, SessionId>,
}

#[async_trait]
impl SubagentEventSink for ActorSubagentEventSink {
    async fn lifecycle(&self, event: SubagentLifecycleEvent) -> Result<(), ToolError> {
        let position = self.coordinator.position(self.index)?;
        let multiple = self.coordinator.multi_producer_calls.contains(&self.index);
        let (kind, spawned) = match event {
            SubagentLifecycleEvent::Spawned {
                subagent_id,
                child_session_id,
                task,
            } => {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if (!multiple && state.single_spawned) || state.active.contains_key(&subagent_id) {
                    return Err(ToolError::Output(
                        "subagent lifecycle emitted a duplicate active spawn".to_owned(),
                    ));
                }
                state.single_spawned = true;
                state
                    .active
                    .insert(subagent_id.clone(), child_session_id.clone());
                (
                    PendingEvent::SubagentSpawned {
                        subagent_id,
                        child_session_id,
                        task,
                    },
                    true,
                )
            }
            SubagentLifecycleEvent::Finished {
                subagent_id,
                result,
            } => {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let session_id = state.active.get(&subagent_id).ok_or_else(|| {
                    ToolError::Output(
                        "subagent lifecycle emitted Finished without an active spawn".to_owned(),
                    )
                })?;
                if result.subagent_id != subagent_id || &result.session_id != session_id {
                    return Err(ToolError::Output(
                        "subagent lifecycle Finished identity does not match Spawned".to_owned(),
                    ));
                }
                state.active.remove(&subagent_id);
                (
                    PendingEvent::SubagentFinished {
                        subagent_id,
                        result: *result,
                    },
                    false,
                )
            }
        };
        if spawned {
            self.coordinator
                .wait_for(
                    &self.coordinator.next_spawn,
                    &self.coordinator.spawned,
                    position,
                )
                .await;
        } else {
            self.coordinator
                .wait_for(
                    &self.coordinator.allowed_finish,
                    &self.coordinator.finished,
                    position,
                )
                .await;
        }
        persist_event(&self.coordinator.signals, kind)
            .await
            .map_err(|error| ToolError::Output(error.to_string()))?;
        if spawned && !multiple {
            self.coordinator
                .next_spawn
                .store(position.saturating_add(1), Ordering::Release);
            self.coordinator.spawned.notify_waiters();
        }
        Ok(())
    }

    async fn progress(&self, event: SubagentProgressEvent) -> Result<(), ToolError> {
        self.coordinator
            .signals
            .send(TurnSignal::SubagentProgress(event))
            .map_err(|_| ToolError::Cancelled)
    }
}
