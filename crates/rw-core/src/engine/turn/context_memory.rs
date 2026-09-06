//! Shared working admission precedes context copies, token planning and TOON.
use crate::engine::{AgentLoopError, recovery::HistoryRead, session::SessionActorConfig};
use rw_types::{Block, ToolOutput, ToolOutputPart, Turn, allocation::PrepareAllocation};
use std::{collections::VecDeque, io::Write};

pub(super) const TOON_WORKING_BYTES: usize = 32 * 1024 * 1024;
pub(in crate::engine) struct ContextWorkPlan {
    bytes: usize,
}
pub(in crate::engine) type ContextWorkingSet = HistoryRead<ContextWorkPlan>;

pub(in crate::engine) fn admit(
    reserved: HistoryRead<()>,
    config: &SessionActorConfig,
    conversation: &[Turn],
    queued: &VecDeque<String>,
) -> Result<ContextWorkingSet, AgentLoopError> {
    let bytes = planned_bytes(config, conversation, queued)?;
    if bytes > crate::engine::recovery::MAX_HISTORY_RESULT_BYTES {
        return Err(invalid(
            "context transformation exceeds shared working admission",
        ));
    }
    Ok(reserved.map(|()| ContextWorkPlan { bytes }))
}
impl ContextWorkPlan {
    pub(super) fn validate(&self) -> Result<(), AgentLoopError> {
        if self.bytes > crate::engine::recovery::MAX_HISTORY_RESULT_BYTES {
            return Err(invalid("context working reservation is insufficient"));
        }
        Ok(())
    }
}

fn planned_bytes(
    config: &SessionActorConfig,
    conversation: &[Turn],
    queued: &VecDeque<String>,
) -> Result<usize, AgentLoopError> {
    let mut payload = 0usize;
    let mut metadata = 0usize;
    let mut transformed = 0usize;
    let mut scratch = 0usize;
    for turn in config.initial_session_context.iter().chain(conversation) {
        add(&mut payload, turn.prepared_bytes())?;
        add(
            &mut metadata,
            turn.blocks
                .len()
                .checked_mul(512)
                .and_then(|bytes| bytes.checked_add(512)),
        )?;
        for block in &turn.blocks {
            if let Block::ToolResult { output, .. } = block {
                output_plan(output, &mut transformed, &mut scratch)?;
            }
        }
    }
    for content in queued {
        add(
            &mut payload,
            content.capacity().checked_add(std::mem::size_of::<Turn>()),
        )?;
        add(&mut metadata, Some(512))?;
    }
    let mut stable = Counter(0);
    for turn in &config.initial_session_context {
        serde_json::to_writer(&mut stable, turn).map_err(|_| invalid("context prefix size"))?;
    }
    for tool in config.tools.descriptor_refs() {
        add(
            &mut payload,
            tool.name
                .capacity()
                .checked_add(tool.description.capacity())
                .and_then(|bytes| bytes.checked_add(tool.input_schema.prepared_bytes()?)),
        )?;
        add(&mut metadata, Some(1024))?;
        serde_json::to_writer(&mut stable, tool).map_err(|_| invalid("tool schema size"))?;
    }
    // Source materializations retain their own allowance. This reservation covers
    // request-local clones, cached normalization, pruning inspection, stable-prefix
    // JSON/hash construction and the largest live TOON encoder/decoder workspace.
    payload
        .checked_mul(4)
        .and_then(|bytes| bytes.checked_add(stable.0.checked_mul(4)?))
        .and_then(|bytes| bytes.checked_add(transformed.checked_mul(3)?))
        .and_then(|bytes| bytes.checked_add(metadata.checked_mul(4)?))
        .and_then(|bytes| bytes.checked_add(scratch))
        .ok_or_else(|| invalid("context working allocation overflow"))
}
fn output_plan(
    output: &ToolOutput,
    transformed: &mut usize,
    scratch: &mut usize,
) -> Result<(), AgentLoopError> {
    match output {
        ToolOutput::Structured { value } => value_plan(value, transformed, scratch),
        ToolOutput::Mixed { parts } => {
            for part in parts {
                if let ToolOutputPart::Structured { value } = part {
                    value_plan(value, transformed, scratch)?;
                }
            }
            Ok(())
        }
        ToolOutput::Text { .. } => Ok(()),
    }
}
fn value_plan(
    value: &serde_json::Value,
    transformed: &mut usize,
    scratch: &mut usize,
) -> Result<(), AgentLoopError> {
    if let Some(plan) = rw_context::ToonAllocation::for_value(value)
        .filter(|plan| plan.working_bytes <= TOON_WORKING_BYTES)
    {
        add(transformed, Some(plan.prompt_bytes))?;
        *scratch = (*scratch).max(plan.working_bytes);
    }
    Ok(())
}
fn add(total: &mut usize, bytes: Option<usize>) -> Result<(), AgentLoopError> {
    *total = total
        .checked_add(bytes.ok_or_else(|| invalid("context allocation profile"))?)
        .ok_or_else(|| invalid("context allocation overflow"))?;
    Ok(())
}
struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("context size overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn invalid(message: &str) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(message.into())
}
