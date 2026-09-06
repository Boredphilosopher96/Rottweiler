//! Snapshot the currently actionable controls at one actor dispatch boundary.
use crate::engine::{AgentLoopError, session::ActorState, wire_turn_id};
use rw_types::{
    QuestionId, SequenceId, ToolCallId,
    allocation::PrepareAllocation,
    session_controls::{
        self, SessionApproval, SessionControls, SessionControlsSnapshot, SessionQuestion,
    },
};

pub(super) fn snapshot(state: &ActorState) -> Result<SessionControlsSnapshot, AgentLoopError> {
    if state.poisoned || state.closing {
        return Err(AgentLoopError::Closed);
    }
    if state.pending_questions.len() + state.pending_model_switches.len()
        > rw_types::question_admission::MAX_PENDING_QUESTION_REQUESTS
        || state.pending_approvals.len() > rw_types::tool_admission::MAX_PENDING_TOOL_INVOCATIONS
    {
        return Err(limit());
    }
    // The host acquired direct-read admission before enqueueing this command.
    // Count existing allocations before making the one independent reply copy.
    let mut prepared = 64 * 1024;
    for questions in state
        .pending_questions
        .values()
        .map(|value| &value.question)
        .chain(
            state
                .pending_model_switches
                .values()
                .map(|value| &value.question),
        )
    {
        rw_types::question_admission::validate_question(questions).map_err(|_| limit())?;
        charge(&mut prepared, questions.prepared_bytes())?;
    }
    for pending in state.pending_approvals.values() {
        let request = &pending.request;
        for bytes in [
            request.id.prepared_bytes(),
            request.invocation_id.prepared_bytes(),
            request.tool_name.prepared_bytes(),
            request.arguments.prepared_bytes(),
            request.capabilities.prepared_bytes(),
            request.approval_diff.prepared_bytes(),
        ] {
            charge(&mut prepared, bytes)?;
        }
    }
    if let Some(plan) = &state.pending_plan {
        session_controls::validate_plan(plan).map_err(|_| limit())?;
        charge(&mut prepared, plan.prepared_bytes())?;
    }
    let questions = state
        .pending_questions
        .iter()
        .map(|(id, value)| SessionQuestion {
            question_id: QuestionId(id.clone()),
            turn_id: wire_turn_id(value.turn),
            question: value.question.clone(),
        })
        .chain(
            state
                .pending_model_switches
                .iter()
                .map(|(id, value)| SessionQuestion {
                    question_id: QuestionId(id.clone()),
                    turn_id: wire_turn_id(value.turn),
                    question: value.question.clone(),
                }),
        )
        .collect();
    let approvals = state
        .pending_approvals
        .values()
        .map(|pending| {
            let request = &pending.request;
            SessionApproval {
                invocation_id: request.invocation_id.clone(),
                tool_call_id: ToolCallId(request.id.clone()),
                turn_id: wire_turn_id(pending.turn),
                name: request.tool_name.clone(),
                args: request.arguments.clone(),
                capabilities: request.capabilities.clone(),
                rationale: request.rationale(),
                diff: request.approval_diff.clone(),
            }
        })
        .collect();
    let result = SessionControlsSnapshot {
        through: state.sequence.map(SequenceId),
        controls: SessionControls {
            questions,
            approvals,
            pending_plan: state.pending_plan.clone(),
        },
    };
    session_controls::encoded_size(&result, session_controls::MAX_SESSION_CONTROLS_BYTES)
        .map_err(|_| limit())?;
    Ok(result)
}
fn charge(total: &mut usize, bytes: Option<usize>) -> Result<(), AgentLoopError> {
    *total = total
        .checked_add(bytes.ok_or_else(limit)?)
        .filter(|bytes| *bytes <= session_controls::MAX_SESSION_CONTROLS_PREPARED_BYTES)
        .ok_or_else(limit)?;
    Ok(())
}
fn limit() -> AgentLoopError {
    AgentLoopError::InvalidConfiguration("session controls exceed source admission".into())
}

pub(super) async fn resolve_approval(
    state: &mut ActorState,
    config: &std::sync::Arc<crate::engine::session::SessionActorConfig>,
    events: &crate::engine::live_events::LiveEvents,
    id: &rw_types::ToolCallId,
    decision: rw_types::ApprovalDecision,
) -> Result<(), AgentLoopError> {
    let Some(pending) = state.pending_approvals.get(&id.0) else {
        return Ok(());
    };
    let event = crate::engine::pending_event::PendingEvent::ToolApprovalResolved {
        turn: pending.turn,
        tool_call_id: id.clone(),
        invocation_id: pending.request.invocation_id.clone(),
        decision,
    };
    crate::engine::turn::emit(state, events, &config.event_sink, event)
        .await
        .map(|_| ())
}
