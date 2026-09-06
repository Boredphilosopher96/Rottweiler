//! Shared working admission precedes context copies, token planning and TOON.
mod profiles;
use crate::engine::{
    AgentLoopError,
    recovery::{ConversationSource, HistoryRead},
    session::SessionActorConfig,
};
use rw_types::Turn;
use std::collections::VecDeque;

pub(super) const TOON_WORKING_BYTES: usize = 32 * 1024 * 1024;
pub(in crate::engine) struct ContextWorkPlan {
    bytes: usize,
    generation: usize,
    profiles: profiles::Profiles,
    pub(super) cache: std::sync::Mutex<super::context_cache::ContextCache>,
}
pub(in crate::engine) type ContextWorkingSet = HistoryRead<ContextWorkPlan>;

pub(in crate::engine) fn admit(
    reserved: HistoryRead<()>,
    config: &SessionActorConfig,
    conversation: &[Turn],
    sources: &[ConversationSource],
    queued: &VecDeque<String>,
) -> Result<ContextWorkingSet, AgentLoopError> {
    readmit(
        reserved.map(|()| ContextWorkPlan {
            bytes: 0,
            generation: std::ptr::from_ref(config) as usize,
            profiles: profiles::Profiles::default(),
            cache: std::sync::Mutex::default(),
        }),
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
    reserved.try_map(|mut plan| {
        let generation = std::ptr::from_ref(config) as usize;
        if plan.generation != generation {
            plan.cache = std::sync::Mutex::default();
            plan.profiles = profiles::Profiles::default();
            plan.generation = generation;
        }
        plan.bytes = plan
            .profiles
            .planned(config, conversation, sources, queued)?;
        plan.validate()?;
        Ok(plan)
    })
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
fn invalid(message: &str) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(message.into())
}
