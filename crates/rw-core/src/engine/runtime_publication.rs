//! Publication occurs only after the actor commits and installs one complete runtime.
use super::AgentLoopError;
use std::sync::Arc;

/// A host-owned prepared generation keeps external admission closed until this
/// synchronous commit. Dropping its owner must retain any unproven effects.
pub trait PreparedRuntimePublication: Send + Sync {
    /// # Errors
    /// Reports lost publication authority; the session must close its admission.
    fn publish(&self) -> Result<(), AgentLoopError>;
}

#[derive(Clone)]
pub enum RuntimePublication {
    /// Registries contain no deferred external generation to activate.
    Active,
    /// Native endpoints, provider routes and event workers share this commit.
    Prepared(Arc<dyn PreparedRuntimePublication>),
}
impl RuntimePublication {
    /// # Errors
    /// Returns the prepared owner's publication failure.
    pub fn publish(&self) -> Result<(), AgentLoopError> {
        match self {
            Self::Active => Ok(()),
            Self::Prepared(owner) => owner.publish(),
        }
    }
}
