use super::RecoveryError;
use rw_types::{ClientId, ContextItemId, ModeId, SequenceId, SessionId, SessionMode};
use serde::{Deserialize, Serialize};

pub(super) const CONVERSATION: u8 = 1;
pub(super) const BOUNDARIES: u8 = 2;
pub(super) const CONTEXT_ACTIONS: u8 = 3;
pub(super) const PRUNED_OUTPUTS: u8 = 4;
pub(super) const ACCOUNTING: u8 = 5;
pub(super) const ACTIVE_ASSISTANT: u8 = 6;
pub(super) const ACTIVE_TOOL_LIFECYCLE: u8 = 7;
pub(super) const ACTIVE_TOOL_RESULTS: u8 = 8;
pub(super) const SOURCE_ORDINAL: u8 = 12;
pub(super) const PROMPTS: u8 = 15;
pub(super) const MAX_QUEUED: usize = rw_types::session_state::MAX_SESSION_QUEUE_ITEMS;
pub(super) const MAX_QUESTIONS: usize = rw_types::question_admission::MAX_PENDING_QUESTION_REQUESTS;

/// Bounded actor metadata; historical bodies remain source-addressed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConversationMetadata {
    pub turns: u64,
    pub system_turns: u64,
    pub resolved_model: Option<String>,
    pub system_resolved_model: Option<String>,
    pub title_prompt: Option<String>,
    pub has_assistant_text: bool,
    pub approved_plan_item: Option<ContextItemId>,
}

/// Exact visible canonical conversation. Bodies remain in the authoritative journal.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConversationCut {
    pub first_user_source: Option<SequenceId>,
    pub system_model_source: Option<SequenceId>,
    pub system_turns: u64,
    pub has_assistant_text: bool,
    pub approved_plan_ordinal: Option<u64>,
    pub resolved_model_source: Option<SequenceId>,
    pub generation: u64,
    pub turns: u64,
    pub serialized_bytes: u64,
    pub decoded_bytes: u64,
    pub estimated_tokens: u64,
}

/// How to derive a canonical IR turn from its authoritative event.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TurnSourceKind {
    Committed,
    Shell,
}

