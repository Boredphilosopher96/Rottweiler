//! Stale turn signals settle their replies without mutating the damaged projection.
use crate::engine::{AgentLoopError, session::ActorState, turn::TurnSignal};
use rw_types::ApprovalDecision;

pub(in crate::engine) fn reject_signal(signal: TurnSignal, state: &mut ActorState) {
    let error = || AgentLoopError::Persistence("session is awaiting journal recovery".into());
    match signal {
        TurnSignal::AdmitToolResults { respond, .. } => {
            let _ = respond.send(Err(error()));
        }
        TurnSignal::DurableEvent { respond, .. } => {
            let _ = respond.send(Err(error()));
        }
        TurnSignal::Todo(request) => request.reject_recovery(),
        TurnSignal::Approval { respond, .. } => {
            let _ = respond.send(ApprovalDecision::Deny);
        }
        TurnSignal::Question { respond, .. } => {
            let _ = respond.send(Err(rw_tools::ToolError::Cancelled));
        }
        TurnSignal::ManualCompactionComplete {
            completion, result, ..
        } => {
            if let Err(AgentLoopError::EffectsUnsettled(message)) = result {
                state.unsettled.get_or_insert(message);
            }
            if let Some(completion) = completion {
                let _ = completion.send(Err(error()));
            }
        }
        TurnSignal::EffectsUnsettled { message } => {
            state.unsettled.get_or_insert(message);
        }
        TurnSignal::PluginToolComplete { result, .. } => {
            if let Err(AgentLoopError::EffectsUnsettled(message)) = result {
                state.unsettled.get_or_insert(message);
            }
        }
        // These workers completed logically, but their source projection is no
        // longer authoritative. ActorTasks separately owns physical settlement.
        TurnSignal::ToolResultsUnsettled { .. }
        | TurnSignal::Event(_)
        | TurnSignal::ToolOutput { .. }
        | TurnSignal::SubagentProgress(_)
        | TurnSignal::ToolProgress(_)
        | TurnSignal::CompactionProgress(_)
        | TurnSignal::Complete(_)
        | TurnSignal::InitializationComplete { .. }
        | TurnSignal::SessionTitleGenerated { .. } => {}
    }
}
