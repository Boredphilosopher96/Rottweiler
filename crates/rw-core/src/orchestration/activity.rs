//! Parent-visible child activity: whether any child still owes its parent a
//! result, and the observer every parent-less continuation shares.
use std::sync::Arc;

use rw_tools::SubagentEventSink;
use rw_types::SessionId;

use super::{
    ChildDelivery, OrchestratorInner, SessionState, SubagentObserver, SubagentOrchestrator,
    tools::ToolObserver,
};

impl OrchestratorInner {
    /// A child started, finished, or left the queue.
    pub(super) fn activity_changed(&self) {
        self.activity
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }
}

impl SubagentOrchestrator {
    /// Whether any direct child of this parent is running or waiting for a slot.
    #[must_use]
    pub fn has_outstanding_children(&self, parent_session_id: &SessionId) -> bool {
        let running = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|record| {
                record.parent_session_id == *parent_session_id
                    && record.state == SessionState::Active
            });
        running || !self.queued_for_parent(parent_session_id).is_empty()
    }

    /// Resolves once no direct child of this parent is running or queued.
    ///
    /// A child's `subagent_finished` event is durable before it stops counting
    /// here, so a caller that then reads the parent's event log to its current
    /// tail has observed every result these children produced.
    pub async fn children_settled(&self, parent_session_id: &SessionId) {
        let mut activity = self.inner.activity.subscribe();
        loop {
            activity.borrow_and_update();
            if !self.has_outstanding_children(parent_session_id) {
                return;
            }
            // The sender lives as long as this orchestrator.
            let _ = activity.changed().await;
        }
    }

    /// Observer for a child turn started outside a parent tool call, such as a
    /// user continuing a child. Its result wakes an idle parent exactly like a
    /// background `spawn_agent` child.
    #[must_use]
    pub fn background_observer(
        &self,
        events: Arc<dyn SubagentEventSink>,
    ) -> Arc<dyn SubagentObserver> {
        Arc::new(ToolObserver::new(
            events,
            ChildDelivery::new(true),
            self.inner.limits.wake_on_completion,
        ))
    }
}
