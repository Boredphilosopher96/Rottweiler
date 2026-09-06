//! Allocation profiles follow immutable sources; warm requests do not rewalk JSON.
use super::invalid;
use crate::engine::{
    AgentLoopError,
    recovery::{ConversationSource, MAX_MATERIALIZED_HISTORY_TURNS},
    session::SessionActorConfig,
};
use rw_types::{Block, ToolOutput, ToolOutputPart, Turn, allocation::PrepareAllocation};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::Write,
};

#[derive(Default)]
pub(super) struct Profiles {
    stable: Option<Profile>,
    turns: BTreeMap<u64, Profile>,
    #[cfg(test)]
    pub scans: u64,
}
#[derive(Clone, Copy, Default)]
struct Profile {
    payload: usize,
    metadata: usize,
    transformed: usize,
    scratch: usize,
    encoded: usize,
}
impl Profiles {
    pub fn planned(
        &mut self,
        config: &SessionActorConfig,
        conversation: &[Turn],
        sources: &[ConversationSource],
        queued: &VecDeque<String>,
    ) -> Result<usize, AgentLoopError> {
        if conversation.len() != sources.len() || sources.len() > MAX_MATERIALIZED_HISTORY_TURNS {
            return Err(invalid("context allocation source alignment"));
        }
        let selected = sources
            .iter()
            .map(|source| source.sequence.0)
            .collect::<BTreeSet<_>>();
        if selected.len() != sources.len() {
            return Err(invalid("duplicate context allocation source"));
        }
        self.turns.retain(|sequence, _| selected.contains(sequence));
        let mut total = if let Some(profile) = self.stable {
            profile
        } else {
            let profile = stable_profile(config)?;
            self.stable = Some(profile);
            profile
        };
        for (turn, source) in conversation.iter().zip(sources) {
            let profile = match self.turns.entry(source.sequence.0) {
                std::collections::btree_map::Entry::Occupied(entry) => *entry.get(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let profile = turn_profile(turn)?;
                    #[cfg(test)]
                    {
                        self.scans += 1;
                    }
                    *entry.insert(profile)
                }
            };
            total.add(profile)?;
        }
        for content in queued {
            add(
                &mut total.payload,
                content.capacity().checked_add(std::mem::size_of::<Turn>()),
            )?;
            add(&mut total.metadata, Some(512))?;
        }
        total.working_bytes()
    }
}
impl Profile {
    fn add(&mut self, other: Self) -> Result<(), AgentLoopError> {
        add(&mut self.payload, Some(other.payload))?;
        add(&mut self.metadata, Some(other.metadata))?;
        add(&mut self.transformed, Some(other.transformed))?;
        add(&mut self.encoded, Some(other.encoded))?;
        self.scratch = self.scratch.max(other.scratch);
        Ok(())
    }
    fn working_bytes(self) -> Result<usize, AgentLoopError> {
        // Source materializations have their own allowance. Four payload copies
        // cover cached normalization, request assembly, pruning and replacement;
        // cached token/profile metadata and the largest encoder workspace are explicit.
        self.payload
            .checked_mul(4)
            .and_then(|bytes| bytes.checked_add(self.encoded.checked_mul(4)?))
            .and_then(|bytes| bytes.checked_add(self.transformed.checked_mul(3)?))
            .and_then(|bytes| bytes.checked_add(self.metadata.checked_mul(4)?))
            .and_then(|bytes| bytes.checked_add(self.scratch))
            .and_then(|bytes| bytes.checked_add(rw_store::prompt_shapes::MAX_PROFILE_BYTES))
            .and_then(|bytes| bytes.checked_add(rw_store::prompt_shapes::MAX_PROFILE_DECODE_BYTES))
            .ok_or_else(|| invalid("context working allocation overflow"))
    }
}
fn turn_profile(turn: &Turn) -> Result<Profile, AgentLoopError> {
    let mut profile = Profile::default();
    add(&mut profile.payload, turn.prepared_bytes())?;
    add(
        &mut profile.metadata,
        turn.blocks
            .len()
            .checked_mul(512)
            .and_then(|bytes| bytes.checked_add(512)),
    )?;
    for block in &turn.blocks {
        if let Block::ToolResult { output, .. } = block {
            output_plan(output, &mut profile.transformed, &mut profile.scratch)?;
        }
    }
    Ok(profile)
}
fn stable_profile(config: &SessionActorConfig) -> Result<Profile, AgentLoopError> {
    let mut profile = Profile::default();
    let mut encoded = Counter(0);
    for turn in &config.initial_session_context {
        profile.add(turn_profile(turn)?)?;
        serde_json::to_writer(&mut encoded, turn).map_err(|_| invalid("context prefix size"))?;
    }
    for tool in config.tools.descriptor_refs() {
        add(
            &mut profile.payload,
            tool.name
                .capacity()
                .checked_add(tool.description.capacity())
                .and_then(|bytes| bytes.checked_add(tool.input_schema.prepared_bytes()?)),
        )?;
        add(&mut profile.metadata, Some(1024))?;
        serde_json::to_writer(&mut encoded, tool).map_err(|_| invalid("tool schema size"))?;
    }
    profile.encoded = encoded.0;
    Ok(profile)
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
        .filter(|plan| plan.working_bytes <= super::TOON_WORKING_BYTES)
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