/// Admission metadata and exact source selector for one canonical conversation turn.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConversationSource {
    pub has_resolved_model: bool,
    pub sequence: SequenceId,
    pub kind: TurnSourceKind,
    pub agent_turn: u64,
    pub role: rw_types::Role,
    pub serialized_bytes: u64,
    pub decoded_bytes: u64,
    pub estimated_tokens: u64,
    pub cumulative_bytes: u64,
    pub cumulative_decoded_bytes: u64,
    pub cumulative_tokens: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedSource {
    pub position: u64,
    pub sequence: SequenceId,
    pub content_digest: [u8; 32],
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QuestionSource {
    pub id: String,
    pub agent_turn: u64,
    pub sequence: SequenceId,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcceptedSource {
    pub agent_turn: u64,
    pub sequence: SequenceId,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActiveTurn {
    pub announced_citations: rw_types::citation_admission::CitationAdmission,
    pub committed_citations: rw_types::citation_admission::CitationAdmission,
    pub turn: u64,
    pub started: SequenceId,
    pub first_conversation_ordinal: u64,
    pub last_assistant_commit: Option<SequenceId>,
    pub last_tool_commit: Option<SequenceId>,
    pub assistant_parts: SourceTotals,
    pub tool_lifecycle: SourceTotals,
    pub tool_results: SourceTotals,
}

impl ActiveTurn {
    pub(super) fn replace_conversation(&mut self, sequence: SequenceId) {
        self.first_conversation_ordinal = 0;
        self.last_assistant_commit = Some(sequence);
        self.last_tool_commit = Some(sequence);
        self.assistant_parts = SourceTotals::default();
        self.tool_results = SourceTotals::default();
    }
}

/// Admission totals for source-backed active work, independent of historical output.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceTotals {
    pub records: u64,
    pub serialized_bytes: u64,
    pub decoded_bytes: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct ActiveSource {
    pub sequence: SequenceId,
    pub serialized_bytes: u64,
    pub decoded_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolStartIdentity {
    pub invocation_id: rw_types::ToolInvocationId,
    pub tool_call_id: rw_types::ToolCallId,
    pub index: usize,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum ToolLifecycleSource {
    Started(ToolStartIdentity),
    Finished(rw_types::ToolInvocationId),
}

/// Bounded live control state. Large client/provider payloads are source selectors.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecoveryControl {
    pub next_turn: u64,
    pub completed_turns: u64,
    pub active: Option<ActiveTurn>,
    pub driver: Option<ClientId>,
    pub title: Option<SequenceId>,
    pub todos: Option<SequenceId>,
    pub model: Option<SequenceId>,
    pub mode: SessionMode,
    pub mode_id: Option<ModeId>,
    pub permission_mode: Option<SequenceId>,
    pub pending_plan: Option<SequenceId>,
    pub approved_plan: Option<SequenceId>,
    pub plan_gate_active: bool,
    pub active_shell: Option<(String, SequenceId)>,
    pub workspace: Option<SequenceId>,
    pub workspace_generation: u64,
    pub workspace_root_count: usize,
    pub workspace_digest: [u8; 32],
    pub queued: Vec<QueuedSource>,
    pub accepted: Vec<AcceptedSource>,
    pub questions: Vec<QuestionSource>,
}
impl Default for RecoveryControl {
    fn default() -> Self {
        Self {
            next_turn: 1,
            completed_turns: 0,
            active: None,
            driver: None,
            title: None,
            todos: None,
            model: None,
            mode: SessionMode::Execute,
            mode_id: None,
            permission_mode: None,
            pending_plan: None,
            approved_plan: None,
            plan_gate_active: false,
            active_shell: None,
            workspace: None,
            workspace_generation: 0,
            workspace_root_count: 0,
            workspace_digest: *blake3::hash(b"").as_bytes(),
            queued: Vec::new(),
            accepted: Vec::new(),
            questions: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Boundary {
    pub source_sequence: SequenceId,
    pub conversation: ConversationCut,
    pub control: RecoveryControl,
    pub context_cut: u64,
    pub budget: rw_context::BudgetSnapshot,
    pub(super) extension_root: Option<SequenceId>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum RewindPhase {
    Boundaries,
    Prompts,
    Context,
    Prunes,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum Maintenance {
    Rewind {
        sequence: SequenceId,
        target: u64,
        phase: RewindPhase,
    },
    Clear {
        sequence: SequenceId,
        from: ConversationCut,
        after: Option<u64>,
        to: ConversationCut,
    },
}

/// Bounded recovery checkpoint. Neither lifetime raw events nor historical bodies are retained.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecoveryHead {
    pub session_id: Option<SessionId>,
    pub next_sequence: u64,
    pub registry_fingerprint: [u8; 32],
    pub inherited_journal_through: Option<SequenceId>,
    pub conversation: ConversationCut,
    pub control: RecoveryControl,
    pub budget: rw_context::BudgetSnapshot,
    pub accounting: crate::engine::SessionAccountingState,
    pub latest_budget: Option<SequenceId>,
    pub plugin_statuses: std::collections::BTreeMap<String, SequenceId>,
    pub(super) extension_root: Option<SequenceId>,
    pub(super) compacting: Option<ConversationCut>,
    pub(super) context_cut: u64,
    pub(super) maintenance: Option<Maintenance>,
}
impl RecoveryHead {
    /// An interrupted summary generation has not replaced visible conversation.
    #[must_use]
    pub const fn interrupted_compaction(&self) -> bool {
        self.compacting.is_some()
    }

    pub(super) fn new(
        fingerprint: [u8; 32],
        inherited_journal_through: Option<SequenceId>,
    ) -> Self {
        Self {
            session_id: None,
            next_sequence: 0,
            registry_fingerprint: fingerprint,
            inherited_journal_through,
            extension_root: None,
            conversation: ConversationCut::default(),
            control: RecoveryControl::default(),
            budget: rw_context::Budgeter::default().snapshot(),
            accounting: crate::engine::SessionAccountingState::default(),
            latest_budget: None,
            plugin_statuses: std::collections::BTreeMap::default(),
            compacting: None,
            context_cut: 0,
            maintenance: None,
        }
    }
    pub(super) fn validate(&self) -> Result<(), RecoveryError> {
        if self.control.queued.len() > MAX_QUEUED
            || self.control.accepted.len() > MAX_QUEUED
            || self.control.questions.len() > MAX_QUESTIONS
        {
            return Err(RecoveryError::Limit("active queue/question identities"));
        }
        if self.plugin_statuses.len() > rw_types::session_state::MAX_SESSION_PLUGIN_STATUSES {
            return Err(RecoveryError::Limit("active plugin statuses"));
        }
        for (id, source) in &self.plugin_statuses {
            rw_types::session_state::validate_plugin_status(id, "")
                .map_err(RecoveryError::Invalid)?;
            if source.0 >= self.next_sequence {
                return Err(RecoveryError::Invalid("plugin status source"));
            }
        }
        rw_context::Budgeter::from_snapshot(self.budget)
            .map_err(|_| RecoveryError::Invalid("budget reconciliation"))?;
        if self.control.next_turn == 0 {
            return Err(RecoveryError::Invalid("control counters"));
        }
        Ok(())
    }
}

impl rw_types::allocation::DecodeAllocation for ToolLifecycleSource {
    fn decode_node_bytes() -> Option<usize> {
        Some(
            std::mem::size_of::<Self>()
                .max(std::mem::size_of::<ToolStartIdentity>())
                .max(<rw_types::ToolInvocationId as rw_types::allocation::DecodeAllocation>::decode_node_bytes()?)
                .max(<rw_types::ToolCallId as rw_types::allocation::DecodeAllocation>::decode_node_bytes()?)
                .max(<usize as rw_types::allocation::DecodeAllocation>::decode_node_bytes()?),
        )
    }
}
