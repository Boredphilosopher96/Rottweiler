//! Explicit driver authority for a response to one owned descendant control.
use super::{ActorCommand, SessionHandle};
use crate::engine::AgentLoopError;
use rw_types::{
    ClientCommand, ClientId, CommandMeta, CommandOutcome, SequenceId, SessionId,
    family_controls::{ChildControlResponse, ChildControlsSnapshot},
};
use std::sync::Arc;
use tokio::sync::oneshot;

/// A live root-driver proof. It grants only a typed control response, never
/// attachment, provider credentials, or a child driver lease.
#[derive(Clone)]
pub struct FamilyControlAuthority {
    root: Arc<super::SessionControl>,
    root_id: SessionId,
    client: ClientId,
}
impl FamilyControlAuthority {
    /// The root whose committed driver authorizes this narrow response.
    #[must_use]
    pub fn root_session_id(&self) -> &SessionId {
        &self.root_id
    }

    pub(crate) fn valid(&self, client: &ClientId) -> bool {
        self.client == *client && self.root.authorizes(client)
    }
}
impl SessionHandle {
    /// # Errors
    /// Rejects callers that do not hold this root's committed driver lease.
    pub fn family_control_authority(
        &self,
        client: &ClientId,
    ) -> Result<FamilyControlAuthority, AgentLoopError> {
        if !self.shutdown.control.authorizes(client) {
            return Err(invalid("family response requires the root driver"));
        }
        Ok(FamilyControlAuthority {
            root: Arc::clone(&self.shutdown.control),
            root_id: self.session_id.clone(),
            client: client.clone(),
        })
    }
    /// # Errors
    /// Rejects closed actors or control snapshots exceeding their admitted bound.
    pub async fn child_controls(&self) -> Result<ChildControlsSnapshot, AgentLoopError> {
        let (respond, reply) = oneshot::channel();
        self.commands
            .send(ActorCommand::ChildControls { respond })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        reply.await.map_err(|_| AgentLoopError::Closed)?
    }
    /// # Errors
    /// Rejects stale control fences, changed driver authority, or invalid responses.
    pub async fn respond_child_control(
        &self,
        authority: FamilyControlAuthority,
        meta: CommandMeta,
        expected_revision: SequenceId,
        response: ChildControlResponse,
    ) -> Result<CommandOutcome, AgentLoopError> {
        let (respond, reply) = oneshot::channel();
        let (completion, completed) = oneshot::channel();
        self.commands
            .send(ActorCommand::ChildControl {
                authority,
                command: response.command(meta, self.session_id.clone()),
                expected_revision,
                respond,
                completion,
            })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        let outcome = reply.await.map_err(|_| AgentLoopError::Closed)?;
        if matches!(outcome, CommandOutcome::Accepted {}) {
            completed.await.map_err(|_| AgentLoopError::Closed)??;
        }
        Ok(outcome)
    }
}
trait ResponseCommand {
    fn command(self, meta: CommandMeta, session_id: SessionId) -> ClientCommand;
}
impl ResponseCommand for ChildControlResponse {
    fn command(self, meta: CommandMeta, session_id: SessionId) -> ClientCommand {
        match self {
            Self::Question {
                question_id,
                answers,
            } => ClientCommand::AnswerQuestion {
                meta,
                session_id,
                question_id,
                answers,
            },
            Self::Approval {
                tool_call_id,
                invocation_id,
                decision,
                binding,
            } => ClientCommand::ApproveTool {
                meta,
                session_id,
                tool_call_id,
                invocation_id,
                decision,
                binding,
            },
            Self::Plan {
                decision,
                revisions,
            } => ClientCommand::ApprovePlan {
                meta,
                session_id,
                decision,
                revisions,
            },
        }
    }
}
fn invalid(message: &str) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(message.into())
}
