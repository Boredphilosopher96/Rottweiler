//! Tool IR profiling borrows its body in a retained, CPU-admitted worker.
use crate::engine::{AgentLoopError, SessionActorConfig, task_ownership::ActorTasks};
use rw_tools::CancellationToken;
use rw_types::{Turn, allocation::PrepareAllocation, tool_result_admission::ToolResultAdmission};
use std::sync::Arc;

pub(super) async fn profile(
    turn: Turn,
    tasks: &ActorTasks,
    config: &Arc<SessionActorConfig>,
    owner: super::tool_result_budget::ToolResultBudget,
) -> Result<
    (
        Turn,
        ToolResultAdmission,
        super::tool_result_budget::ToolResultBudget,
    ),
    AgentLoopError,
> {
    let retained = turn
        .prepared_bytes()
        .ok_or_else(|| invalid("tool IR allocation overflow"))?;
    let task = tasks
        .spawn_blocking(
            Arc::clone(config),
            CancellationToken::default(),
            rw_resources::ResourceClass::Cpu,
            move || {
                let logical = ToolResultAdmission::measure(&turn)
                    .map_err(|error| invalid(&error.to_string()))?;
                owner.finish_profile(retained)?;
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

/// Singleton profiles are admitted before a completion becomes authoritative.
pub(super) async fn completion(
    mut execution: super::tool_requests::ToolExecution,
    tasks: &ActorTasks,
    config: &Arc<SessionActorConfig>,
    owner: super::tool_result_budget::ToolResultBudget,
) -> Result<(super::tool_requests::ToolExecution, ToolResultAdmission), AgentLoopError> {
    let task = tasks
        .spawn_blocking(
            Arc::clone(config),
            CancellationToken::default(),
            rw_resources::ResourceClass::Cpu,
            move || {
                let _owner = owner;
                let logical = if let Ok(logical) = measure_execution(&mut execution) {
                    logical
                } else {
                    super::tool_result_budget::reject(&mut execution);
                    measure_execution(&mut execution)
                        .map_err(|error| invalid(&error.to_string()))?
                };
                Ok::<_, AgentLoopError>((execution, logical))
            },
        )
        .await?;
    task.await
        .map_err(|error| invalid(&format!("result profile task: {error}")))?
}
fn measure_execution(
    execution: &mut super::tool_requests::ToolExecution,
) -> Result<ToolResultAdmission, serde_json::Error> {
    let mut turn = rw_types::Turn {
        role: rw_types::Role::Tool,
        blocks: vec![rw_types::Block::ToolResult {
            id: rw_types::ToolCallId(execution.call.id.clone()),
            output: std::mem::replace(
                &mut execution.output,
                rw_types::ToolOutput::Text {
                    text: String::new(),
                },
            ),
            is_error: execution.is_error,
        }],
        meta: rw_types::TurnMeta::default(),
    };
    let result = ToolResultAdmission::measure(&turn);
    if let Some(rw_types::Block::ToolResult { output, .. }) = turn.blocks.pop() {
        execution.output = output;
    }
    result
}
pub(super) fn fallback(
    call: &super::tool_requests::PendingToolCall,
) -> Result<ToolResultAdmission, AgentLoopError> {
    ToolResultAdmission::measure(&rw_types::Turn {
        role: rw_types::Role::Tool,
        blocks: vec![rw_types::Block::ToolResult {
            id: rw_types::ToolCallId(call.id.clone()),
            output: rw_types::ToolOutput::Text {
                text: super::tool_result_budget::REJECTED_OUTPUT.into(),
            },
            is_error: true,
        }],
        meta: rw_types::TurnMeta::default(),
    })
    .map_err(|error| invalid(&error.to_string()))
}
