//! Scalar control discovery. No question, argument, diff or plan bodies cross this owner.
use super::ActorState;
use rw_types::{EngineEvent, SequenceId, family_controls::ChildControlSummary};
use std::sync::{Mutex, OnceLock};
use tokio::sync::watch;

fn changes() -> &'static watch::Sender<u64> {
    static CHANGES: OnceLock<watch::Sender<u64>> = OnceLock::new();
    CHANGES.get_or_init(|| watch::channel(0).0)
}
/// Family readers capture this fence before reading rows. A concurrent update
/// therefore remains visible to their next conditional read.
pub(crate) fn revision() -> SequenceId {
    SequenceId(*changes().borrow())
}
pub(crate) fn changed() {
    changes().send_modify(|revision| *revision = revision.wrapping_add(1));
}
pub(crate) async fn wait(after: Option<SequenceId>) {
    let mut receiver = changes().subscribe();
    if after.is_some_and(|after| after.0 == *receiver.borrow_and_update()) {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(
                rw_types::family_controls::FAMILY_CONTROL_WAIT_MILLIS as u64,
            ),
            receiver.changed(),
        )
        .await;
    }
}

#[derive(Default)]
pub(in crate::engine) struct ControlObservation {
    current: Mutex<(Option<SequenceId>, ChildControlSummary)>,
}
impl ControlObservation {
    pub(in crate::engine) fn snapshot(&self) -> ChildControlSummary {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1
            .clone()
    }
    pub(super) fn publish(&self, source: Option<SequenceId>, mut next: ChildControlSummary) {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        next.revision = current.1.revision;
        // Streaming journal advancement does not wake a control observer.
        if source != current.0
            || next.questions != current.1.questions
            || next.approvals != current.1.approvals
            || next.pending_plan != current.1.pending_plan
            || next.available != current.1.available
        {
            changed();
            next.revision = revision();
        }
        *current = (source, next);
    }
}
pub(in crate::engine) fn publish(state: &ActorState) {
    let available = !state.closing && !state.poisoned;
    state.control.observation.publish(
        state.live.controls_source,
        ChildControlSummary {
            revision: SequenceId(0),
            through: state.sequence.map(SequenceId),
            questions: if available {
                u32::try_from(state.pending_questions.len() + state.pending_model_switches.len())
                    .unwrap_or(u32::MAX)
            } else {
                0
            },
            approvals: if available {
                u32::try_from(state.pending_approvals.len()).unwrap_or(u32::MAX)
            } else {
                0
            },
            pending_plan: available && state.pending_plan.is_some(),
            available,
        },
    );
}
pub(in crate::engine) fn is_control_event(event: &EngineEvent) -> bool {
    matches!(
        event,
        EngineEvent::QuestionAsked { .. }
            | EngineEvent::QuestionAnswered { .. }
            | EngineEvent::ToolApprovalNeeded { .. }
            | EngineEvent::ToolApprovalResolved { .. }
            | EngineEvent::ToolCallFinished { .. }
            | EngineEvent::PlanSubmitted { .. }
            | EngineEvent::PlanReviewed { .. }
            | EngineEvent::ConversationRewound { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::{ControlObservation, revision, wait};
    use rw_types::{SequenceId, family_controls::ChildControlSummary};
    #[tokio::test]
    async fn scalar_revision_changes_for_replacement_and_removal_without_progress() {
        let observation = ControlObservation::default();
        let pending = ChildControlSummary {
            questions: 1,
            available: true,
            ..Default::default()
        };
        observation.publish(Some(SequenceId(1)), pending.clone());
        let first = observation.snapshot();
        let global = revision();
        observation.publish(Some(SequenceId(2)), pending);
        assert_ne!(observation.snapshot().revision, first.revision);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), wait(Some(global)))
                .await
                .is_ok()
        );
        let replacement = observation.snapshot().revision;
        observation.publish(Some(SequenceId(3)), ChildControlSummary::default());
        assert_ne!(observation.snapshot().revision, replacement);
        assert_eq!(observation.snapshot().questions, 0);
    }
}
