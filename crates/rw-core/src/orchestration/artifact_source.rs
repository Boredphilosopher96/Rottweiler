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

    /// Reads a completed retained child's durable result after process restart.
    /// # Errors
    /// Rejects unavailable or corrupt source authority.
    async fn completed_result(
        &self,
        _parent: &SessionId,
        _subagent: &SubagentId,
    ) -> Result<Option<SubagentResult>, OrchestrationError> {
        Ok(None)
    }

    /// Resolve the latest effective child result's optional artifact reference.
    /// # Errors
    /// Rejects unavailable or corrupt source authority.
    async fn latest(
        &self,
        parent: &SessionId,
        subagent: &SubagentId,
    ) -> Result<Option<String>, OrchestrationError>;
}
