//! Result admission reserves a complete error suffix before any body is published.
use super::{
    signals::TurnSignal,
    tool_requests::{PendingToolCall, ToolExecution},
    tool_result_budget::ToolResultBudget,
};
use crate::engine::{ActorState, AgentLoopError, SessionActorConfig, task_ownership::ActorTasks};
use rw_types::{EventMeta, SequenceId, tool_result_admission::ToolResultAdmission};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub(super) struct ResultProfiles {
    // Published prefix followed by reserved error completions for every remaining call.
    parts: Vec<ToolResultAdmission>,
}
impl ResultProfiles {
    pub(super) fn new<'a>(
        calls: impl Iterator<Item = &'a PendingToolCall>,
    ) -> Result<Self, AgentLoopError> {
        Ok(Self {
            parts: calls
                .map(super::tool_result_profile::fallback)
                .collect::<Result<_, _>>()?,
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn admit(
        &mut self,
        index: usize,
        execution: ToolExecution,
        turn: u64,
        tasks: &ActorTasks,
        config: &Arc<SessionActorConfig>,
        owner: &ToolResultBudget,
        signals: &mpsc::UnboundedSender<TurnSignal>,
    ) -> Result<ToolExecution, AgentLoopError> {
        let (mut execution, profile) =
            super::tool_result_profile::completion(execution, tasks, config, owner.clone()).await?;
        let reserved = std::mem::replace(&mut self.parts[index], profile);
        if check(signals, turn, &self.parts).await.is_err() {
            self.parts[index] = reserved;
            super::tool_result_budget::reject(&mut execution);
            // A broken header/configuration is not a settled result. The caller preserves repair.
            check(signals, turn, &self.parts).await?;
        }
        Ok(execution)
    }
}
async fn check(
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    parts: &[ToolResultAdmission],
) -> Result<(), AgentLoopError> {
    let logical = ToolResultAdmission::combine(parts)
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
    let (respond, receive) = oneshot::channel();
    signals
        .send(TurnSignal::AdmitToolResults {
            turn,
            logical,
            respond,
        })
        .map_err(|_| AgentLoopError::Closed)?;
    receive.await.map_err(|_| AgentLoopError::Closed)?
}
pub(super) fn validate(
    state: &ActorState,
    turn: u64,
    logical: &ToolResultAdmission,
) -> Result<(), AgentLoopError> {
    if state.running.as_ref().map(|running| running.id) != Some(turn) || state.unsettled.is_some() {
        return Err(AgentLoopError::EffectsUnsettled(
            "tool result closure lost its active owner".into(),
        ));
    }
    // Sequence width cannot grow past this reservation. The final append also validates
    // its exact timestamp, cause and prefix before any selector is published.
    let meta = EventMeta {
        protocol_version: rw_types::PROTOCOL_VERSION,
        session_id: state.session_id.clone(),
        sequence_id: SequenceId(u64::MAX),
        emitted_at: state.event_clock.emitted_at(),
        caused_by: state.caused_by(),
    };
    crate::engine::recovery::tool_results::validate_admission(&meta, turn, logical)
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))
}
