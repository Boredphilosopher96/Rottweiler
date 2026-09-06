//! Tool IR profiling borrows its body in a retained, CPU-admitted worker.
use crate::engine::{
    AgentLoopError, SessionActorConfig, recovery::HistoryWorkingAllowance,
    task_ownership::ActorTasks,
};
use rw_tools::CancellationToken;
use rw_types::{
    Turn,
    allocation::PrepareAllocation,
    tool_result_admission::{MAX_TOOL_RESULT_IR_BYTES, ToolResultAdmission},
};
use std::sync::Arc;

pub(super) async fn profile(
    turn: Turn,
    tasks: &ActorTasks,
    config: &Arc<SessionActorConfig>,
    cancellation: &CancellationToken,
) -> Result<(Turn, ToolResultAdmission, Box<dyn HistoryWorkingAllowance>), AgentLoopError> {
    let retained = turn
        .prepared_bytes()
        .ok_or_else(|| invalid("tool IR allocation overflow"))?;
    // Encoding and the structural visitor's largest escaped-string scratch coexist.
    let peak = retained
        .checked_add(2 * MAX_TOOL_RESULT_IR_BYTES)
        .filter(|bytes| *bytes <= crate::engine::recovery::MAX_HISTORY_RESULT_BYTES)
        .ok_or_else(|| invalid("tool IR profiling exceeds working admission"))?;
    let mut owner = super::history_context::reserve_working(config).await?;
    owner.resize(peak)?;
    let task = tasks
        .spawn_blocking(
            Arc::clone(config),
            cancellation.clone(),
            rw_resources::ResourceClass::Cpu,
            move || {
                let logical = ToolResultAdmission::measure(&turn)
                    .map_err(|error| invalid(&error.to_string()))?;
                owner.resize(retained)?;
                Ok::<_, AgentLoopError>((turn, logical, owner))
            },
        )
        .await?;
    task.await.map_err(|error| {
        AgentLoopError::EffectsUnsettled(format!("tool result profiling worker failed: {error}"))
    })?
}
fn invalid(message: &str) -> AgentLoopError {
    AgentLoopError::Persistence(message.into())
}
