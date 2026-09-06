//! Test producers emit real completion bodies and exact provider references.
#![cfg(test)]
#![allow(clippy::expect_used)]
use super::PendingEvent;
use rw_types::{
    Block, SequenceId, ToolInvocationId, Turn, conversation_input::ToolResultReference,
    tool_result_admission::ToolResultAdmission,
};

/// The caller owns the active turn; returned events finish and adopt each result.
pub(in crate::engine) fn events(first: u64, agent_turn: u64, turn: &Turn) -> Vec<PendingEvent> {
    let logical = ToolResultAdmission::measure(turn).expect("valid fixture tool IR");
    let mut events = Vec::with_capacity(turn.blocks.len() * 2 + 1);
    let mut results = Vec::with_capacity(turn.blocks.len());
    for (index, block) in turn.blocks.iter().enumerate() {
        let Block::ToolResult {
            id,
            output,
            is_error,
        } = block
        else {
            panic!("result fixture block");
        };
        let invocation_id = ToolInvocationId(format!("fixture-{first}-{index}"));
        events.push(PendingEvent::ToolCallStarted {
            turn: agent_turn,
            id: id.0.clone(),
            invocation_id: invocation_id.clone(),
            name: "fixture".into(),
            arguments: serde_json::json!({}),
            index,
        });
        results.push(ToolResultReference {
            invocation_id: invocation_id.clone(),
            finished_source: SequenceId(first + events.len() as u64),
        });
        events.push(PendingEvent::ToolCallFinished {
            presentation: None,
            turn: agent_turn,
            id: id.0.clone(),
            invocation_id,
            output: output.clone(),
            is_error: *is_error,
            index,
        });
    }
    events.push(PendingEvent::ConversationToolResultsCommitted {
        agent_turn,
        results,
        logical,
    });
    events
}
