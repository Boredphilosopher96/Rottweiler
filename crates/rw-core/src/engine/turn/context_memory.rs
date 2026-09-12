//! Shared working admission precedes context copies, token planning and TOON.
mod profiles;
use crate::engine::{
    AgentLoopError,
    recovery::{ConversationSource, HistoryWorkingAllowance},
    session::SessionActorConfig,
};
use rw_types::Turn;
use std::collections::VecDeque;

pub(super) const TOON_WORKING_BYTES: usize = 32 * 1024 * 1024;
// The bounded profiles and temporary source membership set are admitted before
// scanning any source. Body walks borrow their already-admitted source values.
const PROFILE_BASE_BYTES: usize = 64 * 1024;
const PROFILE_BYTES_PER_SOURCE: usize = 512;
pub(in crate::engine) struct ContextWorkPlan {
    allowance: Box<dyn HistoryWorkingAllowance>,
    bytes: usize,
    generation: usize,
    profiles: profiles::Profiles,
    pub(super) cache: std::sync::Mutex<super::context_cache::ContextCache>,
}
pub(in crate::engine) type ContextWorkingSet = ContextWorkPlan;

pub(in crate::engine) fn admit(
    mut reserved: Box<dyn HistoryWorkingAllowance>,
    config: &SessionActorConfig,
    conversation: &[Turn],
    sources: &[ConversationSource],
    queued: &VecDeque<String>,
) -> Result<ContextWorkingSet, AgentLoopError> {
    let metadata = profile_metadata(conversation, sources)?;
    reserved.resize(metadata)?;
    readmit(
        ContextWorkPlan {
            allowance: reserved,
            bytes: metadata,
            generation: std::ptr::from_ref(config) as usize,
            profiles: profiles::Profiles::default(),
            cache: std::sync::Mutex::default(),
        },
        config,
        conversation,
        sources,
        queued,
    )
}
/// The run/query retains its immutable configuration while this working owner
/// exists. Replacing that owner invalidates both source profiles and normalized data.
pub(in crate::engine) fn readmit(
    reserved: ContextWorkingSet,
    config: &SessionActorConfig,
    conversation: &[Turn],
    sources: &[ConversationSource],
    queued: &VecDeque<String>,
) -> Result<ContextWorkingSet, AgentLoopError> {
    let mut plan = reserved;
    let generation = std::ptr::from_ref(config) as usize;
    if plan.generation != generation {
        plan.cache = std::sync::Mutex::default();
        plan.profiles = profiles::Profiles::default();
        plan.generation = generation;
    }
    let metadata = profile_metadata(conversation, sources)?;
    plan.allowance.resize(plan.bytes.max(metadata))?;
    plan.bytes = plan.bytes.max(metadata);
    let planned = plan
        .profiles
        .planned(config, conversation, sources, queued)?
        .checked_add(metadata)
        .ok_or_else(|| invalid("context working allocation overflow"))?;
    // Retain the high-water until this owner is dropped. A previous normalized
    // cache may still exist until assembly removes its discarded sources.
    let bytes = plan.bytes.max(planned);
    if bytes > crate::engine::recovery::MAX_HISTORY_RESULT_BYTES {
        return Err(invalid(
            "context transformation exceeds shared working admission",
        ));
    }
    plan.allowance.resize(bytes)?;
    plan.bytes = bytes;
    Ok(plan)
}
impl ContextWorkPlan {
    #[cfg(test)]
    pub(in crate::engine) fn normalizations(&self) -> u64 {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .normalizations
    }
    #[cfg(test)]
    pub(in crate::engine) const fn profile_scans(&self) -> u64 {
        self.profiles.scans
    }
    pub(super) fn validate(&self) -> Result<(), AgentLoopError> {
        if self.bytes > crate::engine::recovery::MAX_HISTORY_RESULT_BYTES {
            return Err(invalid(
                "context transformation exceeds shared working admission",
            ));
        }
        Ok(())
    }
}
fn profile_metadata(
    conversation: &[Turn],
    sources: &[ConversationSource],
) -> Result<usize, AgentLoopError> {
    if conversation.len() != sources.len()
        || sources.len() > crate::engine::recovery::MAX_MATERIALIZED_HISTORY_TURNS
    {
        return Err(invalid("context allocation source alignment"));
    }
    // Includes B-tree node slack for the retained profile and temporary membership
    // set. Their bounded entries contain only a source number and five counters.
    Ok(PROFILE_BASE_BYTES + sources.len() * PROFILE_BYTES_PER_SOURCE)
}
fn invalid(message: &str) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(message.into())
}
