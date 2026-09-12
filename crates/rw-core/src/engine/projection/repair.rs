//! Deterministic interrupted tool repair shared by canonical and audit recovery.
use super::{InterruptedToolRepair, InterruptedToolStart};
use crate::engine::recovery::ToolStartIdentity;
use rw_types::{Block, Role, ToolInvocationId, ToolOutput, Turn, TurnMeta};

pub(in crate::engine) struct InterruptedRepair {
    pub(in crate::engine) tools: Vec<InterruptedToolRepair>,
    pub(in crate::engine) tool_turn: Option<Turn>,
}

/// Only a committed provider call permits a model `ToolResult`. A tool-only
/// invocation still receives its durable terminal event, without inventing IR.
pub(in crate::engine) fn repair_tools<'a>(
    agent_turn: u64,
    conversation: impl Iterator<Item = &'a Turn>,
    mut starts: Vec<ToolStartIdentity>,
    completed: Vec<Block>,
) -> InterruptedRepair {
    let mut pending = Vec::new();
    let mut ordinal = 0_usize;
    for turn in conversation {
        let mut call_index = 0;
        for block in &turn.blocks {
            match block {
                Block::ToolCall { id, name, args } => {
                    pending.push((ordinal, call_index, id, name, args));
                    ordinal += 1;
                    call_index += 1;
                }
                Block::ToolResult { id, .. } => {
                    if let Some(index) = pending.iter().position(|call| call.2 == id) {
                        pending.remove(index);
                    }
                }
                _ => {}
            }
        }
    }
    let mut blocks = Vec::new();
    for block in completed {
        if let Block::ToolResult { id, .. } = &block
            && let Some(index) = pending.iter().position(|call| call.2 == id)
        {
            pending.remove(index);
            blocks.push(block);
        }
    }
    let mut tools = Vec::new();
    for (ordinal, call_index, id, name, arguments) in pending {
        let started = starts
            .iter()
            .position(|start| &start.tool_call_id == id)
            .map(|index| starts.remove(index));
        let (invocation_id, index, missing_start) = if let Some(start) = started {
            (start.invocation_id, start.index, None)
        } else {
            (
                ToolInvocationId(format!("turn-{agent_turn}:repair-{ordinal}")),
                call_index,
                Some(InterruptedToolStart {
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
            )
        };
        let output = interrupted_output();
        tools.push(InterruptedToolRepair {
            agent_turn,
            call_index: index,
            tool_call_id: id.clone(),
            invocation_id,
            missing_start,
            output: output.clone(),
        });
        blocks.push(Block::ToolResult {
            id: id.clone(),
            output,
            is_error: true,
        });
    }
    tools.extend(starts.into_iter().map(|start| InterruptedToolRepair {
        agent_turn,
        call_index: start.index,
        tool_call_id: start.tool_call_id,
        invocation_id: start.invocation_id,
        missing_start: None,
        output: interrupted_output(),
    }));
    let tool_turn = (!blocks.is_empty()).then_some(Turn {
        role: Role::Tool,
        blocks,
        meta: TurnMeta::default(),
    });
    InterruptedRepair { tools, tool_turn }
}

fn interrupted_output() -> ToolOutput {
    ToolOutput::Text {
        text: "tool call was interrupted before a result was persisted".to_owned(),
    }
}
