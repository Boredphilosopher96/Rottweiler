//! Hosted child display admission shares a slot across every cloned parent handle.
use super::state::ActorCommand;
use crate::engine::{AgentLoopError, turn::child_progress::ChildProgressSlot};
use rw_tools::SubagentProgressEvent;
use rw_types::{SessionId, SubagentId};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

pub(super) struct HostedChildProgress {
    active: Mutex<HashMap<SubagentId, (SessionId, Arc<ChildProgressSlot>)>>,
    pub(super) budget: rw_tools::ChildProgressBudget,
}
impl HostedChildProgress {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            active: Mutex::new(HashMap::new()),
            budget: rw_tools::ChildProgressBudget::default(),
        })
    }
    pub(super) fn register(
        &self,
        child: &SubagentId,
        session: &SessionId,
    ) -> Result<(), AgentLoopError> {
        SessionId::validate(&session.0).map_err(invalid)?;
        if child.0.is_empty() || child.0.len() > 256 {
            return Err(invalid("invalid child progress identity"));
        }
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.len() >= crate::orchestration::MAX_RETAINED_SUBAGENTS
            || active.contains_key(child)
        {
            return Err(invalid(
                "child progress spawn admission exhausted or duplicated",
            ));
        }
        active.insert(
            child.clone(),
            (session.clone(), ChildProgressSlot::new(self.budget.clone())),
        );
        Ok(())
    }
    pub(super) fn publish(
        &self,
        event: SubagentProgressEvent,
        commands: &mpsc::Sender<ActorCommand>,
    ) -> Result<(), AgentLoopError> {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = active
            .get(&event.subagent_id)
            .filter(|(session, _)| *session == event.child_session_id)
            .map(|(_, slot)| slot)
            .ok_or_else(|| invalid("child progress has no matching active spawn"))?;
        slot.publish(event, |slot| {
            commands
                .try_send(ActorCommand::PublishSubagentProgress(slot))
                .is_ok()
        })
        .map_err(invalid)
    }
    pub(super) fn finish(&self, child: &SubagentId) {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(child);
    }
}
fn invalid(error: impl std::fmt::Display) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(error.to_string())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    fn event(owner: &HostedChildProgress, sequence: u64) -> SubagentProgressEvent {
        SubagentProgressEvent {
            subagent_id: SubagentId("child".into()),
            child_session_id: SessionId("session".into()),
            child_sequence: Some(sequence),
            event: owner
                .budget
                .encode_value(Some(sequence), &serde_json::json!({"text": "delta"}))
                .expect("encode")
                .expect("preview"),
        }
    }
    #[test]
    fn saturated_actor_queue_never_blocks_child_effects_and_marks_the_next_source() {
        let owner = HostedChildProgress::new();
        let initial = event(&owner, 0);
        owner
            .register(&initial.subagent_id, &initial.child_session_id)
            .expect("register");
        let (send, mut receive) = mpsc::channel(1);
        owner.publish(initial, &send).expect("first");
        let mut other = event(&owner, 1);
        other.subagent_id = SubagentId("other".into());
        owner
            .register(&other.subagent_id, &other.child_session_id)
            .expect("other");
        owner
            .publish(other, &send)
            .expect("saturation does not block");
        let ActorCommand::PublishSubagentProgress(slot) = receive.try_recv().expect("first signal")
        else {
            panic!("progress")
        };
        drop(slot.take());
        let mut other = event(&owner, 2);
        other.subagent_id = SubagentId("other".into());
        owner.publish(other, &send).expect("next");
        let ActorCommand::PublishSubagentProgress(slot) =
            receive.try_recv().expect("second signal")
        else {
            panic!("progress")
        };
        assert!(
            slot.take()
                .expect("source marker")
                .event
                .event
                .value()
                .is_null()
        );
        owner.finish(&SubagentId("other".into()));
        let mut stale = event(&owner, 3);
        stale.subagent_id = SubagentId("other".into());
        assert!(owner.publish(stale, &send).is_err());
        assert_eq!(
            owner.budget.available_bytes(),
            rw_tools::CHILD_PROGRESS_MEMORY_BYTES
        );
    }
}
