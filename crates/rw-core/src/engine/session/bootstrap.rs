//! Actor startup consumes bounded canonical controls, never an audit transcript.
use crate::engine::projection::InterruptedToolRepair;
use crate::engine::recovery::{ConversationMetadata, HistoryRead, RecoveryBootstrap};
use crate::engine::{
    AgentLoopError, RecoveredQuestion, RecoveredUserShell, SessionAccountingState,
};
use rw_context::Budgeter;
use rw_types::{
    ClientId, ModeId, PlanArtifact, SequenceId, SessionMode, Turn, WorkspaceRootDescriptor,
    config::ThinkingLevel,
};
use std::collections::BTreeMap;

#[derive(Default)]
pub struct SessionActorRecovery {
    pub title: Option<String>,
    pub conversation: ConversationMetadata,
    pub accepted_messages: Vec<crate::engine::recovery::RecoveredMessage>,
    pub queued_messages: Vec<String>,
    pub queued_message_positions: Vec<u64>,
    pub completed_turns: u64,
    pub next_turn: u64,
    pub last_sequence: Option<SequenceId>,
    pub interrupted_turn: Option<u64>,
    pub driver_client_id: Option<ClientId>,
    pub interrupted_tool_repairs: Vec<InterruptedToolRepair>,
    pub interrupted_tool_turn: Option<Turn>,
    pub interrupted_assistant_turn: Option<Turn>,
    pub pending_questions: BTreeMap<String, RecoveredQuestion>,
    pub accounting: SessionAccountingState,
    pub latest_budget: Option<rw_types::session_state::SessionBudgetState>,
    pub budgeter: Budgeter,
    pub interrupted_compaction: bool,
    pub model_alias: Option<String>,
    pub provider: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub mode: SessionMode,
    pub mode_id: Option<ModeId>,
    pub permission_mode: Option<rw_types::PermissionModeDescriptor>,
    pub pending_plan: Option<PlanArtifact>,
    pub approved_plan: Option<PlanArtifact>,
    pub plan_gate_active: bool,
    pub active_shell: Option<RecoveredUserShell>,
    pub workspace_generation: u64,
    pub workspace_roots: Vec<WorkspaceRootDescriptor>,
    pub(in crate::engine) source: Option<HistoryRead<()>>,
}
impl std::fmt::Debug for SessionActorRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionActorRecovery")
            .field("through", &self.last_sequence)
            .field("conversation", &self.conversation)
            .finish_non_exhaustive()
    }
}
impl SessionActorRecovery {
    /// Transfer admitted canonical controls into the actor's startup owner.
    /// # Errors
    /// Rejects inconsistent budget metadata or queued attachment contracts.
    pub fn from_bootstrap(
        bootstrap: HistoryRead<RecoveryBootstrap>,
    ) -> Result<Self, AgentLoopError> {
        let (bootstrap, source) = bootstrap.into_parts();
        let RecoveryBootstrap {
            head,
            controls,
            interrupted,
        } = bootstrap;
        let interrupted_compaction = head.interrupted_compaction();
        let mut queued_messages = Vec::with_capacity(controls.queued_messages.len());
        let mut queued_message_positions = Vec::with_capacity(controls.queued_messages.len());
        for (position, message) in controls.queued_messages {
            if !message.attachments.is_empty() {
                return Err(AgentLoopError::Persistence(
                    "queued messages cannot contain attachments".into(),
                ));
            }
            queued_message_positions.push(position);
            queued_messages.push(message.content);
        }
        let (interrupted_tool_repairs, interrupted_tool_turn, interrupted_assistant_turn) =
            interrupted.map_or((vec![], None, None), |repair| {
                (repair.tools, repair.tool_turn, repair.assistant_turn)
            });
        let (model_alias, provider, thinking) =
            controls.model.map_or((None, None, None), |model| {
                (Some(model.model.0), model.provider, Some(model.thinking))
            });
        Ok(Self {
            title: controls.title,
            conversation: controls.conversation,
            accepted_messages: controls
                .accepted_messages
                .into_iter()
                .map(|(_, message)| message)
                .collect(),
            queued_messages,
            queued_message_positions,
            completed_turns: head.control.completed_turns,
            next_turn: head.control.next_turn,
            last_sequence: head.next_sequence.checked_sub(1).map(SequenceId),
            interrupted_turn: head.control.active.as_ref().map(|active| active.turn),
            driver_client_id: head.control.driver,
            interrupted_tool_repairs,
            interrupted_tool_turn,
            interrupted_assistant_turn,
            pending_questions: controls
                .pending_questions
                .into_iter()
                .map(|question| (question.question_id.0.clone(), question))
                .collect(),
            accounting: head.accounting,
            latest_budget: controls.latest_budget,
            budgeter: Budgeter::from_snapshot(head.budget)
                .map_err(|_| AgentLoopError::Persistence("canonical budget metadata".into()))?,
            interrupted_compaction,
            model_alias,
            provider,
            thinking,
            mode: head.control.mode,
            mode_id: head.control.mode_id,
            permission_mode: controls.permission_mode,
            pending_plan: controls.pending_plan,
            approved_plan: controls.approved_plan,
            plan_gate_active: head.control.plan_gate_active,
            active_shell: controls.active_shell,
            workspace_generation: head.control.workspace_generation,
            workspace_roots: controls.workspace_roots,
            source: Some(source),
        })
    }
}
