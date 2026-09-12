//! Bounded source proof for a child's inert control discovery.
use crate::{
    AgentLoopError, SessionActorConfig,
    recovery::{RecoveryHead, registry_fingerprint},
};
use rw_types::{SequenceId, SessionId, family_controls::ChildControlSummary};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DormantChildControls {
    pub session_id: SessionId,
    pub through: Option<SequenceId>,
    pub registry_fingerprint: [u8; 32],
    pub questions: u32,
    pub pending_plan: bool,
}
impl DormantChildControls {
    /// # Errors
    /// Rejects missing identity or a question count exceeding the source contract.
    pub fn from_head(session_id: &SessionId, head: &RecoveryHead) -> Result<Self, AgentLoopError> {
        if head
            .session_id
            .as_ref()
            .is_some_and(|stored| stored != session_id)
            || (head.session_id.is_none() && head.next_sequence != 0)
        {
            return Err(invalid("child control source identity"));
        }
        let questions = u32::try_from(head.control.questions.len())
            .map_err(|_| invalid("child question count"))?;
        Ok(Self {
            session_id: session_id.clone(),
            through: head.next_sequence.checked_sub(1).map(SequenceId),
            registry_fingerprint: head.registry_fingerprint,
            questions,
            pending_plan: head.control.pending_plan.is_some(),
        })
    }
    pub(super) fn summary(&self) -> ChildControlSummary {
        ChildControlSummary {
            revision: self.through.unwrap_or(SequenceId(0)),
            through: self.through,
            questions: self.questions,
            approvals: 0,
            pending_plan: self.pending_plan,
            available: true,
        }
    }
    pub(super) fn matches(&self, config: &SessionActorConfig) -> bool {
        config.session_id == self.session_id
            && config.recovered.last_sequence == self.through
            && registry_fingerprint(&config.modes) == self.registry_fingerprint
    }
}
fn invalid(message: &str) -> AgentLoopError {
    AgentLoopError::Persistence(message.into())
}
