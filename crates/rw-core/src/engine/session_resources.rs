//! Lifecycle ownership for runtime resources composed beside an actor.

use super::AgentLoopError;
use async_trait::async_trait;

#[async_trait]
pub trait SessionResources: Send + Sync {
    async fn shutdown(&self) -> Result<(), AgentLoopError>;
}

#[derive(Default)]
pub struct NoopSessionResources;

#[async_trait]
impl SessionResources for NoopSessionResources {
    async fn shutdown(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }
}
