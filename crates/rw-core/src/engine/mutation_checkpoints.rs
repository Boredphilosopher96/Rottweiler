use super::AgentLoopError;
use async_trait::async_trait;
use rw_tools::MutationScope;
use rw_types::ReviewFileDecision;
use rw_types::SessionId;
use rw_types::SessionReview;
use rw_types::UnrestorablePath;
use std::path::Path;

/// Opaque checkpoint handle returned before a mutating tool starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationCheckpoint {
    pub id: Option<String>,
}

/// Terminal disposition reported after a checkpointed tool attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationCheckpointOutcome {
    Completed,
    Failed,
    Cancelled,
}

/// Opaque handle for a prepared and applied rewind transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindCheckpoint {
    pub id: String,
    pub unrestorable_paths: Vec<UnrestorablePath>,
}

/// Storage-neutral boundary used around every mutating tool execution.
#[async_trait]
pub trait MutationCheckpointCoordinator: Send + Sync {
    /// Waits for retained checkpoint workers and reports an unacknowledged workspace outcome.
    /// The caller must settle tool effects and finish active checkpoint handles first.
    async fn settle_effects(&self) -> Result<(), AgentLoopError>;

    async fn begin(
        &self,
        session_id: &SessionId,
        agent_turn: u64,
        tool_call_id: &str,
        scope: &MutationScope,
    ) -> Result<MutationCheckpoint, AgentLoopError>;

    async fn finish(
        &self,
        checkpoint: &MutationCheckpoint,
        outcome: MutationCheckpointOutcome,
    ) -> Result<(), AgentLoopError>;

    async fn prepare_apply_rewind(
        &self,
        session_id: &SessionId,
        to_turn: u64,
        operation_id: &str,
    ) -> Result<RewindCheckpoint, AgentLoopError>;

    async fn acknowledge_rewind(&self, checkpoint: &RewindCheckpoint)
    -> Result<(), AgentLoopError>;

    /// Returns a complete cumulative review snapshot for one session.
    async fn session_review(
        &self,
        _session_id: &SessionId,
    ) -> Result<SessionReview, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "session review is not configured".to_owned(),
        ))
    }

    /// Resolves one fingerprint-bound review entry and returns a full snapshot.
    async fn resolve_review_file(
        &self,
        _session_id: &SessionId,
        _path: &Path,
        _decision: ReviewFileDecision,
        _current_hash: &str,
    ) -> Result<SessionReview, AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "session review is not configured".to_owned(),
        ))
    }
}

/// Checkpoint coordinator for read-only or ephemeral sessions.
#[derive(Debug, Default)]
pub struct NoopMutationCheckpointCoordinator;

#[async_trait]
impl MutationCheckpointCoordinator for NoopMutationCheckpointCoordinator {
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }

    async fn begin(
        &self,
        _session_id: &SessionId,
        _agent_turn: u64,
        _tool_call_id: &str,
        _scope: &MutationScope,
    ) -> Result<MutationCheckpoint, AgentLoopError> {
        Ok(MutationCheckpoint { id: None })
    }

    async fn finish(
        &self,
        _checkpoint: &MutationCheckpoint,
        _outcome: MutationCheckpointOutcome,
    ) -> Result<(), AgentLoopError> {
        Ok(())
    }

    async fn prepare_apply_rewind(
        &self,
        _session_id: &SessionId,
        _to_turn: u64,
        operation_id: &str,
    ) -> Result<RewindCheckpoint, AgentLoopError> {
        Ok(RewindCheckpoint {
            id: operation_id.to_owned(),
            unrestorable_paths: Vec::new(),
        })
    }

    async fn acknowledge_rewind(
        &self,
        _checkpoint: &RewindCheckpoint,
    ) -> Result<(), AgentLoopError> {
        Ok(())
    }

    async fn session_review(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionReview, AgentLoopError> {
        Ok(SessionReview {
            session_id: session_id.clone(),
            files: Vec::new(),
        })
    }

    async fn resolve_review_file(
        &self,
        session_id: &SessionId,
        _path: &Path,
        _decision: ReviewFileDecision,
        _current_hash: &str,
    ) -> Result<SessionReview, AgentLoopError> {
        self.session_review(session_id).await
    }
}
