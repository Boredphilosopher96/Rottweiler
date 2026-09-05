//! Effective durable artifact queries. Result acknowledgements never grant authority by themselves.
use super::OrchestrationError;
use async_trait::async_trait;
use rw_types::{SessionId, SubagentId, SubagentResult};

#[async_trait]
pub trait SubagentArtifactSource: rw_tools::DiffArtifactAuthority {
    /// Verify the acknowledged result against its committed source before the
    /// child is exposed as inactive or its worktree can be released.
    /// # Errors
    /// Rejects a missing or mismatched durable terminal result.
    async fn verify_result(
        &self,
        parent: &SessionId,
        result: &SubagentResult,
    ) -> Result<(), OrchestrationError>;

    /// Resolve the latest effective child result's optional artifact reference.
    /// # Errors
    /// Rejects unavailable or corrupt source authority.
    async fn latest(
        &self,
        parent: &SessionId,
        subagent: &SubagentId,
    ) -> Result<Option<String>, OrchestrationError>;
}
