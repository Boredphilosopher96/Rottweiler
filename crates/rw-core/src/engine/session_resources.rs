//! Lifecycle ownership for runtime resources composed beside an actor.

use super::AgentLoopError;
use async_trait::async_trait;

#[async_trait]
pub trait SessionResources: Send + Sync {
    /// Binds session-scoped capabilities before the actor can execute callbacks.
    /// # Errors
    /// Rejects an unavailable owner or inconsistent session/generation binding.
    fn bind_session(&self, binding: super::PluginSessionBinding) -> Result<(), AgentLoopError>;
    async fn shutdown(&self) -> Result<(), AgentLoopError>;
}

#[derive(Default)]
pub struct NoopSessionResources;

#[async_trait]
impl SessionResources for NoopSessionResources {
    fn bind_session(&self, _binding: super::PluginSessionBinding) -> Result<(), AgentLoopError> {
        Ok(())
    }
    async fn shutdown(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }
}
